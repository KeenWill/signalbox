//! Bounded Open XML interpretation inside the supervised file-media worker.

use std::{
    collections::{HashMap, HashSet},
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
    MAX_TEXT_BODY_BYTES, ProbeDeclaration, ProbeDeclarationInput, ProbeStrength, ProcessorFailure,
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
// numeric-bound: not-a-bound - fixed ZIP signature width required by the format
const ZIP_PREFIX_BYTES: u64 = 4;
// numeric-bound: not-a-bound - fixed maximum ZIP comment plus trailing record coverage
const ZIP_SUFFIX_BYTES: u64 = 65_536;
// numeric-bound: ceiling - bounds ZIP64 extensible end-record data inspected during probing
const MAX_ZIP64_EOCD_BYTES: u64 = 64 * 1024;
// numeric-bound: not-a-bound - maximum EOCD reach, locator width, and bounded ZIP64 record
const EOCD_PRECEDING_BYTES: u64 = 21 + 20 + MAX_ZIP64_EOCD_BYTES;
// numeric-bound: not-a-bound - maximum selected main-part name width across supported families
const SELECTED_PART_NAME_BYTES: u64 = 20;
// numeric-bound: not-a-bound - fixed number of supported Office families selected by one probe
const MAX_SELECTED_PARTS: u64 = 3;
// numeric-bound: ceiling - protects probe broker memory and cumulative source reads
const VALIDATION_SOURCE_BYTES: u64 = 262_144;
// numeric-bound: ceiling - protects probe decompression from oversized type metadata
const CONTENT_TYPES_COMPRESSED_BYTES: u64 = 64 * 1024;
// numeric-bound: ceiling - protects probe decompression from oversized relationship metadata
const PACKAGE_RELS_COMPRESSED_BYTES: u64 = 8 * 1024;
// numeric-bound: not-a-bound - fixed ZIP local-file-header width required by the format
const LOCAL_HEADER_BYTES: u64 = 30;
// numeric-bound: not-a-bound - fixed byte length of the canonical content-types part name
const CONTENT_TYPES_NAME_BYTES: u64 = 19;
// numeric-bound: not-a-bound - fixed byte length of the canonical package-relationships name
const PACKAGE_RELS_NAME_BYTES: u64 = 11;
// numeric-bound: ceiling - protects probe reads from adversarial local extra fields
const LOCAL_EXTRA_BYTES: u64 = 256;
// numeric-bound: ceiling - protects worker memory while reading a complete Office source
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
// numeric-bound: tunable - controls bounded source-read granularity
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
// numeric-bound: ceiling - protects decoder memory from one expanded Office part
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
// numeric-bound: ceiling - protects worker memory and CPU across all expanded Office parts
const MAX_TOTAL_EXPANDED_BYTES: u64 = 16 * 1024 * 1024;
// numeric-bound: ceiling - protects XML parsing from adversarial nesting and scope cloning
const MAX_XML_DEPTH: usize = 256;
// numeric-bound: ceiling - bounds one namespace prefix so cloned scopes stay small
const MAX_NAMESPACE_PREFIX_BYTES: usize = 64;
// numeric-bound: ceiling - bounds one namespace name so cloned scopes stay small
const MAX_NAMESPACE_URI_BYTES: usize = 1024;
// numeric-bound: ceiling - bounds live declarations per scope so per-element clones cannot amplify
const MAX_NAMESPACE_DECLARATIONS: usize = 128;
// numeric-bound: ceiling - bounds cumulative markup-compatibility state per scope so per-element clones cannot amplify
const MAX_MARKUP_COMPATIBILITY_ENTRIES: usize = 128;
// numeric-bound: ceiling - protects worker framing from oversized metadata output
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const CONTENT_TYPES: &str = "[Content_Types].xml";
const PACKAGE_RELS: &str = "_rels/.rels";
const CONTENT_TYPES_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/content-types";
const PACKAGE_RELATIONSHIPS_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/relationships";
const OFFICE_RELATIONSHIPS_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const WORDPROCESSINGML_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const SPREADSHEETML_NAMESPACE: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const PRESENTATIONML_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/presentationml/2006/main";
const DRAWINGML_NAMESPACE: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const MARKUP_COMPATIBILITY_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/markup-compatibility/2006";
const STRICT_OFFICE_RELATIONSHIPS_NAMESPACE: &[u8] =
    b"http://purl.oclc.org/ooxml/officeDocument/relationships";
const STRICT_WORDPROCESSINGML_NAMESPACE: &[u8] =
    b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const STRICT_SPREADSHEETML_NAMESPACE: &[u8] = b"http://purl.oclc.org/ooxml/spreadsheetml/main";
const STRICT_PRESENTATIONML_NAMESPACE: &[u8] = b"http://purl.oclc.org/ooxml/presentationml/main";
const STRICT_DRAWINGML_NAMESPACE: &[u8] = b"http://purl.oclc.org/ooxml/drawingml/main";
const DOCX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const XLSX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const PPTX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
const VBA_PROJECT_CONTENT_TYPE: &str = "application/vnd.ms-office.vbaProject";

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
                    evidence_bytes: inventory.examined_bytes,
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
                return Ok(ProcessorValidationOutput::NoMatch);
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
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: ZIP_PREFIX_BYTES,
            suffix_bytes: ZIP_SUFFIX_BYTES,
            range_count: 16,
            cumulative_bytes: VALIDATION_SOURCE_BYTES,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            VALIDATION_SOURCE_BYTES,
            signalbox_file_media_runtime::MAX_VALIDATION_RANGES,
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
    /// Actual cumulative source bytes read while building this inventory.
    examined_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
enum ValidationIssue {
    Unrecognized,
    Malformed(&'static str),
}

impl ValidationIssue {
    const fn reason(self) -> &'static str {
        match self {
            Self::Unrecognized => MALFORMED_REASON,
            Self::Malformed(reason) => reason,
        }
    }

    fn validation(self, kind: OfficeKind) -> ProcessorValidationOutput {
        match self {
            Self::Unrecognized => ProcessorValidationOutput::NoMatch,
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

#[derive(Default)]
struct DecodedBudget {
    bytes: u64,
}

impl DecodedBudget {
    fn include(&mut self, bytes: u64) -> Result<(), ReadIssue> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT))?;
        if self.bytes > MAX_TOTAL_EXPANDED_BYTES {
            return Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT));
        }
        Ok(())
    }
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
    let mut examined: u64 = 0;
    let source_length = source.byte_length().get();
    let prefix_length = source_length.min(ZIP_PREFIX_BYTES);
    let prefix = read_range(source, 0, prefix_length).await?;
    examined = examined.saturating_add(prefix_length);
    if !prefix.starts_with(b"PK\x03\x04") {
        return Err(ValidationIssue::Unrecognized.into());
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
    examined = examined.saturating_add(preceding_length);
    suffix.extend(read_range(source, suffix_offset, suffix_length).await?);
    examined = examined.saturating_add(suffix_length);
    let eocd_relative = find_consistent_eocd(&suffix, suffix_start)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let eocd = &suffix[eocd_relative..];
    let eocd_length = 22_usize
        .checked_add(usize::from(le_u16(eocd, 20)?))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if eocd.len() != eocd_length {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let (entries, central_size, central_offset) =
        central_directory_fields(&suffix, eocd_relative, suffix_start)?;
    if entries > usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES).unwrap_or(usize::MAX) {
        return Err(ValidationIssue::Malformed(ENTRY_COUNT_LIMIT).into());
    }
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
                - LOCAL_EXTRA_BYTES
                - CONTENT_TYPES_COMPRESSED_BYTES
                - LOCAL_HEADER_BYTES
                - PACKAGE_RELS_NAME_BYTES
                - LOCAL_EXTRA_BYTES
                - PACKAGE_RELS_COMPRESSED_BYTES
                - MAX_SELECTED_PARTS
                    * (LOCAL_HEADER_BYTES + SELECTED_PART_NAME_BYTES + LOCAL_EXTRA_BYTES)
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    if entries == 0 || central_size == 0 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let central = read_range(source, central_offset, central_size).await?;
    examined = examined.saturating_add(central_size);
    require_active(cancellation)?;
    let mut inventory = parse_central_directory(&central, entries).map_err(|error| {
        CentralReadError::Validation {
            issue: error.issue,
            kinds: error.kinds,
        }
    })?;
    let recognized = marker_kinds(&inventory.entries_by_name);
    if inventory.encrypted {
        let content_types = inventory
            .entries_by_name
            .iter()
            .find(|entry| entry.name == CONTENT_TYPES);
        let package_relationships = inventory
            .entries_by_name
            .iter()
            .find(|entry| entry.name == PACKAGE_RELS);
        let (Some(content_types), Some(package_relationships)) =
            (content_types, package_relationships)
        else {
            return Err(CentralReadError::Validation {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                kinds: recognized,
            });
        };
        if recognized.is_empty() {
            return Err(CentralReadError::Validation {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                kinds: recognized,
            });
        }
        if content_types.flags & 1 != 0 || package_relationships.flags & 1 != 0 {
            inventory.kinds = recognized;
            validate_selected_probe_entries(source, cancellation, &inventory, &mut examined)
                .await?;
            inventory.examined_bytes = examined;
            return Ok(inventory);
        }
    }
    let content_types = match read_probe_entry(
        source,
        cancellation,
        &inventory,
        CONTENT_TYPES,
        CONTENT_TYPES_COMPRESSED_BYTES,
        &mut examined,
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
    let package_relationships = match read_probe_entry(
        source,
        cancellation,
        &inventory,
        PACKAGE_RELS,
        PACKAGE_RELS_COMPRESSED_BYTES,
        &mut examined,
    )
    .await
    {
        Ok(package_relationships) => package_relationships,
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
    inventory.kinds = validate_package_relationships(&package_relationships, &content_type_kinds)
        .map_err(|issue| CentralReadError::Validation {
        issue,
        kinds: recognized,
    })?;
    validate_selected_probe_entries(source, cancellation, &inventory, &mut examined).await?;
    inventory.examined_bytes = examined;
    Ok(inventory)
}

async fn validate_selected_probe_entries(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    inventory: &CentralInventory,
    examined: &mut u64,
) -> Result<(), CentralReadError> {
    for kind in &inventory.kinds {
        let entry = inventory
            .entries_by_name
            .iter()
            .find(|entry| entry.name == kind.marker())
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        validate_selected_probe_entry(entry)?;
        let data_offset = read_probe_local_header(source, cancellation, entry, examined).await?;
        require_range(
            source.byte_length().get(),
            data_offset,
            entry.compressed_bytes,
        )?;
    }
    Ok(())
}

fn validate_selected_probe_entry(entry: &CentralEntry) -> Result<(), CentralReadError> {
    if entry.flags & 1 == 0 {
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
    let mut parsed_entries = Vec::with_capacity(expected_entries);
    let mut recognized = Vec::new();
    while offset < bytes.len() {
        let parsed = parse_central_entry(bytes, offset).map_err(|issue| CentralParseError {
            issue,
            kinds: recognized.clone(),
        })?;
        if let Some(kind) = [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx]
            .into_iter()
            .find(|kind| parsed.entry.name == kind.marker())
            && !recognized.contains(&kind)
        {
            recognized.push(kind);
        }
        offset = parsed.next_offset;
        parsed_entries.push(parsed);
    }
    if parsed_entries.len() != expected_entries {
        return Err(CentralParseError {
            issue: ValidationIssue::Malformed(MALFORMED_REASON),
            kinds: recognized,
        });
    }
    let mut entries_by_name = Vec::with_capacity(expected_entries);
    let mut names = HashSet::with_capacity(expected_entries);
    let mut expanded_bytes = 0_u64;
    let mut decoded_bytes = 0_u64;
    let mut encrypted = false;
    for parsed in parsed_entries {
        let entry = parsed.entry;
        if !names.insert(entry.name.clone()) {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                kinds: recognized.clone(),
            });
        }
        validate_entry_name(&entry.name).map_err(|issue| CentralParseError {
            issue,
            kinds: recognized.clone(),
        })?;
        if parsed.creator_system == 3
            && ((parsed.external_attributes >> 16) & 0o170_000) == 0o120_000
        {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(SYMLINK_ENTRY),
                kinds: recognized.clone(),
            });
        }
        if recursive_container_name(&entry.name) {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(RECURSIVE_CONTAINER),
                kinds: recognized.clone(),
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
                    kinds: recognized.clone(),
                });
            }
            if entry.expanded_bytes > MAX_ENTRY_BYTES {
                return Err(CentralParseError {
                    issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                    kinds: recognized.clone(),
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
                    kinds: recognized.clone(),
                });
            }
        }
        entries_by_name.push(entry);
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
        examined_bytes: 0,
    })
}

