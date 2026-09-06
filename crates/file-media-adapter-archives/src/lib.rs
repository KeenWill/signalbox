//! Bounded archive enumeration inside the supervised file-media worker, governed by
//! `docs/spec/file-and-media.md`.

use std::{
    error::Error,
    io::{Cursor, Read},
    num::NonZeroU64,
    str::FromStr,
};

use flate2::{bufread::GzDecoder, read::MultiGzDecoder};

use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration,
    ProbeDeclarationInput, ProbeStrength, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationDeclaration, ValidationEvidence, VerifiedBlobSource,
};
use zip::{CompressionMethod, ZipArchive};

const PROVIDER_NAME: &str = "archives";
const READER_REVISION: &str = "zip8-tar04-gz1-zstd013-v2";
const ENTRIES_VIEW: &str = "entries";
const MALFORMED_REASON: &str = "malformed_archive";
const ENTRY_COUNT_REASON: &str = "entry_count_limit";
const EXPANDED_SIZE_REASON: &str = "expanded_size_limit";
const HOSTILE_NAME_REASON: &str = "hostile_entry_name";
const LINK_ENTRY_REASON: &str = "link_entry";
const RECURSIVE_REASON: &str = "recursive_container";
const SPECIAL_ENTRY_REASON: &str = "special_entry";
const SOURCE_SIZE_REASON: &str = "source_size_limit";
const UNSUPPORTED_COMPRESSION_REASON: &str = "unsupported_compression_method";
// numeric-bound: hard safety ceiling - bounds probe I/O and retained signature evidence
const PROBE_BYTES: u64 = 1_024;
// numeric-bound: hard safety ceiling - bounds whole-archive memory and decode latency
const SOURCE_BYTES: u64 = 256 * 1024;
// numeric-bound: hard safety ceiling - bounds archive inventory memory and enumeration work
const MAX_ENTRIES: usize = 1_000;
// numeric-bound: hard safety ceiling - bounds one decoded entry's memory and CPU work
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
// numeric-bound: hard safety ceiling - prevents Zstandard frames from reserving more than 8 MiB
const ZSTD_WINDOW_LOG_MAX: u32 = 23;
// numeric-bound: hard safety ceiling - bounds aggregate decompression memory and CPU work
const MAX_EXPANDED_BYTES: u64 = 16 * 1024 * 1024;
// numeric-bound: hard safety ceiling - bounds retained untrusted entry-name memory
const MAX_NAME_BYTES: usize = 512;
// numeric-bound: hard safety ceiling - bounds one structured tool result
const OUTPUT_BYTES: usize = 500_000;
// numeric-bound: hard safety ceiling - bounds retained recursive-format prefix evidence
const PREFIX_BYTES: usize = 1_024;
// numeric-bound: hard safety ceiling - the entries view reads the complete source once
const READ_RANGES: u32 = 1;
// numeric-bound: hard safety ceiling - bounds structured result nesting
const OUTPUT_DEPTH: u32 = 5;
// numeric-bound: hard safety ceiling - bounds structured result traversal work
const OUTPUT_NODES: u64 = 5_000;
// numeric-bound: hard safety ceiling - bounds aggregate structured string memory
const OUTPUT_STRING_BYTES: usize = 480_000;
// numeric-bound: not-a-bound - fixed ZIP central-directory record header size
const ZIP_CENTRAL_HEADER_BYTES: usize = 46;
// numeric-bound: not-a-bound - fixed streaming read-buffer size for decoded bytes
const DECODE_BUFFER_BYTES: usize = 8_192;

/// ZIP, TAR, GZIP, and Zstandard provider for the isolated worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArchiveProvider;

impl ArchiveProvider {
    /// Constructs the stateless archive provider.
    pub const fn new() -> Self {
        Self
    }
}