struct ParsedCentralEntry {
    entry: CentralEntry,
    flags: u16,
    creator_system: u8,
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
    let disk_start_16 = le_u16(fixed, 34)?;
    let (expanded_bytes, compressed_bytes, local_offset, disk_start) = decode_zip64_entry_fields(
        extra,
        expanded_32,
        compressed_32,
        local_offset_32,
        disk_start_16,
    )?;
    if disk_start != 0 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
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
        creator_system: *fixed
            .get(5)
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?,
        external_attributes: le_u32(fixed, 38)?,
        next_offset,
    })
}

fn decode_zip64_entry_fields(
    extra: &[u8],
    expanded_32: u32,
    compressed_32: u32,
    local_offset_32: u32,
    disk_start_16: u16,
) -> Result<(u64, u64, u64, u32), ValidationIssue> {
    let needs_zip64 = expanded_32 == u32::MAX
        || compressed_32 == u32::MAX
        || local_offset_32 == u32::MAX
        || disk_start_16 == u16::MAX;
    if !needs_zip64 {
        return Ok((
            u64::from(expanded_32),
            u64::from(compressed_32),
            u64::from(local_offset_32),
            u32::from(disk_start_16),
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
            let disk_start = if disk_start_16 == u16::MAX {
                let value = body
                    .get(zip64_offset..zip64_offset.saturating_add(4))
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
                u32::from_le_bytes([value[0], value[1], value[2], value[3]])
            } else {
                u32::from(disk_start_16)
            };
            return Ok((expanded, compressed, local_offset, disk_start));
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
    examined: &mut u64,
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
    let data_offset = read_probe_local_header(source, cancellation, entry, examined).await?;
    require_range(
        source.byte_length().get(),
        data_offset,
        entry.compressed_bytes,
    )?;
    let compressed = read_range(source, data_offset, entry.compressed_bytes).await?;
    *examined = examined.saturating_add(entry.compressed_bytes);
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

async fn read_probe_local_header(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    entry: &CentralEntry,
    examined: &mut u64,
) -> Result<u64, CentralReadError> {
    require_active(cancellation)?;
    require_range(
        source.byte_length().get(),
        entry.local_offset,
        LOCAL_HEADER_BYTES,
    )?;
    let local = read_range(source, entry.local_offset, LOCAL_HEADER_BYTES).await?;
    *examined = examined.saturating_add(LOCAL_HEADER_BYTES);
    if local.get(0..4) != Some(b"PK\x03\x04")
        || le_u16(&local, 6)? != entry.flags
        || le_u16(&local, 8)? != entry.compression
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let name_length = u64::from(le_u16(&local, 26)?);
    let extra_length = u64::from(le_u16(&local, 28)?);
    if name_length != u64::try_from(entry.name.len()).unwrap_or(u64::MAX)
        || extra_length > LOCAL_EXTRA_BYTES
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let local_name_offset = entry
        .local_offset
        .checked_add(LOCAL_HEADER_BYTES)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let local_variable_length = name_length
        .checked_add(extra_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    require_range(
        source.byte_length().get(),
        local_name_offset,
        local_variable_length,
    )?;
    let local_variable = read_range(source, local_name_offset, local_variable_length).await?;
    *examined = examined.saturating_add(local_variable_length);
    let local_name_length =
        usize::try_from(name_length).map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    let local_name = local_variable
        .get(..local_name_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let local_extra = local_variable
        .get(local_name_length..)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if local_name != entry.name.as_bytes() {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    validate_local_header_fields(&local, local_extra, entry)?;
    entry
        .local_offset
        .checked_add(LOCAL_HEADER_BYTES)
        .and_then(|value| value.checked_add(local_variable_length))
        .ok_or_else(|| ValidationIssue::Malformed(MALFORMED_REASON).into())
}

fn validate_local_header_fields(
    local: &[u8],
    local_extra: &[u8],
    entry: &CentralEntry,
) -> Result<(), ValidationIssue> {
    const DATA_DESCRIPTOR_FLAG: u16 = 1 << 3;
    if entry.flags & DATA_DESCRIPTOR_FLAG != 0 {
        return Ok(());
    }
    let expanded_32 = le_u32(local, 22)?;
    let compressed_32 = le_u32(local, 18)?;
    let (expanded, compressed, _, _) =
        decode_zip64_entry_fields(local_extra, expanded_32, compressed_32, 0, 0)?;
    if le_u32(local, 14)? != entry.crc32
        || compressed != entry.compressed_bytes
        || expanded != entry.expanded_bytes
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    Ok(())
}

fn validate_package_relationships(
    bytes: &[u8],
    content_type_kinds: &[OfficeKind],
) -> Result<Vec<OfficeKind>, ValidationIssue> {
    let transcoded =
        transcode_xml(bytes).map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = Vec::new();
    let mut relationship_ids = HashSet::new();
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut relationships_scope = std::collections::HashMap::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?
        {
            Event::Start(element) => {
                if depth == 0 {
                    if saw_root || local_name(element.name().as_ref()) != b"Relationships" {
                        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                    }
                    apply_namespace_declarations(&reader, &element, &mut relationships_scope)
                        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    if !element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &relationships_scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    ) {
                        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                    }
                    saw_root = true;
                } else if depth == 1 && local_name(element.name().as_ref()) == b"Relationship" {
                    let mut scope = relationships_scope.clone();
                    apply_namespace_declarations(&reader, &element, &mut scope)
                        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    if element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    ) {
                        collect_package_relationship(
                            &reader,
                            &element,
                            &mut relationship_ids,
                            &mut targets,
                        )?;
                    }
                }
                depth = depth
                    .checked_add(1)
                    .filter(|next| *next <= MAX_XML_DEPTH)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Empty(element)
                if depth == 1 && local_name(element.name().as_ref()) == b"Relationship" =>
            {
                let mut scope = relationships_scope.clone();
                apply_namespace_declarations(&reader, &element, &mut scope)
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if element_uses_scoped_namespace(
                    element.name().as_ref(),
                    &scope,
                    PACKAGE_RELATIONSHIPS_NAMESPACE,
                ) {
                    collect_package_relationship(
                        &reader,
                        &element,
                        &mut relationship_ids,
                        &mut targets,
                    )?;
                }
            }
            Event::Empty(element) if depth == 0 => {
                if saw_root || local_name(element.name().as_ref()) != b"Relationships" {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                apply_namespace_declarations(&reader, &element, &mut relationships_scope)
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if !element_uses_scoped_namespace(
                    element.name().as_ref(),
                    &relationships_scope,
                    PACKAGE_RELATIONSHIPS_NAMESPACE,
                ) {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                saw_root = true;
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Text(text) => {
                let decoded = text
                    .xml10_content()
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if !decoded.trim().is_empty() {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
            }
            Event::CData(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(ValidationIssue::Malformed(MALFORMED_REASON));
            }
            Event::Eof if saw_root && depth == 0 => break,
            Event::Eof => return Err(ValidationIssue::Malformed(MALFORMED_REASON)),
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
    let target = normalized_package_target(&target)?;
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

fn normalized_package_target(target: &str) -> Result<String, ValidationIssue> {
    if target.contains('\\')
        || target.contains('?')
        || target.contains('#')
        || target.starts_with("//")
        || target
            .split('/')
            .next()
            .is_some_and(|first| first.contains(':'))
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    let path = target.strip_prefix('/').unwrap_or(target);
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" => return Err(ValidationIssue::Malformed(MALFORMED_REASON)),
            "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
            }
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    Ok(segments.join("/"))
}

fn collect_package_relationship(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    relationship_ids: &mut HashSet<String>,
    targets: &mut Vec<String>,
) -> Result<(), ValidationIssue> {
    let mut id = None;
    let mut relationship_type = None;
    let mut target = None;
    let mut external = false;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        match attribute.key.as_ref() {
            b"Id" => id = Some(value.into_owned()),
            b"Type" => relationship_type = Some(value.into_owned()),
            b"Target" => target = Some(value.into_owned()),
            b"TargetMode" => external = value.eq_ignore_ascii_case("external"),
            _ => {}
        }
    }
    let id = id.ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let relationship_type =
        relationship_type.ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let target = target.ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if !relationship_ids.insert(id) {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    if relationship_type_matches(&relationship_type, "/officeDocument") {
        if external {
            return Err(ValidationIssue::Malformed(MALFORMED_REASON));
        }
        targets.push(target);
    }
    Ok(())
}

fn validate_content_types(
    bytes: &[u8],
    entries: &[CentralEntry],
) -> Result<Vec<OfficeKind>, ValidationIssue> {
    let transcoded =
        transcode_xml(bytes).map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut content_types_scope = std::collections::HashMap::new();
    let mut kinds = Vec::new();
    let mut part_names = HashSet::new();
    let mut default_types = std::collections::HashMap::new();
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
                    apply_namespace_declarations(&reader, &start, &mut content_types_scope)
                        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    if !element_uses_scoped_namespace(
                        start.name().as_ref(),
                        &content_types_scope,
                        CONTENT_TYPES_NAMESPACE,
                    ) {
                        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                    }
                    saw_root = true;
                } else if depth == 1 {
                    let mut scope = content_types_scope.clone();
                    apply_namespace_declarations(&reader, &start, &mut scope)
                        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    if element_uses_scoped_namespace(
                        start.name().as_ref(),
                        &scope,
                        CONTENT_TYPES_NAMESPACE,
                    ) {
                        match local_name(start.name().as_ref()) {
                            b"Override" => collect_content_type_kind(
                                &reader,
                                &start,
                                entries,
                                &mut kinds,
                                &mut part_names,
                            )?,
                            b"Default" => {
                                collect_default_content_type(&reader, &start, &mut default_types)?
                            }
                            _ => {}
                        }
                    }
                }
                depth = depth
                    .checked_add(1)
                    .filter(|next| *next <= MAX_XML_DEPTH)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Empty(empty) if depth == 1 => {
                let mut scope = content_types_scope.clone();
                apply_namespace_declarations(&reader, &empty, &mut scope)
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if element_uses_scoped_namespace(
                    empty.name().as_ref(),
                    &scope,
                    CONTENT_TYPES_NAMESPACE,
                ) {
                    match local_name(empty.name().as_ref()) {
                        b"Override" => collect_content_type_kind(
                            &reader,
                            &empty,
                            entries,
                            &mut kinds,
                            &mut part_names,
                        )?,
                        b"Default" => {
                            collect_default_content_type(&reader, &empty, &mut default_types)?
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(empty) if depth == 0 => {
                if saw_root || local_name(empty.name().as_ref()) != b"Types" {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                apply_namespace_declarations(&reader, &empty, &mut content_types_scope)
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if !element_uses_scoped_namespace(
                    empty.name().as_ref(),
                    &content_types_scope,
                    CONTENT_TYPES_NAMESPACE,
                ) {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                saw_root = true;
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Text(text) => {
                let decoded = text
                    .xml10_content()
                    .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                if !decoded.trim().is_empty() {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
            }
            Event::CData(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(ValidationIssue::Malformed(MALFORMED_REASON));
            }
            Event::Eof if saw_root && depth == 0 => break,
            Event::Eof => return Err(ValidationIssue::Malformed(MALFORMED_REASON)),
            _ => {}
        }
        buffer.clear();
    }
    for kind in [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx] {
        let expected_part = format!("/{}", kind.marker());
        let extension = kind
            .marker()
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        if !part_names.contains(&expected_part)
            && default_types
                .get(extension)
                .is_some_and(|value| value.eq_ignore_ascii_case(kind.main_content_type()))
            && entries.iter().any(|entry| entry.name == kind.marker())
        {
            kinds.push(kind);
        }
    }
    kinds.sort_by_key(|kind| kind.reader());
    kinds.dedup();
    if kinds.is_empty() {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    Ok(kinds)
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
    part_names: &mut HashSet<String>,
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
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    };
    if recursive_content_type(&content_type) {
        return Err(ValidationIssue::Malformed(RECURSIVE_CONTAINER));
    }
    if content_type.eq_ignore_ascii_case(VBA_PROJECT_CONTENT_TYPE) {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    if !part_names.insert(part_name.clone()) {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    for kind in [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx] {
        let expected_part = format!("/{}", kind.marker());
        if part_name == expected_part
            && content_type.eq_ignore_ascii_case(kind.main_content_type())
            && entries.iter().any(|entry| entry.name == kind.marker())
        {
            kinds.push(kind);
        }
    }
    Ok(())
}

fn collect_default_content_type(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    default_types: &mut std::collections::HashMap<String, String>,
) -> Result<(), ValidationIssue> {
    let mut extension = None;
    let mut content_type = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        match attribute.key.as_ref() {
            b"Extension" => extension = Some(value.into_owned().to_ascii_lowercase()),
            b"ContentType" => content_type = Some(value.into_owned()),
            _ => {}
        }
    }
    let (Some(extension), Some(content_type)) = (extension, content_type) else {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    };
    if recursive_content_type(&content_type) {
        return Err(ValidationIssue::Malformed(RECURSIVE_CONTAINER));
    }
    if content_type.eq_ignore_ascii_case(VBA_PROJECT_CONTENT_TYPE)
        || default_types.insert(extension, content_type).is_some()
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
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

fn recursive_content_type(content_type: &str) -> bool {
    [
        DOCX_MEDIA_TYPE,
        XLSX_MEDIA_TYPE,
        PPTX_MEDIA_TYPE,
        "application/zip",
    ]
    .into_iter()
    .any(|candidate| content_type.eq_ignore_ascii_case(candidate))
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
    let mut budget = DecodedBudget::default();
    let names = match kind {
        OfficeKind::Docx => vec![String::from(kind.marker())],
        OfficeKind::Xlsx => return read_spreadsheet_text(archive, cancellation, &mut budget),
        OfficeKind::Pptx => match presentation_slide_names(archive, &mut budget) {
            Ok(names) => names,
            Err(ReadIssue::Expansion(reason)) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(reason),
                });
            }
            Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
        },
    };
    let mut output = String::new();
    for name in names {
        require_active(cancellation)?;
        let bytes = match read_entry(archive, &name, &mut budget) {
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
    budget: &mut DecodedBudget,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let workbook = match read_entry(archive, "xl/workbook.xml", budget) {
        Ok(bytes) => bytes,
        Err(ReadIssue::Expansion(reason)) => {
            return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: String::from(reason),
            });
        }
        Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
    };
    let relationships = match read_entry(archive, "xl/_rels/workbook.xml.rels", budget) {
        Ok(bytes) => bytes,
        Err(ReadIssue::Expansion(reason)) => {
            return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: String::from(reason),
            });
        }
        Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
    };
    let relationship_ids =
        workbook_relationship_ids(&workbook).map_err(|_| ProcessorFailure::Failed)?;
    let targets = workbook_relationship_targets(&relationships)
        .map_err(|_| ProcessorFailure::Failed)?
        .into_iter()
        .collect::<HashMap<_, _>>();
    let shared_target =
        workbook_shared_strings_target(&relationships).map_err(|_| ProcessorFailure::Failed)?;
    let shared_strings = if let Some(shared_target) = shared_target {
        match read_entry(archive, &shared_target, budget) {
            Ok(bytes) => {
                spreadsheet_shared_strings(&bytes).map_err(|_| ProcessorFailure::Failed)?
            }
            Err(ReadIssue::Expansion(reason)) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(reason),
                });
            }
            Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
        }
    } else {
        Vec::new()
    };
    let mut output = String::new();
    for relationship_id in relationship_ids {
        require_active(cancellation)?;
        let target = targets
            .get(&relationship_id)
            .ok_or(ProcessorFailure::Failed)?;
        let Some(target) = target else {
            continue;
        };
        let worksheet = match read_entry(archive, target, budget) {
            Ok(bytes) => bytes,
            Err(ReadIssue::Expansion(reason)) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(reason),
                });
            }
            Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
        };
        let text = match spreadsheet_worksheet_text(&worksheet, &shared_strings) {
            Ok(text) => text,
            Err(XmlIssue::OutputTooLarge) => {
                return Ok(ProcessorReadOutput::OutputUnitTooLarge);
            }
            Err(XmlIssue::Malformed) => return Err(ProcessorFailure::Failed),
        };
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
    ordered_relationship_ids(
        bytes,
        b"workbook",
        SPREADSHEETML_NAMESPACE,
        b"sheets",
        b"sheet",
    )
}

fn ordered_relationship_ids(
    bytes: &[u8],
    root_name: &[u8],
    root_namespace: &[u8],
    list_name: &[u8],
    item_name: &[u8],
) -> Result<Vec<String>, XmlIssue> {
    validate_xml_root(bytes, root_name, root_namespace)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut list_depth = None;
    let mut ids = Vec::new();
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if local_name(element.name().as_ref()) == list_name
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        root_namespace,
                    )
                    && depth == 1
                {
                    if list_depth.is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    list_depth = Some(depth);
                } else if local_name(element.name().as_ref()) == item_name
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        root_namespace,
                    )
                    && list_depth == depth.checked_sub(1)
                {
                    record_ordered_relationship_id(
                        &mut ids,
                        required_relationship_id(&reader, &element, &scope)?,
                    )?;
                }
                namespace_scopes.push(scope);
                depth = next_xml_depth(depth)?;
            }
            Event::Empty(element)
                if local_name(element.name().as_ref()) == item_name
                    && list_depth == depth.checked_sub(1) =>
            {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if element_uses_scoped_namespace(element.name().as_ref(), &scope, root_namespace) {
                    record_ordered_relationship_id(
                        &mut ids,
                        required_relationship_id(&reader, &element, &scope)?,
                    )?;
                }
            }
            Event::End(element) => {
                let scope = namespace_scopes.last().ok_or(XmlIssue::Malformed)?;
                let qualified_name = element.name();
                let closes_list = local_name(qualified_name.as_ref()) == list_name
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        scope,
                        root_namespace,
                    )
                    && list_depth == depth.checked_sub(1);
                depth = depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
                if closes_list {
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

fn record_ordered_relationship_id(ids: &mut Vec<String>, id: String) -> Result<(), XmlIssue> {
    let limit = usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES).unwrap_or(usize::MAX);
    if ids.len() >= limit {
        return Err(XmlIssue::Malformed);
    }
    ids.push(id);
    Ok(())
}

fn apply_namespace_declarations(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    scope: &mut std::collections::HashMap<Vec<u8>, Vec<u8>>,
) -> Result<(), XmlIssue> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
        let key = attribute.key.as_ref();
        let prefix = if key == b"xmlns" {
            b"".as_slice()
        } else if let Some(prefix) = key.strip_prefix(b"xmlns:") {
            prefix
        } else {
            continue;
        };
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| XmlIssue::Malformed)?;
        if prefix.len() > MAX_NAMESPACE_PREFIX_BYTES || value.len() > MAX_NAMESPACE_URI_BYTES {
            return Err(XmlIssue::Malformed);
        }
        scope.insert(prefix.to_vec(), value.as_bytes().to_vec());
        if scope.len() > MAX_NAMESPACE_DECLARATIONS {
            return Err(XmlIssue::Malformed);
        }
    }
    Ok(())
}

fn next_xml_depth(depth: usize) -> Result<usize, XmlIssue> {
    let next = depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
    if next > MAX_XML_DEPTH {
        return Err(XmlIssue::Malformed);
    }
    Ok(next)
}

fn element_uses_scoped_namespace(
    name: &[u8],
    namespace_scope: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
    namespace: &[u8],
) -> bool {
    let prefix = name
        .iter()
        .position(|byte| *byte == b':')
        .map_or(b"".as_slice(), |separator| &name[..separator]);
    namespace_scope
        .get(prefix)
        .is_some_and(|actual| namespace_matches(actual, namespace))
}

#[derive(Clone, Default)]
struct MarkupCompatibilityScope {
    ignorable_namespaces: HashSet<Vec<u8>>,
    process_content: HashSet<(Vec<u8>, Vec<u8>)>,
}

fn apply_markup_compatibility_attributes(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    namespace_scope: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
    compatibility: &mut MarkupCompatibilityScope,
) -> Result<(), XmlIssue> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
        let name = attribute.key.as_ref();
        let Some(separator) = name.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let prefix = &name[..separator];
        if !namespace_scope
            .get(prefix)
            .is_some_and(|namespace| namespace == MARKUP_COMPATIBILITY_NAMESPACE)
        {
            continue;
        }
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| XmlIssue::Malformed)?;
        match &name[separator + 1..] {
            b"MustUnderstand" => {
                for declared_prefix in value.split_ascii_whitespace() {
                    let namespace = namespace_scope
                        .get(declared_prefix.as_bytes())
                        .ok_or(XmlIssue::Malformed)?;
                    if !supported_markup_namespace(namespace) {
                        return Err(XmlIssue::Malformed);
                    }
                }
            }
            b"Ignorable" => {
                for declared_prefix in value.split_ascii_whitespace() {
                    let namespace = namespace_scope
                        .get(declared_prefix.as_bytes())
                        .ok_or(XmlIssue::Malformed)?;
                    compatibility.ignorable_namespaces.insert(namespace.clone());
                    if compatibility.ignorable_namespaces.len()
                        + compatibility.process_content.len()
                        > MAX_MARKUP_COMPATIBILITY_ENTRIES
                    {
                        return Err(XmlIssue::Malformed);
                    }
                }
            }
            b"ProcessContent" => {
                for qualified_name in value.split_ascii_whitespace() {
                    let (declared_prefix, local) =
                        qualified_name.split_once(':').ok_or(XmlIssue::Malformed)?;
                    let namespace = namespace_scope
                        .get(declared_prefix.as_bytes())
                        .ok_or(XmlIssue::Malformed)?;
                    compatibility
                        .process_content
                        .insert((namespace.clone(), local.as_bytes().to_vec()));
                    if compatibility.ignorable_namespaces.len()
                        + compatibility.process_content.len()
                        > MAX_MARKUP_COMPATIBILITY_ENTRIES
                    {
                        return Err(XmlIssue::Malformed);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn supported_markup_namespace(namespace: &[u8]) -> bool {
    [
        OFFICE_RELATIONSHIPS_NAMESPACE,
        WORDPROCESSINGML_NAMESPACE,
        SPREADSHEETML_NAMESPACE,
        PRESENTATIONML_NAMESPACE,
        DRAWINGML_NAMESPACE,
        MARKUP_COMPATIBILITY_NAMESPACE,
        STRICT_OFFICE_RELATIONSHIPS_NAMESPACE,
        STRICT_WORDPROCESSINGML_NAMESPACE,
        STRICT_SPREADSHEETML_NAMESPACE,
        STRICT_PRESENTATIONML_NAMESPACE,
        STRICT_DRAWINGML_NAMESPACE,
    ]
    .into_iter()
    .any(|supported| namespace == supported)
}

fn markup_choice_is_supported(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    namespace_scope: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
) -> Result<bool, XmlIssue> {
    let mut requires = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
        if attribute.key.as_ref() == b"Requires" {
            requires = Some(
                attribute
                    .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                    .map_err(|_| XmlIssue::Malformed)?
                    .into_owned(),
            );
        }
    }
    let requires = requires.ok_or(XmlIssue::Malformed)?;
    let mut prefixes = requires.split_ascii_whitespace().peekable();
    if prefixes.peek().is_none() {
        return Err(XmlIssue::Malformed);
    }
    Ok(prefixes.all(|prefix| {
        namespace_scope
            .get(prefix.as_bytes())
            .is_some_and(|namespace| supported_markup_namespace(namespace))
    }))
}

fn ignores_markup_compatibility_element(
    name: &[u8],
    namespace_scope: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
    compatibility: &MarkupCompatibilityScope,
) -> bool {
    let prefix = name
        .iter()
        .position(|byte| *byte == b':')
        .map_or(b"".as_slice(), |separator| &name[..separator]);
    let Some(namespace) = namespace_scope.get(prefix) else {
        return false;
    };
    compatibility.ignorable_namespaces.contains(namespace)
        && !compatibility
            .process_content
            .contains(&(namespace.clone(), local_name(name).to_vec()))
}

fn required_relationship_id(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    namespace_scope: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
) -> Result<String, XmlIssue> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
        let name = attribute.key.as_ref();
        let Some(separator) = name.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let prefix = &name[..separator];
        let local = &name[separator + 1..];
        if local == b"id"
            && namespace_scope
                .get(prefix)
                .is_some_and(|actual| namespace_matches(actual, OFFICE_RELATIONSHIPS_NAMESPACE))
        {
            return Ok(attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .map_err(|_| XmlIssue::Malformed)?
                .into_owned());
        }
    }
    Err(XmlIssue::Malformed)
}

fn workbook_relationship_targets(bytes: &[u8]) -> Result<Vec<(String, Option<String>)>, XmlIssue> {
    validate_xml_root(bytes, b"Relationships", PACKAGE_RELATIONSHIPS_NAMESPACE)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = Vec::new();
    let mut relationship_ids = HashSet::new();
    let mut depth = 0_usize;
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if depth == 1
                    && local_name(element.name().as_ref()) == b"Relationship"
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    )
                {
                    collect_workbook_relationship(
                        &reader,
                        &element,
                        &mut relationship_ids,
                        &mut targets,
                    )?;
                }
                namespace_scopes.push(scope);
                depth = next_xml_depth(depth)?;
            }
            Event::Empty(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if depth == 1
                    && local_name(element.name().as_ref()) == b"Relationship"
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    )
                {
                    collect_workbook_relationship(
                        &reader,
                        &element,
                        &mut relationship_ids,
                        &mut targets,
                    )?;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
            }
            Event::Text(text) => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                if !decoded.trim().is_empty() {
                    return Err(XmlIssue::Malformed);
                }
            }
            Event::CData(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(XmlIssue::Malformed);
            }
            Event::Eof if depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(targets)
}

fn collect_workbook_relationship(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    relationship_ids: &mut HashSet<String>,
    targets: &mut Vec<(String, Option<String>)>,
) -> Result<(), XmlIssue> {
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
    let id = id.ok_or(XmlIssue::Malformed)?;
    record_relationship_id(relationship_ids, &id)?;
    let worksheet = relationship_type
        .as_deref()
        .is_some_and(|value| relationship_type_matches(value, "/worksheet"));
    if worksheet && external {
        return Err(XmlIssue::Malformed);
    }
    let target = if worksheet {
        Some(spreadsheet_target_name(
            &target.ok_or(XmlIssue::Malformed)?,
        )?)
    } else {
        None
    };
    targets.push((id, target));
    Ok(())
}

fn workbook_shared_strings_target(bytes: &[u8]) -> Result<Option<String>, XmlIssue> {
    let mut targets = relationship_targets(bytes, "/sharedStrings", |target| {
        normalized_part_target(target, "xl/")
    })?;
    if targets.len() > 1 {
        return Err(XmlIssue::Malformed);
    }
    Ok(targets.pop().map(|(_, target)| target))
}

fn relationship_targets(
    bytes: &[u8],
    relationship_suffix: &str,
    normalize: fn(&str) -> Result<String, XmlIssue>,
) -> Result<Vec<(String, String)>, XmlIssue> {
    validate_xml_root(bytes, b"Relationships", PACKAGE_RELATIONSHIPS_NAMESPACE)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = Vec::new();
    let mut relationship_ids = HashSet::new();
    let mut depth = 0_usize;
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if depth == 1
                    && local_name(element.name().as_ref()) == b"Relationship"
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    )
                {
                    collect_relationship_target(
                        &reader,
                        &element,
                        relationship_suffix,
                        normalize,
                        &mut relationship_ids,
                        &mut targets,
                    )?;
                }
                namespace_scopes.push(scope);
                depth = next_xml_depth(depth)?;
            }
            Event::Empty(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if depth == 1
                    && local_name(element.name().as_ref()) == b"Relationship"
                    && element_uses_scoped_namespace(
                        element.name().as_ref(),
                        &scope,
                        PACKAGE_RELATIONSHIPS_NAMESPACE,
                    )
                {
                    collect_relationship_target(
                        &reader,
                        &element,
                        relationship_suffix,
                        normalize,
                        &mut relationship_ids,
                        &mut targets,
                    )?;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
            }
            Event::Text(text) => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                if !decoded.trim().is_empty() {
                    return Err(XmlIssue::Malformed);
                }
            }
            Event::CData(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(XmlIssue::Malformed);
            }
            Event::Eof if depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(targets)
}