impl FileMediaProvider for ArchiveProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        declaration().unwrap_or_else(|error| {
            eprintln!("archive declaration failed: {error}");
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
            require_active(cancellation)?;
            let length = source.byte_length().get().min(PROBE_BYTES);
            let prefix = ProbePrefix(read_range(source, SourceRange { offset: 0, length }).await?);
            require_active(cancellation)?;
            let prefix_matches = kind.matches_probe(prefix.as_bytes());
            if kind == ArchiveKind::Zip
                && source.byte_length().get() > SOURCE_BYTES
                && prefix_matches
                && !zip_signature_at_start(prefix.as_bytes())
            {
                return Ok(ProcessorProbeOutput::NoMatch);
            }
            if prefix_matches
                || kind == ArchiveKind::Zip && source.byte_length().get() <= SOURCE_BYTES
            {
                let (strength, examined) = if source.byte_length().get() <= SOURCE_BYTES
                    && (kind == ArchiveKind::Zip
                        || prefix_matches && matches!(kind, ArchiveKind::Gzip | ArchiveKind::Zstd))
                {
                    let complete = read_complete_after_prefix(source, prefix).await?;
                    require_active(cancellation)?;
                    let examined = complete.as_bytes().len();
                    let Some(strength) = kind.probe_strength_with_complete_bytes(&complete) else {
                        return Ok(ProcessorProbeOutput::NoMatch);
                    };
                    (strength, examined)
                } else {
                    (
                        kind.probe_strength(prefix.as_bytes()),
                        prefix.as_bytes().len(),
                    )
                };
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(kind.media_type()),
                    strength,
                    evidence_bytes: u64::try_from(examined)
                        .map_err(|_| FileMediaProviderFailure::Failed)?,
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
            require_active(cancellation)?;
            if request.media_type.as_str() != kind.media_type() {
                return Err(FileMediaProviderFailure::Failed);
            }
            let mut complete = None;
            if request.evidence == ValidationEvidence::DeclaredCandidateStructurallyValidated {
                let length = source.byte_length().get().min(PROBE_BYTES);
                let prefix =
                    ProbePrefix(read_range(source, SourceRange { offset: 0, length }).await?);
                require_active(cancellation)?;
                if !kind.matches_probe(prefix.as_bytes()) {
                    if kind != ArchiveKind::Zip || source.byte_length().get() > SOURCE_BYTES {
                        return Ok(ProcessorValidationOutput::NoMatch);
                    }
                    let bytes = read_complete_after_prefix(source, prefix).await?;
                    require_active(cancellation)?;
                    if ZipArchive::new(Cursor::new(bytes.as_bytes())).is_err() {
                        return Ok(ProcessorValidationOutput::NoMatch);
                    }
                    complete = Some(bytes.0);
                } else if source.byte_length().get() <= SOURCE_BYTES {
                    complete = Some(read_complete_after_prefix(source, prefix).await?.0);
                }
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(malformed_validation(kind, SOURCE_SIZE_REASON));
            }
            let bytes = match complete {
                Some(bytes) => bytes,
                None => read_all(source).await?,
            };
            require_active(cancellation)?;
            match enumerate(kind, &bytes) {
                Ok(summary) => validated_output(kind, request.evidence, &summary),
                Err(ArchiveIssue::Encrypted) => Ok(ProcessorValidationOutput::EncryptedOrLocked {
                    media_type: String::from(kind.media_type()),
                }),
                Err(issue) => Ok(malformed_validation(kind, issue.reason())),
            }
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
            require_active(cancellation)?;
            if request.detected_media_type.as_str() != kind.media_type() {
                return Err(FileMediaProviderFailure::Failed);
            }
            match &request.input {
                FileReadInput::Initial { options } if empty_options(options) => {}
                FileReadInput::Initial { .. } => {
                    return Ok(ProcessorReadOutput::InvalidViewArguments);
                }
                FileReadInput::Continuation { .. } => {
                    return Ok(ProcessorReadOutput::UnsupportedView);
                }
            }
            if request.view.as_str() != ENTRIES_VIEW {
                return Ok(ProcessorReadOutput::UnsupportedView);
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: SOURCE_BYTES,
                });
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            match enumerate(kind, &bytes) {
                Ok(summary) => entries_output(kind, &summary),
                Err(ArchiveIssue::Expansion) => Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(EXPANDED_SIZE_REASON),
                }),
                Err(
                    ArchiveIssue::Malformed
                    | ArchiveIssue::Encrypted
                    | ArchiveIssue::EntryCount
                    | ArchiveIssue::HostileName
                    | ArchiveIssue::Link
                    | ArchiveIssue::Recursive
                    | ArchiveIssue::Special
                    | ArchiveIssue::UnsupportedCompression,
                ) => Err(FileMediaProviderFailure::Failed),
            }
        })
    }
}