fn collect_relationship_target(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    relationship_suffix: &str,
    normalize: fn(&str) -> Result<String, XmlIssue>,
    relationship_ids: &mut HashSet<String>,
    targets: &mut Vec<(String, String)>,
) -> Result<(), XmlIssue> {
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
    let id = id.ok_or(XmlIssue::Malformed)?;
    record_relationship_id(relationship_ids, &id)?;
    if relationship_type
        .as_deref()
        .is_some_and(|value| relationship_type_matches(value, relationship_suffix))
    {
        if external {
            return Err(XmlIssue::Malformed);
        }
        targets.push((id, normalize(&target.ok_or(XmlIssue::Malformed)?)?));
    }
    Ok(())
}

fn record_relationship_id(ids: &mut HashSet<String>, id: &str) -> Result<(), XmlIssue> {
    let limit = usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES).unwrap_or(usize::MAX);
    if ids.len() >= limit || !ids.insert(String::from(id)) {
        return Err(XmlIssue::Malformed);
    }
    Ok(())
}

fn relationship_type_matches(value: &str, suffix: &str) -> bool {
    [
        std::str::from_utf8(OFFICE_RELATIONSHIPS_NAMESPACE).ok(),
        std::str::from_utf8(STRICT_OFFICE_RELATIONSHIPS_NAMESPACE).ok(),
    ]
    .into_iter()
    .flatten()
    .any(|namespace| value.strip_prefix(namespace) == Some(suffix))
}

fn spreadsheet_target_name(target: &str) -> Result<String, XmlIssue> {
    normalized_part_target(target, "xl/")
}

fn normalized_part_target(target: &str, base: &str) -> Result<String, XmlIssue> {
    if target.contains('\\') {
        return Err(XmlIssue::Malformed);
    }
    let unresolved = target
        .strip_prefix('/')
        .map_or_else(|| format!("{base}{target}"), String::from);
    normalized_package_target(&unresolved).map_err(|_| XmlIssue::Malformed)
}

fn spreadsheet_shared_strings(bytes: &[u8]) -> Result<Vec<String>, XmlIssue> {
    validate_xml_root(bytes, b"sst", SPREADSHEETML_NAMESPACE)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut strings = Vec::new();
    let mut current = None::<String>;
    let mut text_depth = 0_usize;
    let mut phonetic_depth = 0_usize;
    let mut element_depth = 0_usize;
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                element_depth = next_xml_depth(element_depth)?;
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                let spreadsheet_element = element_uses_scoped_namespace(
                    element.name().as_ref(),
                    &scope,
                    SPREADSHEETML_NAMESPACE,
                );
                if spreadsheet_element && local_name(element.name().as_ref()) == b"si" {
                    if current.replace(String::new()).is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                } else if current.is_some()
                    && spreadsheet_element
                    && local_name(element.name().as_ref()) == b"rPh"
                {
                    phonetic_depth = next_xml_depth(phonetic_depth)?;
                } else if current.is_some()
                    && phonetic_depth == 0
                    && spreadsheet_element
                    && local_name(element.name().as_ref()) == b"t"
                {
                    text_depth = next_xml_depth(text_depth)?;
                }
                namespace_scopes.push(scope);
            }
            Event::Empty(element) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                if element_uses_scoped_namespace(
                    element.name().as_ref(),
                    &scope,
                    SPREADSHEETML_NAMESPACE,
                ) && local_name(element.name().as_ref()) == b"si"
                {
                    if current.is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    strings.push(String::new());
                }
            }
            Event::End(element) => {
                element_depth = element_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                let scope = namespace_scopes.last().ok_or(XmlIssue::Malformed)?;
                let qualified_name = element.name();
                let spreadsheet_element = element_uses_scoped_namespace(
                    qualified_name.as_ref(),
                    scope,
                    SPREADSHEETML_NAMESPACE,
                );
                let name = local_name(qualified_name.as_ref());
                if spreadsheet_element && name == b"t" && text_depth > 0 {
                    text_depth -= 1;
                } else if spreadsheet_element && name == b"rPh" && phonetic_depth > 0 {
                    phonetic_depth -= 1;
                } else if spreadsheet_element && name == b"si" {
                    strings.push(current.take().ok_or(XmlIssue::Malformed)?);
                }
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
            }
            Event::Text(text) if text_depth > 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(current.as_mut().ok_or(XmlIssue::Malformed)?, &decoded)?;
            }
            Event::CData(text) if text_depth > 0 => {
                let decoded = text.decode().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(current.as_mut().ok_or(XmlIssue::Malformed)?, &decoded)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 => {
                let decoded = reference.decode().map_err(|_| XmlIssue::Malformed)?;
                let value = decode_xml_reference(&decoded)?;
                append_xml_text(current.as_mut().ok_or(XmlIssue::Malformed)?, &value)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof
                if current.is_none()
                    && text_depth == 0
                    && phonetic_depth == 0
                    && namespace_scopes.len() == 1 =>
            {
                break;
            }
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(strings)
}

fn spreadsheet_worksheet_text(bytes: &[u8], shared: &[String]) -> Result<String, XmlIssue> {
    validate_xml_root(bytes, b"worksheet", SPREADSHEETML_NAMESPACE)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut shared_cell = false;
    let mut inline_cell = false;
    let mut value_depth = 0_usize;
    let mut inline_text_depth = 0_usize;
    let mut inline_phonetic_depth = 0_usize;
    let mut element_depth = 0_usize;
    let mut value = String::new();
    let mut inline_value = String::new();
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                element_depth = next_xml_depth(element_depth)?;
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &element, &mut scope)?;
                let qualified_name = element.name();
                let name = local_name(qualified_name.as_ref());
                // Extension elements (e.g. `<ext:c>`) share local names with
                // real spreadsheet cells; require the SpreadsheetML namespace
                // so foreign markup cannot be read back as cell text.
                let spreadsheet_element = element_uses_scoped_namespace(
                    qualified_name.as_ref(),
                    &scope,
                    SPREADSHEETML_NAMESPACE,
                );
                if spreadsheet_element && name == b"c" {
                    shared_cell = false;
                    inline_cell = false;
                    value.clear();
                    inline_value.clear();
                    for attribute in element.attributes() {
                        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
                        if attribute.key.as_ref() == b"t" {
                            let cell_type = attribute
                                .decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .map_err(|_| XmlIssue::Malformed)?;
                            shared_cell = cell_type == "s";
                            inline_cell = cell_type == "inlineStr";
                        }
                    }
                } else if shared_cell && spreadsheet_element && name == b"v" {
                    value.clear();
                    value_depth = next_xml_depth(value_depth)?;
                } else if inline_cell && spreadsheet_element && name == b"rPh" {
                    inline_phonetic_depth = inline_phonetic_depth
                        .checked_add(1)
                        .ok_or(XmlIssue::Malformed)?;
                } else if inline_cell
                    && spreadsheet_element
                    && inline_phonetic_depth == 0
                    && name == b"t"
                {
                    inline_text_depth = inline_text_depth
                        .checked_add(1)
                        .ok_or(XmlIssue::Malformed)?;
                }
                namespace_scopes.push(scope);
            }
            Event::Text(text) if value_depth > 0 => {
                value.push_str(&text.xml10_content().map_err(|_| XmlIssue::Malformed)?);
            }
            Event::CData(text) if value_depth > 0 => {
                value.push_str(&text.decode().map_err(|_| XmlIssue::Malformed)?);
            }
            Event::GeneralRef(reference) if value_depth > 0 => {
                let decoded = reference.decode().map_err(|_| XmlIssue::Malformed)?;
                value.push_str(&decode_xml_reference(&decoded)?);
            }
            Event::Text(text) if inline_text_depth > 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(&mut inline_value, &decoded)?;
            }
            Event::CData(text) if inline_text_depth > 0 => {
                let decoded = text.decode().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(&mut inline_value, &decoded)?;
            }
            Event::GeneralRef(reference) if inline_text_depth > 0 => {
                let decoded = reference.decode().map_err(|_| XmlIssue::Malformed)?;
                let value = decode_xml_reference(&decoded)?;
                append_xml_text(&mut inline_value, &value)?;
            }
            Event::End(element) => {
                element_depth = element_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                // Namespace is not re-checked here: a well-formed document's
                // end tag always matches the namespace-gated start tag that
                // incremented these depths or set these flags, so the
                // Start-side check above is sufficient.
                let qualified_name = element.name();
                let name = local_name(qualified_name.as_ref());
                if name == b"v" && value_depth > 0 {
                    value_depth -= 1;
                } else if name == b"t" && inline_text_depth > 0 {
                    inline_text_depth -= 1;
                } else if name == b"rPh" && inline_phonetic_depth > 0 {
                    inline_phonetic_depth -= 1;
                } else if name == b"c" {
                    if shared_cell {
                        let index = value.parse::<usize>().map_err(|_| XmlIssue::Malformed)?;
                        append_xml_text(
                            &mut output,
                            shared.get(index).ok_or(XmlIssue::Malformed)?,
                        )?;
                        append_xml_text(&mut output, "\n")?;
                    } else if inline_cell {
                        append_xml_text(&mut output, &inline_value)?;
                        append_xml_text(&mut output, "\n")?;
                    }
                    shared_cell = false;
                    inline_cell = false;
                    value.clear();
                    inline_value.clear();
                }
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof
                if value_depth == 0
                    && inline_text_depth == 0
                    && inline_phonetic_depth == 0
                    && namespace_scopes.len() == 1 =>
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

fn presentation_slide_names<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    budget: &mut DecodedBudget,
) -> Result<Vec<String>, ReadIssue> {
    let presentation = read_entry(archive, "ppt/presentation.xml", budget)?;
    let relationships = read_entry(archive, "ppt/_rels/presentation.xml.rels", budget)?;
    let relationship_ids =
        presentation_relationship_ids(&presentation).map_err(|_| ReadIssue::Failed)?;
    let targets = presentation_relationship_targets(&relationships)
        .map_err(|_| ReadIssue::Failed)?
        .into_iter()
        .collect::<HashMap<_, _>>();
    let mut names = Vec::with_capacity(relationship_ids.len());
    for relationship_id in relationship_ids {
        let target = targets.get(&relationship_id).ok_or(ReadIssue::Failed)?;
        archive.by_name(target).map_err(|_| ReadIssue::Failed)?;
        names.push(target.clone());
    }
    Ok(names)
}

fn presentation_relationship_ids(bytes: &[u8]) -> Result<Vec<String>, XmlIssue> {
    ordered_relationship_ids(
        bytes,
        b"presentation",
        PRESENTATIONML_NAMESPACE,
        b"sldIdLst",
        b"sldId",
    )
}

fn presentation_relationship_targets(bytes: &[u8]) -> Result<Vec<(String, String)>, XmlIssue> {
    relationship_targets(bytes, "/slide", presentation_target_name)
}

fn presentation_target_name(target: &str) -> Result<String, XmlIssue> {
    normalized_part_target(target, "ppt/")
}

fn read_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    budget: &mut DecodedBudget,
) -> Result<Vec<u8>, ReadIssue> {
    let file = archive.by_name(name).map_err(|_| ReadIssue::Failed)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadIssue::Failed)?;
    if u64::try_from(bytes.len()).map_err(|_| ReadIssue::Failed)? > MAX_ENTRY_BYTES {
        return Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT));
    }
    budget.include(u64::try_from(bytes.len()).map_err(|_| ReadIssue::Failed)?)?;
    Ok(bytes)
}

fn extract_xml_text(bytes: &[u8], kind: OfficeKind) -> Result<String, XmlIssue> {
    let (expected_root, expected_namespace) = expected_text_root(kind);
    validate_xml_root(bytes, expected_root, expected_namespace)?;
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut element_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut saw_root = false;
    let mut alternate_depth = None;
    let mut selected_branch_depth = None;
    let mut supported_choice_found = false;
    let mut ignored_depth = None;
    let mut namespace_scopes = vec![std::collections::HashMap::new()];
    let mut compatibility_scopes = vec![MarkupCompatibilityScope::default()];
    let mut text_elements = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(start) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &start, &mut scope)?;
                let mut compatibility = compatibility_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_markup_compatibility_attributes(&reader, &start, &scope, &mut compatibility)?;
                if element_depth == 0 {
                    if saw_root {
                        return Err(XmlIssue::Malformed);
                    }
                    saw_root = true;
                }
                element_depth = next_xml_depth(element_depth)?;
                let qualified_name = start.name();
                let name = local_name(qualified_name.as_ref());
                if ignored_depth.is_none()
                    && ignores_markup_compatibility_element(
                        qualified_name.as_ref(),
                        &scope,
                        &compatibility,
                    )
                {
                    ignored_depth = Some(element_depth);
                }
                let not_ignored = ignored_depth.is_none();
                let selected =
                    not_ignored && (alternate_depth.is_none() || selected_branch_depth.is_some());
                let text_namespace = match kind {
                    OfficeKind::Docx => WORDPROCESSINGML_NAMESPACE,
                    OfficeKind::Xlsx => SPREADSHEETML_NAMESPACE,
                    OfficeKind::Pptx => DRAWINGML_NAMESPACE,
                };
                let supported_text = name == b"t"
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        &scope,
                        text_namespace,
                    );
                let supported_word_control = kind == OfficeKind::Docx
                    && (name == b"tab" || name == b"br" || name == b"cr")
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        &scope,
                        WORDPROCESSINGML_NAMESPACE,
                    );
                let supported_drawing_break = kind == OfficeKind::Pptx
                    && name == b"br"
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        &scope,
                        DRAWINGML_NAMESPACE,
                    );
                let markup_compatibility_element = element_uses_scoped_namespace(
                    qualified_name.as_ref(),
                    &scope,
                    MARKUP_COMPATIBILITY_NAMESPACE,
                );
                if (supported_word_control || supported_drawing_break) && selected {
                    append_xml_text(&mut output, if name == b"tab" { "\t" } else { "\n" })?;
                } else if not_ignored && name == b"AlternateContent" && markup_compatibility_element
                {
                    if alternate_depth.replace(element_depth).is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    supported_choice_found = false;
                } else if not_ignored
                    && name == b"Choice"
                    && markup_compatibility_element
                    && alternate_depth.is_some_and(|depth| element_depth == depth + 1)
                    && !supported_choice_found
                    && markup_choice_is_supported(&reader, &start, &scope)?
                {
                    supported_choice_found = true;
                    selected_branch_depth = Some(element_depth);
                } else if not_ignored
                    && name == b"Fallback"
                    && markup_compatibility_element
                    && alternate_depth.is_some_and(|depth| element_depth == depth + 1)
                    && !supported_choice_found
                {
                    if selected_branch_depth.replace(element_depth).is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                } else if supported_text && selected {
                    text_depth = next_xml_depth(text_depth)?;
                }
                text_elements.push(supported_text && selected);
                namespace_scopes.push(scope);
                compatibility_scopes.push(compatibility);
            }
            Event::End(end) => {
                element_depth = element_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                let was_text = text_elements.pop().ok_or(XmlIssue::Malformed)?;
                let qualified_name = end.name();
                let name = local_name(qualified_name.as_ref());
                let scope = namespace_scopes.last().ok_or(XmlIssue::Malformed)?;
                let markup_compatibility_element = element_uses_scoped_namespace(
                    qualified_name.as_ref(),
                    scope,
                    MARKUP_COMPATIBILITY_NAMESPACE,
                );
                let paragraph_namespace = match kind {
                    OfficeKind::Docx => WORDPROCESSINGML_NAMESPACE,
                    OfficeKind::Xlsx => SPREADSHEETML_NAMESPACE,
                    OfficeKind::Pptx => DRAWINGML_NAMESPACE,
                };
                let supported_paragraph = name == b"p"
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        scope,
                        paragraph_namespace,
                    );
                let supported_spreadsheet_boundary = kind == OfficeKind::Xlsx
                    && (name == b"si" || name == b"c")
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        scope,
                        SPREADSHEETML_NAMESPACE,
                    );
                if was_text {
                    text_depth = text_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                } else if (name == b"Choice" || name == b"Fallback")
                    && markup_compatibility_element
                    && selected_branch_depth.is_some_and(|depth| element_depth + 1 == depth)
                {
                    selected_branch_depth = None;
                } else if name == b"AlternateContent"
                    && markup_compatibility_element
                    && alternate_depth.is_some_and(|depth| element_depth + 1 == depth)
                {
                    if selected_branch_depth.is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    alternate_depth = None;
                    supported_choice_found = false;
                } else if (supported_paragraph || supported_spreadsheet_boundary)
                    && ignored_depth.is_none()
                    && (alternate_depth.is_none() || selected_branch_depth.is_some())
                    && !output.is_empty()
                    && !output.ends_with('\n')
                {
                    append_xml_text(&mut output, "\n")?;
                }
                if ignored_depth.is_some_and(|depth| element_depth + 1 == depth) {
                    ignored_depth = None;
                }
                namespace_scopes.pop().ok_or(XmlIssue::Malformed)?;
                compatibility_scopes.pop().ok_or(XmlIssue::Malformed)?;
            }
            Event::Empty(empty) => {
                let mut scope = namespace_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_namespace_declarations(&reader, &empty, &mut scope)?;
                let mut compatibility = compatibility_scopes
                    .last()
                    .cloned()
                    .ok_or(XmlIssue::Malformed)?;
                apply_markup_compatibility_attributes(&reader, &empty, &scope, &mut compatibility)?;
                if element_depth == 0 {
                    if saw_root {
                        return Err(XmlIssue::Malformed);
                    }
                    saw_root = true;
                }
                let qualified_name = empty.name();
                let name = local_name(qualified_name.as_ref());
                let selected = ignored_depth.is_none()
                    && !ignores_markup_compatibility_element(
                        qualified_name.as_ref(),
                        &scope,
                        &compatibility,
                    )
                    && (alternate_depth.is_none() || selected_branch_depth.is_some());
                let supported_word_control = kind == OfficeKind::Docx
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        &scope,
                        WORDPROCESSINGML_NAMESPACE,
                    );
                let supported_drawing_break = kind == OfficeKind::Pptx
                    && name == b"br"
                    && element_uses_scoped_namespace(
                        qualified_name.as_ref(),
                        &scope,
                        DRAWINGML_NAMESPACE,
                    );
                if name == b"tab" && selected && supported_word_control {
                    append_xml_text(&mut output, "\t")?;
                } else if selected
                    && (((name == b"br" || name == b"cr") && supported_word_control)
                        || supported_drawing_break)
                {
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
            Event::Text(text) if element_depth == 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                if !decoded.trim().is_empty() {
                    return Err(XmlIssue::Malformed);
                }
            }
            Event::CData(_) if element_depth == 0 => return Err(XmlIssue::Malformed),
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof
                if saw_root
                    && element_depth == 0
                    && text_depth == 0
                    && alternate_depth.is_none()
                    && selected_branch_depth.is_none()
                    && ignored_depth.is_none() =>
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

fn expected_text_root(kind: OfficeKind) -> (&'static [u8], &'static [u8]) {
    match kind {
        OfficeKind::Docx => (b"document", WORDPROCESSINGML_NAMESPACE),
        OfficeKind::Xlsx => (b"worksheet", SPREADSHEETML_NAMESPACE),
        OfficeKind::Pptx => (b"sld", PRESENTATIONML_NAMESPACE),
    }
}

fn validate_xml_root(
    bytes: &[u8],
    expected_name: &[u8],
    expected_namespace: &[u8],
) -> Result<(), XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut saw_root = false;
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                if depth == 0 {
                    if saw_root
                        || !element_is_expected_root(
                            &reader,
                            &element,
                            expected_name,
                            expected_namespace,
                        )
                    {
                        return Err(XmlIssue::Malformed);
                    }
                    saw_root = true;
                }
                depth = next_xml_depth(depth)?;
            }
            Event::Empty(element) if depth == 0 => {
                if saw_root
                    || !element_is_expected_root(
                        &reader,
                        &element,
                        expected_name,
                        expected_namespace,
                    )
                {
                    return Err(XmlIssue::Malformed);
                }
                saw_root = true;
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
            }
            Event::Text(text) if depth == 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                if !decoded.trim().is_empty() {
                    return Err(XmlIssue::Malformed);
                }
            }
            Event::CData(_) if depth == 0 => return Err(XmlIssue::Malformed),
            Event::GeneralRef(_) if depth == 0 => return Err(XmlIssue::Malformed),
            Event::DocType(_) => return Err(XmlIssue::Malformed),
            Event::Eof if saw_root && depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(())
}

fn element_is_expected_root(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    expected_name: &[u8],
    expected_namespace: &[u8],
) -> bool {
    if local_name(element.name().as_ref()) != expected_name {
        return false;
    }
    let qualified_name = element.name();
    let name = qualified_name.as_ref();
    let prefix = name
        .iter()
        .position(|byte| *byte == b':')
        .map_or(b"".as_slice(), |separator| &name[..separator]);
    namespace_declaration(reader, element, prefix)
        .is_some_and(|actual| namespace_matches(&actual, expected_namespace))
}