/// Returns the exact declaration shared by registration and worker composition.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = [
        ArchiveKind::Gzip,
        ArchiveKind::Tar,
        ArchiveKind::Zip,
        ArchiveKind::Zstd,
    ]
    .into_iter()
    .map(|kind| reader_declaration(&provider, kind))
    .collect::<Result<Vec<_>, _>>()?;
    Ok(
        FileMediaProviderDeclaration::try_new_with_container_entries(
            provider,
            readers,
            Some(u64::try_from(MAX_ENTRIES)?),
        )?,
    )
}

fn reader_declaration(
    provider: &FileReaderProviderName,
    kind: ArchiveKind,
) -> Result<ReaderDeclaration, Box<dyn Error>> {
    let entries_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(ENTRIES_VIEW)?,
        String::from("Enumerates bounded hostile-name-safe archive contents without extraction."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming {
            maximum_ranges: READ_RANGES,
        },
        ReadViewBounds::Structured {
            source_bytes: SOURCE_BYTES,
            output_bytes: OUTPUT_BYTES,
            depth: OUTPUT_DEPTH,
            nodes: OUTPUT_NODES,
            string_bytes: OUTPUT_STRING_BYTES,
        },
    )?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(kind.reader())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(kind.media_type())?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: PROBE_BYTES,
            suffix_bytes: 0,
            range_count: 1,
            cumulative_bytes: SOURCE_BYTES,
        }),
        // Validation reads at most two ranges: the bounded signature prefix and
        // the single remainder read that completes the bounded whole source.
        // Their cumulative bytes never exceed the whole-source ceiling.
        validation: ValidationDeclaration::new(SOURCE_BYTES, 2),
        views: vec![entries_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(ENTRY_COUNT_REASON)?,
            ReasonCode::try_new(EXPANDED_SIZE_REASON)?,
            ReasonCode::try_new(HOSTILE_NAME_REASON)?,
            ReasonCode::try_new(LINK_ENTRY_REASON)?,
            ReasonCode::try_new(RECURSIVE_REASON)?,
            ReasonCode::try_new(SPECIAL_ENTRY_REASON)?,
            ReasonCode::try_new(SOURCE_SIZE_REASON)?,
            ReasonCode::try_new(UNSUPPORTED_COMPRESSION_REASON)?,
        ],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveKind {
    Zip,
    Tar,
    Gzip,
    Zstd,
}

impl ArchiveKind {
    const fn reader(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::Tar => "application/x-tar",
            Self::Gzip => "application/gzip",
            Self::Zstd => "application/zstd",
        }
    }

    fn probe_strength(self, bytes: &[u8]) -> ProbeStrength {
        match self {
            Self::Tar => ProbeStrength::StructuralCandidate,
            Self::Zip if !zip_signature_at_start(bytes) => ProbeStrength::StructuralCandidate,
            Self::Zip | Self::Gzip | Self::Zstd => ProbeStrength::Strong,
        }
    }

    fn probe_strength_with_complete_bytes(self, source: &CompleteSource) -> Option<ProbeStrength> {
        let structurally_valid_zip = ZipArchive::new(Cursor::new(source.as_bytes())).is_ok();
        match self {
            Self::Zip if structurally_valid_zip => Some(ProbeStrength::Strong),
            Self::Zip if !zip_signature_at_start(source.probe_prefix()) => None,
            // A ZIP-shaped source demotes a competing claim only once that claim's own
            // format is shown not to be structurally valid. A source that is genuinely
            // both formats keeps both strong claims, so inspection returns the ambiguity
            // the contract requires instead of silently selecting ZIP.
            Self::Gzip if structurally_valid_zip && !structurally_valid_gzip(source.as_bytes()) => {
                Some(ProbeStrength::StructuralCandidate)
            }
            Self::Zstd if structurally_valid_zip && !structurally_valid_zstd(source.as_bytes()) => {
                Some(ProbeStrength::StructuralCandidate)
            }
            Self::Zip | Self::Gzip | Self::Zstd | Self::Tar => {
                Some(self.probe_strength(source.probe_prefix()))
            }
        }
    }

    fn matches_probe(self, bytes: &[u8]) -> bool {
        match self {
            Self::Zip => zip_header(bytes),
            Self::Tar => tar_header(bytes),
            Self::Gzip => bytes.starts_with(b"\x1f\x8b\x08"),
            Self::Zstd => zstd_header(bytes),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntrySummary {
    name: String,
    kind: &'static str,
    expanded_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveSummary {
    entries: Vec<EntrySummary>,
    expanded_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveIssue {
    Malformed,
    Encrypted,
    EntryCount,
    Expansion,
    HostileName,
    Link,
    Recursive,
    Special,
    UnsupportedCompression,
}

impl ArchiveIssue {
    const fn reason(self) -> &'static str {
        match self {
            Self::Malformed | Self::Encrypted => MALFORMED_REASON,
            Self::EntryCount => ENTRY_COUNT_REASON,
            Self::Expansion => EXPANDED_SIZE_REASON,
            Self::HostileName => HOSTILE_NAME_REASON,
            Self::Link => LINK_ENTRY_REASON,
            Self::Recursive => RECURSIVE_REASON,
            Self::Special => SPECIAL_ENTRY_REASON,
            Self::UnsupportedCompression => UNSUPPORTED_COMPRESSION_REASON,
        }
    }
}

fn enumerate(kind: ArchiveKind, bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    match kind {
        ArchiveKind::Zip => enumerate_zip(bytes),
        ArchiveKind::Tar => enumerate_tar(bytes),
        ArchiveKind::Gzip => enumerate_gzip(bytes),
        ArchiveKind::Zstd => enumerate_zstd(bytes),
    }
}

fn enumerate_zip(bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| ArchiveIssue::Malformed)?;
    if zip_central_directory_records(bytes, archive.central_directory_start())? != archive.len() {
        return Err(ArchiveIssue::Malformed);
    }
    if archive.len() > MAX_ENTRIES {
        return Err(ArchiveIssue::EntryCount);
    }
    let mut descriptors = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let file = archive
            .by_index_raw(index)
            .map_err(|_| ArchiveIssue::Malformed)?;
        if file.encrypted() {
            return Err(ArchiveIssue::Encrypted);
        }
        match file.compression() {
            CompressionMethod::Stored | CompressionMethod::Deflated => {}
            _ => return Err(ArchiveIssue::UnsupportedCompression),
        }
        let name = checked_name(file.name().as_bytes())?;
        if is_link(file.unix_mode()) {
            return Err(ArchiveIssue::Link);
        }
        if zip_special(file.unix_mode()) {
            return Err(ArchiveIssue::Special);
        }
        if recursive_name(&name) {
            return Err(ArchiveIssue::Recursive);
        }
        if file.size() > MAX_ENTRY_BYTES {
            return Err(ArchiveIssue::Expansion);
        }
        let is_directory = file.is_dir() || zip_directory_mode(file.unix_mode());
        if is_directory && file.size() != 0 {
            return Err(ArchiveIssue::Special);
        }
        let kind = if is_directory { "directory" } else { "file" };
        descriptors.push((name, kind));
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut total = 0_u64;
    for (index, (name, kind)) in descriptors.into_iter().enumerate() {
        let mut file = archive
            .by_index(index)
            .map_err(|_| ArchiveIssue::Malformed)?;
        let (expanded, recursive) = count_reader(&mut file, MAX_ENTRY_BYTES)?;
        if kind == "directory" && expanded != 0 {
            return Err(ArchiveIssue::Special);
        }
        if recursive {
            return Err(ArchiveIssue::Recursive);
        }
        total = bounded_total(ExpansionAggregation {
            current: total,
            added: expanded,
        })?;
        entries.push(EntrySummary {
            name,
            kind,
            expanded_bytes: expanded,
        });
    }
    Ok(ArchiveSummary {
        entries,
        expanded_bytes: total,
    })
}

/// Counts the central-directory records the source actually carries, walking from the
/// directory start the archive itself resolved.
///
/// `ZipArchive` keys its inventory by entry name, so a central directory that repeats a
/// name keeps only the last record and drops the earlier one before enumeration begins.
/// A record hidden that way is never checked for encryption, links, unsupported
/// compression, recursion, or expansion, and never appears in the reported inventory.
/// A record trailing the archive's own declared count hides identically. Requiring the
/// present record count to equal the enumerated count rejects both.
fn zip_central_directory_records(bytes: &[u8], start: u64) -> Result<usize, ArchiveIssue> {
    let mut offset = usize::try_from(start).map_err(|_| ArchiveIssue::Malformed)?;
    let mut records = 0_usize;
    loop {
        let Some(header) = bytes
            .get(offset..)
            .and_then(|rest| rest.get(..ZIP_CENTRAL_HEADER_BYTES))
        else {
            return Ok(records);
        };
        if !header.starts_with(b"PK\x01\x02") {
            return Ok(records);
        }
        let name = usize::from(u16::from_le_bytes([header[28], header[29]]));
        let extra = usize::from(u16::from_le_bytes([header[30], header[31]]));
        let comment = usize::from(u16::from_le_bytes([header[32], header[33]]));
        offset = offset
            .checked_add(ZIP_CENTRAL_HEADER_BYTES)
            .and_then(|next| next.checked_add(name))
            .and_then(|next| next.checked_add(extra))
            .and_then(|next| next.checked_add(comment))
            .ok_or(ArchiveIssue::Malformed)?;
        records = records.checked_add(1).ok_or(ArchiveIssue::EntryCount)?;
        if records > MAX_ENTRIES {
            return Err(ArchiveIssue::EntryCount);
        }
    }
}

fn enumerate_tar(bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    archive.set_ignore_zeros(true);
    let mut entries = Vec::new();
    let mut total = 0_u64;
    let archive_entries = archive.entries().map_err(|_| ArchiveIssue::Malformed)?;
    for entry in archive_entries {
        if entries.len() >= MAX_ENTRIES {
            return Err(ArchiveIssue::EntryCount);
        }
        let mut entry = entry.map_err(|_| ArchiveIssue::Malformed)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(ArchiveIssue::Link);
        }
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(ArchiveIssue::Special);
        }
        let name = checked_name(&entry.path_bytes())?;
        if recursive_name(&name) {
            return Err(ArchiveIssue::Recursive);
        }
        let declared_size = entry.size();
        if declared_size > MAX_ENTRY_BYTES {
            return Err(ArchiveIssue::Expansion);
        }
        if entry_type.is_dir() && declared_size != 0 {
            return Err(ArchiveIssue::Special);
        }
        let kind = if entry_type.is_dir() {
            "directory"
        } else {
            "file"
        };
        let (expanded, recursive) = if entry_type.is_dir() {
            (0, false)
        } else {
            count_reader(&mut entry, MAX_ENTRY_BYTES)?
        };
        if recursive {
            return Err(ArchiveIssue::Recursive);
        }
        total = bounded_total(ExpansionAggregation {
            current: total,
            added: expanded,
        })?;
        entries.push(EntrySummary {
            name,
            kind,
            expanded_bytes: expanded,
        });
    }
    Ok(ArchiveSummary {
        entries,
        expanded_bytes: total,
    })
}