fn namespace_matches(actual: &[u8], expected: &[u8]) -> bool {
    actual == expected
        || matches!(
            (actual, expected),
            (
                STRICT_OFFICE_RELATIONSHIPS_NAMESPACE,
                OFFICE_RELATIONSHIPS_NAMESPACE
            ) | (
                STRICT_WORDPROCESSINGML_NAMESPACE,
                WORDPROCESSINGML_NAMESPACE
            ) | (STRICT_SPREADSHEETML_NAMESPACE, SPREADSHEETML_NAMESPACE)
                | (STRICT_PRESENTATIONML_NAMESPACE, PRESENTATIONML_NAMESPACE)
                | (STRICT_DRAWINGML_NAMESPACE, DRAWINGML_NAMESPACE)
        )
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
    if total > MAX_TEXT_BODY_BYTES {
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
    if total > MAX_TEXT_BODY_BYTES {
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

#[cfg(test)]
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .enumerate()
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

fn find_consistent_eocd(bytes: &[u8], suffix_start: u64) -> Option<usize> {
    bytes
        .windows(4)
        .enumerate()
        .find_map(|(offset, signature)| {
            if signature != b"PK\x05\x06" {
                return None;
            }
            let record = bytes.get(offset..)?;
            let comment_length = usize::from(le_u16(record, 20).ok()?);
            if record.len() != 22_usize.checked_add(comment_length)? {
                return None;
            }
            let (entries, central_size, central_offset) =
                central_directory_fields(bytes, offset, suffix_start).ok()?;
            if entries == 0 || central_size == 0 {
                return None;
            }
            let central_end = central_offset.checked_add(central_size)?;
            let uses_zip64 = le_u16(record, 8).ok()? == u16::MAX
                || le_u16(record, 10).ok()? == u16::MAX
                || le_u32(record, 12).ok()? == u32::MAX
                || le_u32(record, 16).ok()? == u32::MAX;
            let trailer_start = if uses_zip64 {
                let locator_offset = offset.checked_sub(20)?;
                let locator = bytes.get(locator_offset..offset)?;
                le_u64(locator, 8).ok()?
            } else {
                suffix_start.checked_add(u64::try_from(offset).ok()?)?
            };
            (central_end == trailer_start).then_some(offset)
        })
}

fn central_directory_fields(
    suffix: &[u8],
    eocd_offset: usize,
    suffix_start: u64,
) -> Result<(usize, u64, u64), ValidationIssue> {
    let eocd = suffix
        .get(eocd_offset..)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if le_u16(eocd, 4)? != 0 || le_u16(eocd, 6)? != 0 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }

    let entries_on_disk = le_u16(eocd, 8)?;
    let entries = le_u16(eocd, 10)?;
    let central_size = le_u32(eocd, 12)?;
    let central_offset = le_u32(eocd, 16)?;
    let uses_zip64 = entries_on_disk == u16::MAX
        || entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX;
    if !uses_zip64 {
        if entries_on_disk != entries {
            return Err(ValidationIssue::Malformed(MALFORMED_REASON));
        }
        return Ok((
            usize::from(entries),
            u64::from(central_size),
            u64::from(central_offset),
        ));
    }

    let locator_offset = eocd_offset
        .checked_sub(20)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let locator = suffix
        .get(locator_offset..eocd_offset)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if locator.get(0..4) != Some(b"PK\x06\x07")
        || le_u32(locator, 4)? != 0
        || le_u32(locator, 16)? != 1
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }

    let record_absolute = le_u64(locator, 8)?;
    let record_offset = record_absolute
        .checked_sub(suffix_start)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let record = suffix
        .get(record_offset..locator_offset)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if record.get(0..4) != Some(b"PK\x06\x06")
        || le_u64(record, 4)?
            .checked_add(12)
            .and_then(|length| usize::try_from(length).ok())
            != Some(record.len())
        || le_u32(record, 16)? != 0
        || le_u32(record, 20)? != 0
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }

    let zip64_entries_on_disk = le_u64(record, 24)?;
    let zip64_entries = le_u64(record, 32)?;
    if zip64_entries_on_disk != zip64_entries
        || (entries_on_disk != u16::MAX && u64::from(entries_on_disk) != zip64_entries)
        || (entries != u16::MAX && u64::from(entries) != zip64_entries)
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    let zip64_central_size = le_u64(record, 40)?;
    let zip64_central_offset = le_u64(record, 48)?;
    if (central_size != u32::MAX && u64::from(central_size) != zip64_central_size)
        || (central_offset != u32::MAX && u64::from(central_offset) != zip64_central_offset)
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }

    Ok((
        usize::try_from(zip64_entries)
            .map_err(|_| ValidationIssue::Malformed(ENTRY_COUNT_LIMIT))?,
        zip64_central_size,
        zip64_central_offset,
    ))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64, ValidationIssue> {
    let value = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
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

    struct BytesSource(Vec<u8>);

    impl VerifiedBlobSource for BytesSource {
        fn digest(&self) -> FileDigest {
            FileDigest::from_bytes([0x42; 32])
        }

        fn byte_length(&self) -> NonZeroU64 {
            NonZeroU64::new(u64::try_from(self.0.len()).expect("fixture length fits u64"))
                .expect("fixture length is nonzero")
        }

        fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
            Box::pin(async move {
                let start = usize::try_from(offset).map_err(|_| SourceReadError::Unavailable)?;
                let length =
                    usize::try_from(length.get()).map_err(|_| SourceReadError::Unavailable)?;
                let end = start
                    .checked_add(length)
                    .ok_or(SourceReadError::Unavailable)?;
                self.0
                    .get(start..end)
                    .map(Vec::from)
                    .ok_or(SourceReadError::Unavailable)
            })
        }
    }

    #[test]
    fn unrecognized_declared_candidate_returns_no_match() {
        let result = ValidationIssue::Unrecognized.validation(OfficeKind::Docx);

        assert_eq!(result, ProcessorValidationOutput::NoMatch);
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

        assert_eq!(
            result.expect("the Office family should validate"),
            vec![OfficeKind::Docx]
        );
    }

    #[test]
    fn content_types_honor_inherited_prefixes() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\" ",
            "xmlns:ct=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<ct:Override PartName=\"/word/document.xml\" ",
            "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert_eq!(
            result.expect("the inherited content-types prefix should resolve"),
            vec![OfficeKind::Docx]
        );
    }

    #[test]
    fn content_types_resolve_main_parts_from_defaults() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Default Extension=\"xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        let kinds = result.expect("a matching default should type the main part");
        assert_eq!(kinds, vec![OfficeKind::Docx]);
    }

    #[test]
    fn content_types_transcode_utf16_metadata() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Override PartName=\"/word/document.xml\" ",
            "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "</Types>"
        );
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));

        let result = validate_content_types(&bytes, &[docx_main_entry()]);

        assert_eq!(
            result.expect("the Office family should validate"),
            vec![OfficeKind::Docx]
        );
    }

    #[test]
    fn content_types_reject_duplicate_part_overrides() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "<Override PartName=\"/word/document.xml\" ContentType=\"application/xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn content_types_reject_incomplete_overrides() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Override PartName=\"/word/document.xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn content_types_reject_incomplete_defaults() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Default Extension=\"xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn content_types_reject_vba_projects_by_content_type() {
        let xml = concat!(
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "<Override PartName=\"/custom/project.bin\" ContentType=\"application/vnd.ms-office.vbaProject\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn package_relationships_require_the_opc_namespace() {
        let xml = concat!(
            "<Relationships>",
            "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>",
            "</Relationships>"
        );

        let result = validate_package_relationships(xml.as_bytes(), &[OfficeKind::Docx]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn package_relationships_transcode_utf16_metadata() {
        let xml = concat!(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
            "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>",
            "</Relationships>"
        );
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));

        let result = validate_package_relationships(&bytes, &[OfficeKind::Docx]);

        assert_eq!(
            result.expect("the Office family should validate"),
            vec![OfficeKind::Docx]
        );
    }

    #[test]
    fn package_relationships_ignore_suffix_collisions() {
        let xml = concat!(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
            "<Relationship Id=\"extension\" Type=\"urn:extension/officeDocument\" Target=\"custom.xml\"/>",
            "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>",
            "</Relationships>"
        );

        let result = validate_package_relationships(xml.as_bytes(), &[OfficeKind::Docx]);

        assert_eq!(
            result.expect("only the exact Office relationship should match"),
            vec![OfficeKind::Docx]
        );
    }

    #[test]
    fn package_relationships_require_unique_ids() {
        let missing = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let duplicate = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="duplicate" Type="urn:extension" Target="custom.xml"/><Relationship Id="duplicate" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

        assert!(matches!(
            validate_package_relationships(missing, &[OfficeKind::Docx]),
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
        assert!(matches!(
            validate_package_relationships(duplicate, &[OfficeKind::Docx]),
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn package_relationships_require_complete_attributes() {
        let missing_type = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="custom.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let missing_target = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="urn:extension"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

        assert!(matches!(
            validate_package_relationships(missing_type, &[OfficeKind::Docx]),
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
        assert!(matches!(
            validate_package_relationships(missing_target, &[OfficeKind::Docx]),
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn part_relationships_honor_inherited_prefixes() {
        let workbook_xml = br#"<pr:Relationships xmlns:pr="http://schemas.openxmlformats.org/package/2006/relationships" xmlns:rel="http://schemas.openxmlformats.org/package/2006/relationships"><rel:Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></pr:Relationships>"#;
        let presentation_xml = br#"<pr:Relationships xmlns:pr="http://schemas.openxmlformats.org/package/2006/relationships" xmlns:rel="http://schemas.openxmlformats.org/package/2006/relationships"><rel:Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></pr:Relationships>"#;

        let workbook = workbook_relationship_targets(workbook_xml)
            .expect("the inherited workbook relationship prefix should resolve");
        let presentation = presentation_relationship_targets(presentation_xml)
            .expect("the inherited presentation relationship prefix should resolve");

        assert_eq!(
            workbook,
            vec![(
                String::from("rId1"),
                Some(String::from("xl/worksheets/sheet1.xml")),
            )]
        );
        assert_eq!(
            presentation,
            vec![(String::from("rId1"), String::from("ppt/slides/slide1.xml"),)]
        );
    }

    #[test]
    fn relationship_targets_follow_parts_outside_conventional_directories() {
        let workbook_xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="custom/sheet.xml"/></Relationships>"#;
        let presentation_xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="custom/slide.xml"/></Relationships>"#;

        assert_eq!(
            workbook_relationship_targets(workbook_xml)
                .expect("the worksheet target should resolve"),
            vec![(
                String::from("rId1"),
                Some(String::from("xl/custom/sheet.xml")),
            )]
        );
        assert_eq!(
            presentation_relationship_targets(presentation_xml)
                .expect("the slide target should resolve"),
            vec![(String::from("rId1"), String::from("ppt/custom/slide.xml"))]
        );
    }

    #[test]
    fn relationship_selected_parts_do_not_require_xml_extensions() {
        let workbook_xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet.data"/></Relationships>"#;
        let presentation_xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide.data"/></Relationships>"#;
        let shared_strings_xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="tables/strings.data"/></Relationships>"#;

        let workbook = workbook_relationship_targets(workbook_xml)
            .expect("extension-independent worksheet target should parse");
        let presentation = presentation_relationship_targets(presentation_xml)
            .expect("extension-independent slide target should parse");
        let shared_strings = workbook_shared_strings_target(shared_strings_xml)
            .expect("extension-independent shared-string target should parse");

        assert_eq!(
            workbook,
            vec![(
                String::from("rId1"),
                Some(String::from("xl/worksheets/sheet.data")),
            )]
        );
        assert_eq!(
            presentation,
            vec![(String::from("rId2"), String::from("ppt/slides/slide.data"),)]
        );
        assert_eq!(shared_strings, Some(String::from("xl/tables/strings.data")));
    }

    #[test]
    fn workbook_relationships_reject_character_data() {
        let xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">garbage<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;

        let result = workbook_relationship_targets(xml);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn presentation_relationships_reject_cdata() {
        let xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><![CDATA[garbage]]><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;

        let result = presentation_relationship_targets(xml);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn part_relationships_reject_duplicate_irrelevant_ids() {
        let xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="duplicate" Type="urn:extension/first" Target="first.xml"/><Relationship Id="duplicate" Type="urn:extension/second" Target="second.xml"/></Relationships>"#;

        let result = workbook_relationship_targets(xml);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn package_targets_reject_empty_path_segments() {
        let result = normalized_package_target("word//document.xml");

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
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
    fn central_entries_must_start_on_the_current_disk() {
        let name = CONTENT_TYPES.as_bytes();
        let mut central = vec![0_u8; 46 + name.len()];
        central[0..4].copy_from_slice(b"PK\x01\x02");
        central[28..30].copy_from_slice(
            &u16::try_from(name.len())
                .expect("fixture entry name fits in a ZIP length")
                .to_le_bytes(),
        );
        central[34..36].copy_from_slice(&1_u16.to_le_bytes());
        central[46..].copy_from_slice(name);

        assert!(matches!(
            parse_central_entry(&central, 0),
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn probe_budget_reserves_all_local_header_reads() {
        let admitted_central_bytes = VALIDATION_SOURCE_BYTES
            - ZIP_SUFFIX_BYTES
            - EOCD_PRECEDING_BYTES
            - ZIP_PREFIX_BYTES
            - LOCAL_HEADER_BYTES
            - CONTENT_TYPES_NAME_BYTES
            - LOCAL_EXTRA_BYTES
            - CONTENT_TYPES_COMPRESSED_BYTES
            - LOCAL_HEADER_BYTES
            - PACKAGE_RELS_NAME_BYTES
            - LOCAL_EXTRA_BYTES
            - PACKAGE_RELS_COMPRESSED_BYTES
            - MAX_SELECTED_PARTS
                * (LOCAL_HEADER_BYTES + SELECTED_PART_NAME_BYTES + LOCAL_EXTRA_BYTES);

        assert_eq!(
            ZIP_PREFIX_BYTES
                + ZIP_SUFFIX_BYTES
                + EOCD_PRECEDING_BYTES
                + admitted_central_bytes
                + LOCAL_HEADER_BYTES
                + CONTENT_TYPES_NAME_BYTES
                + LOCAL_EXTRA_BYTES
                + CONTENT_TYPES_COMPRESSED_BYTES
                + LOCAL_HEADER_BYTES
                + PACKAGE_RELS_NAME_BYTES
                + LOCAL_EXTRA_BYTES
                + PACKAGE_RELS_COMPRESSED_BYTES
                + MAX_SELECTED_PARTS
                    * (LOCAL_HEADER_BYTES + SELECTED_PART_NAME_BYTES + LOCAL_EXTRA_BYTES),
            VALIDATION_SOURCE_BYTES
        );
    }

    #[tokio::test]
    async fn selected_parts_require_matching_local_headers() {
        let name = OfficeKind::Docx.marker();
        let mut bytes = vec![0_u8; 30 + name.len()];
        bytes[0..4].copy_from_slice(b"PK\x03\x04");
        bytes[18..22].copy_from_slice(&1_u32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1_u32.to_le_bytes());
        bytes[26..28].copy_from_slice(
            &u16::try_from(name.len())
                .expect("fixture name fits in a ZIP length")
                .to_le_bytes(),
        );
        bytes[30..].copy_from_slice(b"word/document.xmz");
        let source = BytesSource(bytes);
        let entry = CentralEntry {
            name: String::from(name),
            flags: 0,
            compression: 0,
            crc32: 0,
            compressed_bytes: 1,
            expanded_bytes: 1,
            local_offset: 0,
        };

        let result = read_probe_local_header(&source, &NeverCancelled, &entry, &mut 0).await;

        assert!(matches!(
            result,
            Err(CentralReadError::Validation {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                ..
            })
        ));
    }

    #[test]
    fn local_header_fields_must_match_the_central_entry() {
        let entry = CentralEntry {
            name: String::from(CONTENT_TYPES),
            flags: 0,
            compression: 0,
            crc32: 7,
            compressed_bytes: 11,
            expanded_bytes: 11,
            local_offset: 0,
        };
        let mut local = vec![0_u8; usize::try_from(LOCAL_HEADER_BYTES).expect("header fits")];
        local[14..18].copy_from_slice(&8_u32.to_le_bytes());
        local[18..22].copy_from_slice(&11_u32.to_le_bytes());
        local[22..26].copy_from_slice(&11_u32.to_le_bytes());

        let result = validate_local_header_fields(&local, &[], &entry);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn local_zip64_sizes_must_match_the_central_entry() {
        let entry = CentralEntry {
            name: String::from(CONTENT_TYPES),
            flags: 0,
            compression: 0,
            crc32: 7,
            compressed_bytes: 11,
            expanded_bytes: 11,
            local_offset: 0,
        };
        let mut local = vec![0_u8; usize::try_from(LOCAL_HEADER_BYTES).expect("header fits")];
        local[14..18].copy_from_slice(&7_u32.to_le_bytes());
        local[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        local[22..26].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut extra = Vec::from([1_u8, 0, 16, 0]);
        extra.extend_from_slice(&12_u64.to_le_bytes());
        extra.extend_from_slice(&11_u64.to_le_bytes());

        let result = validate_local_header_fields(&local, &extra, &entry);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
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
            examined_bytes: 0,
        };

        let result = read_probe_entry(
            &UnavailableSource,
            &NeverCancelled,
            &inventory,
            CONTENT_TYPES,
            CONTENT_TYPES_COMPRESSED_BYTES,
            &mut 0,
        )
        .await;

        assert!(matches!(
            result,
            Err(CentralReadError::Processor(ProcessorFailure::Failed))
        ));
    }

    #[test]
    fn encrypted_selected_entry_skips_decoder_validation() {
        let inventory = CentralInventory {
            entries: 1,
            expanded_bytes: MAX_ENTRY_BYTES + 1,
            entries_by_name: vec![CentralEntry {
                name: String::from(OfficeKind::Docx.marker()),
                flags: 1,
                compression: 99,
                crc32: 0,
                compressed_bytes: 1,
                expanded_bytes: MAX_ENTRY_BYTES + 1,
                local_offset: 0,
            }],
            kinds: vec![OfficeKind::Docx],
            encrypted: true,
            examined_bytes: 0,
        };

        let entry = inventory
            .entries_by_name
            .first()
            .expect("the fixture should contain its selected entry");
        assert!(validate_selected_probe_entry(entry).is_ok());
    }

    #[test]
    fn text_extraction_decodes_numeric_references() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>&#65;&#x42;</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(result.expect("numeric references should decode"), "AB");
    }

    #[test]
    fn text_extraction_preserves_cdata() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t><![CDATA[A&B]]></w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(result.expect("CDATA should be preserved"), "A&B");
    }

    #[test]
    fn text_extraction_selects_markup_compatibility_fallback() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:ext="urn:unsupported"><mc:AlternateContent><mc:Choice Requires="ext"><w:p><w:t>choice</w:t></w:p></mc:Choice><mc:Fallback><w:p><w:t>fallback</w:t></w:p></mc:Fallback></mc:AlternateContent></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("the fallback branch should extract"),
            "fallback\n"
        );
    }

    #[test]
    fn text_extraction_selects_supported_markup_compatibility_choice() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:AlternateContent><mc:Choice Requires="w"><w:p><w:t>choice</w:t></w:p></mc:Choice><mc:Fallback><w:p><w:t>fallback</w:t></w:p></mc:Fallback></mc:AlternateContent></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("the supported choice branch should extract"),
            "choice\n"
        );
    }

    #[test]
    fn text_extraction_ignores_extension_branch_names() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:ext="urn:extension"><ext:AlternateContent><ext:Fallback><w:t>extension text</w:t></ext:Fallback></ext:AlternateContent><w:t>document text</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("extension branch names should not alter selection"),
            "extension textdocument text"
        );
    }

    #[test]
    fn text_extraction_skips_ignorable_extension_subtrees() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:ext="urn:extension" mc:Ignorable="ext"><ext:metadata><w:t>hidden</w:t></ext:metadata><w:t>visible</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("ignorable extension content should be skipped"),
            "visible"
        );
    }

    #[test]
    fn text_extraction_honors_markup_process_content() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:ext="urn:extension" mc:Ignorable="ext" mc:ProcessContent="ext:metadata"><ext:metadata><w:t>visible</w:t></ext:metadata></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("authorized extension content should be processed"),
            "visible"
        );
    }

    #[test]
    fn text_extraction_rejects_unsupported_mandatory_namespaces() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:ext="urn:unsupported" mc:MustUnderstand="ext"><w:t>text</w:t></w:document>"#;

        assert!(matches!(
            extract_xml_text(xml, OfficeKind::Docx),
            Err(XmlIssue::Malformed)
        ));
    }

    #[test]
    fn namespace_scopes_bound_declaration_count_and_size() {
        let mut many = String::from(
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"",
        );
        for index in 0..MAX_NAMESPACE_DECLARATIONS {
            many.push_str(&format!(" xmlns:n{index}=\"urn:n{index}\""));
        }
        many.push_str("><w:t>text</w:t></w:document>");

        assert!(matches!(
            extract_xml_text(many.as_bytes(), OfficeKind::Docx),
            Err(XmlIssue::Malformed)
        ));

        let long_uri = format!(
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:ext=\"urn:{}\"><w:t>text</w:t></w:document>",
            "a".repeat(MAX_NAMESPACE_URI_BYTES)
        );

        assert!(matches!(
            extract_xml_text(long_uri.as_bytes(), OfficeKind::Docx),
            Err(XmlIssue::Malformed)
        ));
    }

    #[tokio::test]
    async fn selected_probe_entries_require_complete_payload_ranges() {
        let name = b"word/document.xml";
        let mut header = Vec::new();
        header.extend_from_slice(b"PK\x03\x04");
        header.extend_from_slice(&[20, 0]);
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(&8_u16.to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        header.extend_from_slice(&0xAABB_CCDD_u32.to_le_bytes());
        header.extend_from_slice(&64_u32.to_le_bytes());
        header.extend_from_slice(&32_u32.to_le_bytes());
        header.extend_from_slice(
            &u16::try_from(name.len())
                .expect("name length")
                .to_le_bytes(),
        );
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(name);
        let entry = CentralEntry {
            name: String::from("word/document.xml"),
            flags: 0,
            compression: 8,
            crc32: 0xAABB_CCDD,
            compressed_bytes: 64,
            expanded_bytes: 32,
            local_offset: 0,
        };
        let inventory = CentralInventory {
            entries: 1,
            expanded_bytes: 32,
            entries_by_name: vec![entry],
            kinds: vec![OfficeKind::Docx],
            encrypted: false,
            examined_bytes: 0,
        };

        let truncated = BytesSource(header.clone());
        assert!(
            validate_selected_probe_entries(&truncated, &NeverCancelled, &inventory, &mut 0)
                .await
                .is_err()
        );

        let mut complete_bytes = header;
        complete_bytes.extend_from_slice(&[0_u8; 64]);
        let complete = BytesSource(complete_bytes);
        assert!(
            validate_selected_probe_entries(&complete, &NeverCancelled, &inventory, &mut 0)
                .await
                .is_ok()
        );
    }

    #[test]
    fn text_extraction_ignores_extension_paragraph_boundaries() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:ext="urn:extension"><w:p><w:t>first</w:t><ext:p></ext:p><w:t>second</w:t></w:p></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("extension paragraphs should not split displayed text"),
            "firstsecond\n"
        );
    }

    #[test]
    fn text_extraction_preserves_drawing_breaks() {
        let xml = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:t>first</a:t><a:br/><a:t>second</a:t><a:br></a:br><a:t>third</a:t></p:sld>"#;

        let result = extract_xml_text(xml, OfficeKind::Pptx);

        assert_eq!(
            result.expect("DrawingML breaks should extract"),
            "first\nsecond\nthird"
        );
    }

    #[test]
    fn text_extraction_rejects_excessive_xml_depth() {
        let opening = "<w:p>".repeat(MAX_XML_DEPTH);
        let closing = "</w:p>".repeat(MAX_XML_DEPTH);
        let xml = format!(
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{opening}<w:t>deep</w:t>{closing}</w:document>"
        );

        let result = extract_xml_text(xml.as_bytes(), OfficeKind::Docx);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn presentation_relationship_ids_ignore_extension_slide_ids() {
        let xml = br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p14="urn:p14"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst><p:extLst><p14:sldId id="99"/></p:extLst></p:presentation>"#;

        let result = presentation_relationship_ids(xml);

        assert_eq!(
            result.expect("the relationship ID should parse"),
            vec![String::from("rId1")]
        );
    }

    #[test]
    fn workbook_relationship_ids_preserve_sheet_order() {
        let xml = br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet r:id="rId2"/><sheet r:id="rId1"/></sheets></workbook>"#;

        let result = workbook_relationship_ids(xml);

        assert_eq!(
            result.expect("sheet relationship IDs should preserve order"),
            vec![String::from("rId2"), String::from("rId1")]
        );
    }

    #[test]
    fn ordered_relationship_ids_restrict_lists_to_the_document_root() {
        let xml = br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><ext><sheets><sheet r:id="nested"/></sheets></ext><sheets><sheet r:id="root"/></sheets></workbook>"#;

        let result = workbook_relationship_ids(xml);

        assert_eq!(
            result.expect("only the root-level sheet list should be used"),
            vec![String::from("root")]
        );
    }

    #[test]
    fn ordered_relationship_ids_enforce_the_entry_ceiling() {
        let limit = usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES)
            .expect("the relationship ceiling should fit usize");
        let mut ids = vec![String::new(); limit];

        assert!(matches!(
            record_ordered_relationship_id(&mut ids, String::from("overflow")),
            Err(XmlIssue::Malformed)
        ));
    }

    #[test]
    fn relationship_ids_use_the_office_relationships_namespace() {
        let xml = br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:ext="urn:extension"><sheets><sheet ext:id="metadata" r:id="rId1"/></sheets></workbook>"#;

        let result = workbook_relationship_ids(xml);

        assert_eq!(
            result.expect("the relationship ID should parse"),
            vec![String::from("rId1")]
        );
    }

    #[test]
    fn relationship_ids_use_the_current_namespace_scope() {
        let xml = br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:o="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet xmlns:r="urn:extension" r:id="metadata" o:id="rId1"/></sheets></workbook>"#;

        let result = workbook_relationship_ids(xml);

        let ids = result.expect("the element-local namespace scope should be used");
        assert_eq!(ids, vec![String::from("rId1")]);
    }

    #[test]
    fn worksheet_shared_strings_follow_cell_occurrence_order() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="s"><v>1</v></c><c t="s"><v>1</v></c></row></sheetData></worksheet>"#;
        let shared = vec![String::from("unused"), String::from("used")];

        let result = spreadsheet_worksheet_text(xml, &shared);

        assert_eq!(
            result.expect("shared-string cells should extract"),
            "used\nused\n"
        );
    }

    #[test]
    fn shared_strings_decode_references_and_skip_phonetic_runs() {
        let xml =
            br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>R&amp;D</t><rPh><t>phonetic</t></rPh><r><t>&#33;</t></r></si></sst>"#;

        let result = spreadsheet_shared_strings(xml);

        assert_eq!(
            result.expect("shared strings should decode references"),
            vec![String::from("R&D!")]
        );
    }

    #[test]
    fn worksheet_inline_strings_follow_cell_occurrence_order() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="inlineStr"><is><t>first</t></is></c><c t="inlineStr"><is><r><t>second</t></r><r><t> value</t></r></is></c></row></sheetData></worksheet>"#;

        let result = spreadsheet_worksheet_text(xml, &[]);

        assert_eq!(
            result.expect("inline strings should preserve order"),
            "first\nsecond value\n"
        );
    }

    #[test]
    fn worksheet_extraction_ignores_extension_cell_names() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:ext="urn:extension"><sheetData><row><c t="inlineStr"><is><t>real</t></is></c><ext:c t="inlineStr"><ext:t>hidden</ext:t></ext:c></row></sheetData></worksheet>"#;

        let result = spreadsheet_worksheet_text(xml, &[]);

        assert_eq!(result.expect("extension cells should be ignored"), "real\n");
    }

    #[test]
    fn workbook_relationships_skip_non_worksheet_sheets() {
        let xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;

        let result = workbook_relationship_targets(xml);

        let targets = result.expect("workbook relationships should parse");
        assert_eq!(
            targets,
            vec![
                (String::from("rId1"), None),
                (
                    String::from("rId2"),
                    Some(String::from("xl/worksheets/sheet2.xml")),
                ),
            ]
        );
    }

    #[test]
    fn workbook_relationships_resolve_the_shared_string_table() {
        let xml = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdShared" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="tables/strings.xml"/></Relationships>"#;

        let result = workbook_shared_strings_target(xml);

        assert_eq!(
            result
                .expect("the shared-string relationship should parse")
                .expect("a shared-string target should be present"),
            "xl/tables/strings.xml"
        );
    }

    #[test]
    fn shared_strings_preserve_empty_items() {
        let xml = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si/><si></si><si><t>value</t></si></sst>"#;

        let result = spreadsheet_shared_strings(xml);

        let strings = result.expect("empty shared-string items should be preserved");
        assert_eq!(
            strings,
            vec![String::new(), String::new(), String::from("value")]
        );
    }

    #[test]
    fn inline_strings_decode_references_and_skip_phonetic_runs() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="inlineStr"><is><t>R&amp;D</t><rPh><t>phonetic</t></rPh><r><t>&#33;</t></r></is></c></row></sheetData></worksheet>"#;

        let result = spreadsheet_worksheet_text(xml, &[]);

        assert_eq!(
            result.expect("inline strings should decode references"),
            "R&D!\n"
        );
    }

    #[test]
    fn text_extraction_preserves_word_carriage_returns() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>first</w:t><w:cr/><w:t>second</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert_eq!(
            result.expect("Word carriage returns should extract"),
            "first\nsecond"
        );
    }

    #[test]
    fn text_extraction_preserves_expanded_word_controls() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>first</w:t><w:tab></w:tab><w:t>second</w:t><w:br></w:br><w:t>third</w:t><w:cr></w:cr><w:t>fourth</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        let text = result.expect("expanded Word controls should extract");
        assert_eq!(text, "first\tsecond\nthird\nfourth");
    }

    #[test]
    fn text_extraction_rejects_unrelated_docx_roots() {
        let xml = br#"<evil xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>spoofed</w:t></evil>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn text_extraction_rejects_unrelated_pptx_roots() {
        let xml = br#"<evil xmlns:a="urn:a"><a:t>spoofed</a:t></evil>"#;

        let result = extract_xml_text(xml, OfficeKind::Pptx);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn worksheet_extraction_rejects_unrelated_roots() {
        let xml = br#"<evil><c t="inlineStr"><is><t>spoofed</t></is></c></evil>"#;

        let result = spreadsheet_worksheet_text(xml, &[]);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn worksheet_reports_the_declared_output_limit() {
        let value = "x".repeat(MAX_TEXT_BODY_BYTES);
        let xml = format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="inlineStr"><is><t>{value}</t></is></c></row></sheetData></worksheet>"#
        );

        let result = spreadsheet_worksheet_text(xml.as_bytes(), &[]);

        assert!(matches!(result, Err(XmlIssue::OutputTooLarge)));
    }

    #[test]
    fn text_assembly_uses_the_declared_text_body_ceiling() {
        let mut output = "x".repeat(MAX_TEXT_BODY_BYTES);

        assert!(matches!(
            append_xml_text(&mut output, "x"),
            Err(XmlIssue::OutputTooLarge)
        ));
    }

    #[test]
    fn decoded_budget_is_aggregate_across_parts() {
        let mut budget = DecodedBudget::default();

        assert!(budget.include(MAX_ENTRY_BYTES).is_ok());
        assert!(
            budget
                .include(MAX_TOTAL_EXPANDED_BYTES - MAX_ENTRY_BYTES)
                .is_ok()
        );
        assert!(matches!(
            budget.include(1),
            Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT))
        ));
    }

    #[test]
    fn text_extraction_transcodes_utf16_little_endian() {
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>wide text</w:t></w:document>"#;
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));

        let result = extract_xml_text(&bytes, OfficeKind::Docx);

        assert_eq!(result.expect("UTF-16 text should transcode"), "wide text");
    }

    #[test]
    fn text_extraction_rejects_character_data_outside_the_root() {
        let xml = br#"outside<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>inside</w:t></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert!(matches!(result, Err(XmlIssue::Malformed)));
    }

    #[test]
    fn eocd_scan_ignores_signature_bytes_inside_the_comment() {
        let mut bytes = vec![0_u8; 22];
        bytes[0..4].copy_from_slice(b"PK\x05\x06");
        bytes[20..22].copy_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06tail");

        assert_eq!(find_eocd(&bytes), Some(0));
    }

    #[test]
    fn eocd_scan_skips_a_complete_false_record_inside_the_comment() {
        let mut bytes = vec![0_u8; 22];
        bytes[0..4].copy_from_slice(b"PK\x05\x06");
        bytes[20..22].copy_from_slice(&22_u16.to_le_bytes());
        let false_offset = bytes.len();
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&[0_u8; 18]);

        assert_eq!(find_eocd(&bytes), Some(0));
        assert_ne!(find_eocd(&bytes), Some(false_offset));
    }

    #[test]
    fn zip64_eocd_fields_are_resolved_before_entry_limits() {
        let suffix_start = 1_000_u64;
        let mut bytes = vec![0_u8; 56 + 20 + 22];
        bytes[0..4].copy_from_slice(b"PK\x06\x06");
        bytes[4..12].copy_from_slice(&44_u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&3_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&3_u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&123_u64.to_le_bytes());
        bytes[48..56].copy_from_slice(&456_u64.to_le_bytes());
        bytes[56..60].copy_from_slice(b"PK\x06\x07");
        bytes[64..72].copy_from_slice(&suffix_start.to_le_bytes());
        bytes[72..76].copy_from_slice(&1_u32.to_le_bytes());
        bytes[76..80].copy_from_slice(b"PK\x05\x06");
        bytes[84..86].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[86..88].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[88..92].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[92..96].copy_from_slice(&u32::MAX.to_le_bytes());

        let result = central_directory_fields(&bytes, 76, suffix_start);

        assert!(matches!(result, Ok((3, 123, 456))));
    }

    #[test]
    fn zip64_eocd_fields_allow_bounded_extensible_data() {
        let suffix_start = 1_000_u64;
        let extensible_bytes = 32_usize;
        let record_length = 56 + extensible_bytes;
        let locator_offset = record_length;
        let eocd_offset = locator_offset + 20;
        let mut bytes = vec![0_u8; eocd_offset + 22];
        bytes[0..4].copy_from_slice(b"PK\x06\x06");
        bytes[4..12].copy_from_slice(
            &u64::try_from(record_length - 12)
                .expect("fixture record length fits u64")
                .to_le_bytes(),
        );
        bytes[24..32].copy_from_slice(&3_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&3_u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&123_u64.to_le_bytes());
        bytes[48..56].copy_from_slice(&456_u64.to_le_bytes());
        bytes[locator_offset..locator_offset + 4].copy_from_slice(b"PK\x06\x07");
        bytes[locator_offset + 8..locator_offset + 16].copy_from_slice(&suffix_start.to_le_bytes());
        bytes[locator_offset + 16..locator_offset + 20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[eocd_offset..eocd_offset + 4].copy_from_slice(b"PK\x05\x06");
        bytes[eocd_offset + 8..eocd_offset + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[eocd_offset + 10..eocd_offset + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[eocd_offset + 12..eocd_offset + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[eocd_offset + 16..eocd_offset + 20].copy_from_slice(&u32::MAX.to_le_bytes());

        let result = central_directory_fields(&bytes, eocd_offset, suffix_start);

        assert!(matches!(result, Ok((3, 123, 456))));
    }

    #[test]
    fn zip64_trailer_budget_covers_bounded_extensible_data() {
        assert_eq!(EOCD_PRECEDING_BYTES, 21 + 20 + MAX_ZIP64_EOCD_BYTES);
    }

    #[test]
    fn central_errors_preserve_markers_that_follow_the_bad_entry() {
        let hostile_name = b"../hostile";
        let marker_name = b"word/document.xml";
        let marker_offset = 46 + hostile_name.len();
        let mut central = vec![0_u8; marker_offset + 46 + marker_name.len()];
        central[0..4].copy_from_slice(b"PK\x01\x02");
        central[28..30].copy_from_slice(
            &u16::try_from(hostile_name.len())
                .expect("fixture name fits in a ZIP length")
                .to_le_bytes(),
        );
        central[46..marker_offset].copy_from_slice(hostile_name);
        central[marker_offset..marker_offset + 4].copy_from_slice(b"PK\x01\x02");
        central[marker_offset + 28..marker_offset + 30].copy_from_slice(
            &u16::try_from(marker_name.len())
                .expect("fixture name fits in a ZIP length")
                .to_le_bytes(),
        );
        central[marker_offset + 46..].copy_from_slice(marker_name);

        let error = parse_central_directory(&central, 2)
            .expect_err("the hostile entry should reject the directory");

        assert_eq!(error.kinds, vec![OfficeKind::Docx]);
        assert!(matches!(
            error.issue,
            ValidationIssue::Malformed(HOSTILE_ENTRY_NAME)
        ));
    }

    #[test]
    fn central_parse_errors_preserve_markers_from_earlier_entries() {
        let marker_name = b"word/document.xml";
        let invalid_offset = 46 + marker_name.len();
        let mut central = vec![0_u8; invalid_offset + 4];
        central[0..4].copy_from_slice(b"PK\x01\x02");
        central[28..30].copy_from_slice(
            &u16::try_from(marker_name.len())
                .expect("fixture name fits in a ZIP length")
                .to_le_bytes(),
        );
        central[46..invalid_offset].copy_from_slice(marker_name);
        central[invalid_offset..].copy_from_slice(b"PK\x01\x02");

        let error = parse_central_directory(&central, 2)
            .expect_err("the truncated later entry should reject the directory");

        assert_eq!(error.kinds, vec![OfficeKind::Docx]);
        assert!(matches!(
            error.issue,
            ValidationIssue::Malformed(MALFORMED_REASON)
        ));
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