fn enumerate_gzip(bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    let mut remaining = bytes;
    let mut first_name = None;
    let mut expanded = 0_u64;
    let mut detector = RecursiveDetector::new();
    while !remaining.is_empty() {
        let mut decoder = GzDecoder::new(Cursor::new(remaining));
        let name = match decoder.header().and_then(flate2::GzHeader::filename) {
            Some(name) => checked_name_text(&latin1_name(name))?,
            None => String::from("content"),
        };
        if recursive_name(&name) {
            return Err(ArchiveIssue::Recursive);
        }
        if first_name.is_none() {
            first_name = Some(name);
        }
        let maximum = MAX_ENTRY_BYTES
            .checked_sub(expanded)
            .ok_or(ArchiveIssue::Expansion)?;
        let member_expanded = count_reader_with_detector(&mut decoder, maximum, &mut detector)?;
        expanded = bounded_total(ExpansionAggregation {
            current: expanded,
            added: member_expanded,
        })?;
        let consumed = usize::try_from(decoder.into_inner().position())
            .map_err(|_| ArchiveIssue::Malformed)?;
        if consumed == 0 {
            return Err(ArchiveIssue::Malformed);
        }
        remaining = remaining.get(consumed..).ok_or(ArchiveIssue::Malformed)?;
    }
    if detector.detected() {
        return Err(ArchiveIssue::Recursive);
    }
    let name = first_name.ok_or(ArchiveIssue::Malformed)?;
    Ok(single_stream_summary(name, expanded))
}

fn enumerate_zstd(bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    let mut decoder = zstd_decoder(bytes)?;
    let (expanded, recursive) = count_reader(&mut decoder, MAX_ENTRY_BYTES)?;
    if recursive {
        return Err(ArchiveIssue::Recursive);
    }
    Ok(single_stream_summary(String::from("content"), expanded))
}

fn zstd_decoder(
    bytes: &[u8],
) -> Result<zstd::stream::read::Decoder<'static, std::io::BufReader<&[u8]>>, ArchiveIssue> {
    let mut decoder =
        zstd::stream::read::Decoder::new(bytes).map_err(|_| ArchiveIssue::Malformed)?;
    decoder
        .window_log_max(ZSTD_WINDOW_LOG_MAX)
        .map_err(|_| ArchiveIssue::Malformed)?;
    Ok(decoder)
}

fn single_stream_summary(name: String, expanded: u64) -> ArchiveSummary {
    ArchiveSummary {
        entries: vec![EntrySummary {
            name,
            kind: "file",
            expanded_bytes: expanded,
        }],
        expanded_bytes: expanded,
    }
}

fn count_reader(reader: &mut dyn Read, maximum: u64) -> Result<(u64, bool), ArchiveIssue> {
    let mut detector = RecursiveDetector::new();
    let total = count_reader_with_detector(reader, maximum, &mut detector)?;
    Ok((total, detector.detected()))
}

fn count_reader_with_detector(
    reader: &mut dyn Read,
    maximum: u64,
    detector: &mut RecursiveDetector,
) -> Result<u64, ArchiveIssue> {
    let mut total = 0_u64;
    let mut buffer = [0_u8; DECODE_BUFFER_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| ArchiveIssue::Malformed)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count).map_err(|_| ArchiveIssue::Expansion)?)
            .ok_or(ArchiveIssue::Expansion)?;
        if total > maximum {
            return Err(ArchiveIssue::Expansion);
        }
        detector.observe(&buffer[..count]);
    }
    Ok(total)
}

struct RecursiveDetector {
    complete: Vec<u8>,
}

impl RecursiveDetector {
    fn new() -> Self {
        Self {
            complete: Vec::new(),
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        self.complete.extend_from_slice(bytes);
    }

    fn detected(&self) -> bool {
        ZipArchive::new(Cursor::new(&self.complete)).is_ok()
            || structurally_valid_gzip(&self.complete)
            || structurally_valid_zstd(&self.complete)
            || structurally_valid_tar(&self.complete)
    }
}

fn structurally_valid_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1f\x8b\x08")
        && reader_decode_status(&mut MultiGzDecoder::new(bytes)) != DecodeStatus::Malformed
}

fn structurally_valid_zstd(bytes: &[u8]) -> bool {
    if dictionary_zstd_frames(bytes) {
        return true;
    }
    let Ok(mut decoder) = zstd_decoder(bytes) else {
        return false;
    };
    reader_decode_status(&mut decoder) != DecodeStatus::Malformed
}

// The decoder cannot validate dictionary-dependent payloads without the dictionary.
// zstd-safe owns frame boundaries and dictionary IDs for structural recursion detection.
fn dictionary_zstd_frames(mut bytes: &[u8]) -> bool {
    let mut has_dictionary = false;
    while !bytes.is_empty() {
        let Ok(length) = zstd::zstd_safe::find_frame_compressed_size(bytes) else {
            return false;
        };
        if length == 0 {
            return false;
        }
        has_dictionary |= zstd::zstd_safe::get_dict_id_from_frame(bytes).is_some();
        let Some(remaining) = bytes.get(length..) else {
            return false;
        };
        bytes = remaining;
    }
    has_dictionary
}

fn structurally_valid_tar(bytes: &[u8]) -> bool {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    archive.set_ignore_zeros(true);
    let Ok(entries) = archive.entries() else {
        return false;
    };
    let mut saw_entry = false;
    for entry in entries {
        let Ok(mut entry) = entry else {
            return false;
        };
        saw_entry = true;
        if reader_decode_status(&mut entry) == DecodeStatus::Malformed {
            return false;
        }
    }
    saw_entry || empty_tar(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeStatus {
    Complete,
    LimitExceeded,
    Malformed,
}

// Structural decoding reads past `MAX_ENTRY_BYTES` on purpose: a source whose tail is
// corrupt must still report `Malformed` rather than an entry-size verdict. That overshoot
// is bounded by the aggregate expansion ceiling, because a high-expansion source would
// otherwise spend the worker's whole CPU and wall-clock budget proving a decode this
// adapter has already refused on size.
fn reader_decode_status(reader: &mut dyn Read) -> DecodeStatus {
    let mut total = 0_u64;
    let mut buffer = [0_u8; DECODE_BUFFER_BYTES];
    loop {
        let Ok(count) = reader.read(&mut buffer) else {
            return DecodeStatus::Malformed;
        };
        if count == 0 {
            return if total > MAX_ENTRY_BYTES {
                DecodeStatus::LimitExceeded
            } else {
                DecodeStatus::Complete
            };
        }
        let Ok(count) = u64::try_from(count) else {
            return DecodeStatus::Malformed;
        };
        total = total.saturating_add(count);
        if total > MAX_EXPANDED_BYTES {
            return DecodeStatus::LimitExceeded;
        }
    }
}

struct ExpansionAggregation {
    current: u64,
    added: u64,
}

fn bounded_total(aggregation: ExpansionAggregation) -> Result<u64, ArchiveIssue> {
    let total = aggregation
        .current
        .checked_add(aggregation.added)
        .ok_or(ArchiveIssue::Expansion)?;
    if total > MAX_EXPANDED_BYTES {
        Err(ArchiveIssue::Expansion)
    } else {
        Ok(total)
    }
}

fn checked_name(bytes: &[u8]) -> Result<String, ArchiveIssue> {
    let name = std::str::from_utf8(bytes).map_err(|_| ArchiveIssue::HostileName)?;
    checked_name_text(name)
}

fn checked_name_text(name: &str) -> Result<String, ArchiveIssue> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(char::is_control)
        || name.starts_with('/')
        || name.split('/').any(|part| part == "..")
    {
        return Err(ArchiveIssue::HostileName);
    }
    Ok(String::from(name))
}

fn latin1_name(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn recursive_name(name: &str) -> bool {
    let lowercase = name.to_ascii_lowercase();
    [".zip", ".tar", ".tgz", ".gz", ".zst", ".zstd"]
        .iter()
        .any(|suffix| lowercase.ends_with(suffix))
}

fn zip_header(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|window| window == b"PK\x03\x04" || window == b"PK\x05\x06")
}

fn zip_signature_at_start(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

fn zstd_header(bytes: &[u8]) -> bool {
    let Some(magic) = bytes.get(..4) else {
        return false;
    };
    let magic = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
    magic == 0xfd2f_b528 || (0x184d_2a50..=0x184d_2a5f).contains(&magic)
}

fn tar_header(bytes: &[u8]) -> bool {
    if empty_tar(bytes) || bytes.get(257..262) == Some(b"ustar") {
        return true;
    }
    let Some(block) = bytes.get(..512) else {
        return false;
    };
    let mut header = tar::Header::from_byte_slice(block).clone();
    let Ok(expected) = header.cksum() else {
        return false;
    };
    header.set_cksum();
    header.cksum().is_ok_and(|actual| actual == expected)
}

fn empty_tar(bytes: &[u8]) -> bool {
    bytes
        .get(..1_024)
        .is_some_and(|blocks| blocks.iter().all(|byte| *byte == 0))
}

fn is_link(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

fn zip_directory_mode(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o040_000)
}

fn zip_special(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| {
        let kind = mode & 0o170_000;
        !matches!(kind, 0 | 0o040_000 | 0o100_000 | 0o120_000)
    })
}

fn validated_output(
    kind: ArchiveKind,
    evidence: ValidationEvidence,
    summary: &ArchiveSummary,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    let metadata_json = serde_json::to_string(&serde_json::json!({
        "entries": summary.entries.len(),
        "expanded_bytes": summary.expanded_bytes,
        "format": kind.reader(),
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(kind.media_type()),
        evidence,
        metadata_json,
    })
}

fn entries_output(
    kind: ArchiveKind,
    summary: &ArchiveSummary,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    let entries: Vec<_> = summary
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "expanded_bytes": entry.expanded_bytes,
                "kind": entry.kind,
                "name": entry.name,
            })
        })
        .collect();
    let body_json = serde_json::to_string(&serde_json::json!({
        "entries": entries,
        "expanded_bytes": summary.expanded_bytes,
        "format": kind.reader(),
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)?;
    // Entry names are untrusted, and JSON escaping can double the bytes each one
    // contributes. A bounded inventory of admitted names can therefore still serialize
    // past the declared output bound, so report that as the typed output failure rather
    // than emitting a body the runtime would reject as a processor fault.
    if body_json.len() > OUTPUT_BYTES {
        return Ok(ProcessorReadOutput::OutputUnitTooLarge);
    }
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

async fn read_all(source: &dyn VerifiedBlobSource) -> Result<Vec<u8>, FileMediaProviderFailure> {
    read_range(
        source,
        SourceRange {
            offset: 0,
            length: source.byte_length().get(),
        },
    )
    .await
}

struct ProbePrefix(Vec<u8>);

impl ProbePrefix {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

struct CompleteSource(Vec<u8>);

impl CompleteSource {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn probe_prefix(&self) -> &[u8] {
        &self.0[..self.0.len().min(PREFIX_BYTES)]
    }
}

async fn read_complete_after_prefix(
    source: &dyn VerifiedBlobSource,
    prefix: ProbePrefix,
) -> Result<CompleteSource, FileMediaProviderFailure> {
    let total = usize::try_from(source.byte_length().get())
        .map_err(|_| FileMediaProviderFailure::Failed)?;
    let mut bytes = prefix.0;
    if bytes.len() < total {
        let offset = u64::try_from(bytes.len()).map_err(|_| FileMediaProviderFailure::Failed)?;
        let remaining = source
            .byte_length()
            .get()
            .checked_sub(offset)
            .ok_or(FileMediaProviderFailure::Failed)?;
        bytes.extend_from_slice(
            &read_range(
                source,
                SourceRange {
                    offset,
                    length: remaining,
                },
            )
            .await?,
        );
    }
    if bytes.len() != total {
        return Err(FileMediaProviderFailure::Failed);
    }
    Ok(CompleteSource(bytes))
}

struct SourceRange {
    offset: u64,
    length: u64,
}

async fn read_range(
    source: &dyn VerifiedBlobSource,
    range: SourceRange,
) -> Result<Vec<u8>, FileMediaProviderFailure> {
    let length = NonZeroU64::new(range.length).ok_or(FileMediaProviderFailure::Failed)?;
    source
        .read_range(range.offset, length)
        .await
        .map_err(|_| FileMediaProviderFailure::Failed)
}

fn require_reader(reader: &ReaderIdentity) -> Result<ArchiveKind, FileMediaProviderFailure> {
    if reader.provider().as_str() != PROVIDER_NAME || reader.revision().as_str() != READER_REVISION
    {
        return Err(FileMediaProviderFailure::Failed);
    }
    match reader.reader().as_str() {
        "zip" => Ok(ArchiveKind::Zip),
        "tar" => Ok(ArchiveKind::Tar),
        "gzip" => Ok(ArchiveKind::Gzip),
        "zstd" => Ok(ArchiveKind::Zstd),
        _ => Err(FileMediaProviderFailure::Failed),
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), FileMediaProviderFailure> {
    if cancellation.is_cancelled() {
        Err(FileMediaProviderFailure::Failed)
    } else {
        Ok(())
    }
}

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn malformed_validation(kind: ArchiveKind, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(kind.media_type()),
        reason_code: String::from(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Result};

    use super::{DECODE_BUFFER_BYTES, DecodeStatus, MAX_EXPANDED_BYTES, reader_decode_status};

    /// A well-formed decoder over a high-expansion source: it never fails and never ends,
    /// so only a decode bound can stop it. `produced` records how much it was asked for.
    struct EndlessDecoder {
        produced: u64,
    }

    impl Read for EndlessDecoder {
        fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
            buffer.fill(b'x');
            self.produced = self
                .produced
                .saturating_add(u64::try_from(buffer.len()).expect("buffer length fits in u64"));
            Ok(buffer.len())
        }
    }

    #[test]
    fn structural_decoding_of_an_endless_source_stops_at_the_expansion_ceiling() {
        let mut decoder = EndlessDecoder { produced: 0 };
        let buffer_bytes =
            u64::try_from(DECODE_BUFFER_BYTES).expect("read-buffer size fits in u64");

        let status = reader_decode_status(&mut decoder);

        assert_eq!(status, DecodeStatus::LimitExceeded);
        assert!(decoder.produced <= MAX_EXPANDED_BYTES + buffer_bytes);
    }
}
