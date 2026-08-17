//! Bounded archive enumeration inside the supervised file-media worker, governed by
//! `docs/spec/file-and-media.md`.

use std::{
    error::Error,
    io::{Cursor, Read},
    num::NonZeroU64,
    str::FromStr,
};

use flate2::read::MultiGzDecoder;
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};
use zip::{CompressionMethod, ZipArchive};

const PROVIDER_NAME: &str = "archives";
const READER_REVISION: &str = "zip8-tar04-gz1-zstd013-v1";
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
const UNSUPPORTED_DICTIONARY_REASON: &str = "unsupported_dictionary";
const PROBE_BYTES: u64 = 1_024;
const SOURCE_BYTES: u64 = 256 * 1024;
const MAX_ENTRIES: usize = 1_000;
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 512;
const OUTPUT_BYTES: usize = 768 * 1024;
const PREFIX_BYTES: usize = 1_024;

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
            let bytes = read_range(source, 0, length).await?;
            require_active(cancellation)?;
            if kind.matches_probe(&bytes) {
                let strength = if source.byte_length().get() <= SOURCE_BYTES
                    && (kind == ArchiveKind::Zip
                        || zip_header(&bytes)
                            && matches!(kind, ArchiveKind::Gzip | ArchiveKind::Zstd))
                {
                    let complete = read_all(source).await?;
                    require_active(cancellation)?;
                    kind.probe_strength_with_complete_bytes(&bytes, &complete)
                } else {
                    kind.probe_strength(&bytes)
                };
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(kind.media_type()),
                    strength,
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
                return Err(ProcessorFailure::Protocol);
            }
            if request.evidence == ValidationEvidence::DeclaredCandidateStructurallyValidated {
                let length = source.byte_length().get().min(PROBE_BYTES);
                let prefix = read_range(source, 0, length).await?;
                require_active(cancellation)?;
                if !kind.matches_probe(&prefix) {
                    return Ok(ProcessorValidationOutput::NoMatch);
                }
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(malformed_validation(kind, SOURCE_SIZE_REASON));
            }
            let bytes = read_all(source).await?;
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
                return Err(ProcessorFailure::Protocol);
            }
            if !empty_options(&request.options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
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
                    | ArchiveIssue::UnsupportedCompression
                    | ArchiveIssue::UnsupportedDictionary,
                ) => Err(ProcessorFailure::Failed),
            }
        })
    }
}

/// Returns the exact declaration shared by registration and worker composition.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = [
        ArchiveKind::Zip,
        ArchiveKind::Tar,
        ArchiveKind::Gzip,
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
        ReadAccessPattern::Streaming,
        ReadViewBounds::Structured {
            source_bytes: SOURCE_BYTES,
            output_bytes: OUTPUT_BYTES,
            depth: 5,
            nodes: 5_000,
            string_bytes: 600 * 1024,
        },
    )?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(kind.reader())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(kind.media_type())?],
        probe: ProbeDeclaration::new(PROBE_BYTES, 0, 1, SOURCE_BYTES),
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
            ReasonCode::try_new(UNSUPPORTED_DICTIONARY_REASON)?,
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

    fn probe_strength_with_complete_bytes(self, prefix: &[u8], complete: &[u8]) -> ProbeStrength {
        let structurally_valid_zip = ZipArchive::new(Cursor::new(complete)).is_ok();
        match self {
            Self::Zip if structurally_valid_zip => ProbeStrength::Strong,
            Self::Gzip | Self::Zstd if structurally_valid_zip => ProbeStrength::StructuralCandidate,
            Self::Zip | Self::Gzip | Self::Zstd | Self::Tar => self.probe_strength(prefix),
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
    UnsupportedDictionary,
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
            Self::UnsupportedDictionary => UNSUPPORTED_DICTIONARY_REASON,
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
        if file.is_dir() && file.size() != 0 {
            return Err(ArchiveIssue::Special);
        }
        let kind = if file.is_dir() { "directory" } else { "file" };
        descriptors.push((name, kind));
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut total = 0_u64;
    for (index, (name, kind)) in descriptors.into_iter().enumerate() {
        let mut file = archive
            .by_index(index)
            .map_err(|_| ArchiveIssue::Malformed)?;
        let (expanded, recursive) = if file.is_dir() {
            (0, false)
        } else {
            count_reader(&mut file, MAX_ENTRY_BYTES)?
        };
        if recursive {
            return Err(ArchiveIssue::Recursive);
        }
        total = bounded_total(total, expanded)?;
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
        total = bounded_total(total, expanded)?;
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
    let name = match gzip_name(bytes)? {
        Some(name) => checked_name_text(&latin1_name(name))?,
        None => String::from("content"),
    };
    if recursive_name(&name) {
        return Err(ArchiveIssue::Recursive);
    }
    let mut decoder = MultiGzDecoder::new(bytes);
    let (expanded, recursive) = count_reader(&mut decoder, MAX_EXPANDED_BYTES)?;
    if recursive {
        return Err(ArchiveIssue::Recursive);
    }
    Ok(single_stream_summary(name, expanded))
}

fn enumerate_zstd(bytes: &[u8]) -> Result<ArchiveSummary, ArchiveIssue> {
    if zstd_frames_have_dictionary(bytes)? {
        return Err(ArchiveIssue::UnsupportedDictionary);
    }
    let mut decoder =
        zstd::stream::read::Decoder::new(bytes).map_err(|_| ArchiveIssue::Malformed)?;
    let (expanded, recursive) = count_reader(&mut decoder, MAX_EXPANDED_BYTES)?;
    if recursive {
        return Err(ArchiveIssue::Recursive);
    }
    Ok(single_stream_summary(String::from("content"), expanded))
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
    let mut total = 0_u64;
    let mut detector = RecursiveDetector::new();
    let mut buffer = [0_u8; 8192];
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
    Ok((total, detector.detected()))
}

struct RecursiveDetector {
    prefix: Vec<u8>,
    zip_tail: Vec<u8>,
    zip_detected: bool,
}

impl RecursiveDetector {
    fn new() -> Self {
        Self {
            prefix: Vec::with_capacity(PREFIX_BYTES),
            zip_tail: Vec::with_capacity(3),
            zip_detected: false,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        let remaining = PREFIX_BYTES.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);

        let mut searchable = Vec::with_capacity(self.zip_tail.len() + bytes.len());
        searchable.extend_from_slice(&self.zip_tail);
        searchable.extend_from_slice(bytes);
        self.zip_detected |= zip_header(&searchable);
        let tail_start = searchable.len().saturating_sub(3);
        self.zip_tail.clear();
        self.zip_tail.extend_from_slice(&searchable[tail_start..]);
    }

    fn detected(&self) -> bool {
        self.zip_detected || recursive_bytes(&self.prefix)
    }
}

fn bounded_total(total: u64, added: u64) -> Result<u64, ArchiveIssue> {
    let total = total.checked_add(added).ok_or(ArchiveIssue::Expansion)?;
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

fn recursive_bytes(bytes: &[u8]) -> bool {
    zip_header(bytes)
        || bytes.starts_with(b"\x1f\x8b\x08")
        || zstd_header(bytes)
        || tar_header(bytes)
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

fn zstd_frames_have_dictionary(mut bytes: &[u8]) -> Result<bool, ArchiveIssue> {
    let mut saw_frame = false;
    while !bytes.is_empty() {
        let magic = bytes.get(..4).ok_or(ArchiveIssue::Malformed)?;
        let magic = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
        if (0x184d_2a50..=0x184d_2a5f).contains(&magic) {
            let length = bytes.get(4..8).ok_or(ArchiveIssue::Malformed)?;
            let length = usize::try_from(u32::from_le_bytes([
                length[0], length[1], length[2], length[3],
            ]))
            .map_err(|_| ArchiveIssue::Malformed)?;
            let next = 8_usize.checked_add(length).ok_or(ArchiveIssue::Malformed)?;
            bytes = bytes.get(next..).ok_or(ArchiveIssue::Malformed)?;
            saw_frame = true;
            continue;
        }
        if magic != 0xfd2f_b528 {
            return Err(ArchiveIssue::Malformed);
        }
        if zstd::zstd_safe::get_dict_id_from_frame(bytes).is_some() {
            return Ok(true);
        }
        let length = zstd::zstd_safe::find_frame_compressed_size(bytes)
            .map_err(|_| ArchiveIssue::Malformed)?;
        if length == 0 {
            return Err(ArchiveIssue::Malformed);
        }
        bytes = bytes.get(length..).ok_or(ArchiveIssue::Malformed)?;
        saw_frame = true;
    }
    if saw_frame {
        Ok(false)
    } else {
        Err(ArchiveIssue::Malformed)
    }
}

fn tar_header(bytes: &[u8]) -> bool {
    empty_tar(bytes) || bytes.get(257..262) == Some(b"ustar") || valid_tar_checksum(bytes)
}

fn empty_tar(bytes: &[u8]) -> bool {
    bytes
        .get(..1_024)
        .is_some_and(|blocks| blocks.iter().all(|byte| *byte == 0))
}

fn valid_tar_checksum(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..512) else {
        return false;
    };
    let Some(checksum_bytes) = header.get(148..156) else {
        return false;
    };
    let Some(expected) = parse_tar_octal(checksum_bytes) else {
        return false;
    };
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    expected == actual
}

fn parse_tar_octal(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let digits = text.trim_matches(['\0', ' ']);
    if digits.is_empty() || !digits.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        return None;
    }
    u64::from_str_radix(digits, 8).ok()
}

fn is_link(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

fn zip_special(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| {
        let kind = mode & 0o170_000;
        !matches!(kind, 0 | 0o040_000 | 0o100_000 | 0o120_000)
    })
}

fn gzip_name(bytes: &[u8]) -> Result<Option<&[u8]>, ArchiveIssue> {
    if bytes.len() < 10 || !bytes.starts_with(b"\x1f\x8b\x08") {
        return Err(ArchiveIssue::Malformed);
    }
    let flags = bytes[3];
    if flags & 0b1110_0000 != 0 {
        return Err(ArchiveIssue::Malformed);
    }
    let mut offset = 10_usize;
    if flags & 0x04 != 0 {
        let length_bytes = bytes
            .get(offset..offset + 2)
            .ok_or(ArchiveIssue::Malformed)?;
        let length = usize::from(u16::from_le_bytes([length_bytes[0], length_bytes[1]]));
        offset = offset
            .checked_add(2 + length)
            .ok_or(ArchiveIssue::Malformed)?;
        bytes.get(..offset).ok_or(ArchiveIssue::Malformed)?;
    }
    let name = if flags & 0x08 != 0 {
        let rest = bytes.get(offset..).ok_or(ArchiveIssue::Malformed)?;
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(ArchiveIssue::Malformed)?;
        offset = offset.checked_add(end + 1).ok_or(ArchiveIssue::Malformed)?;
        Some(&rest[..end])
    } else {
        None
    };
    if flags & 0x10 != 0 {
        let rest = bytes.get(offset..).ok_or(ArchiveIssue::Malformed)?;
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(ArchiveIssue::Malformed)?;
        offset = offset.checked_add(end + 1).ok_or(ArchiveIssue::Malformed)?;
    }
    if flags & 0x02 != 0 {
        offset = offset.checked_add(2).ok_or(ArchiveIssue::Malformed)?;
    }
    bytes.get(..offset).ok_or(ArchiveIssue::Malformed)?;
    Ok(name)
}

fn validated_output(
    kind: ArchiveKind,
    evidence: ValidationEvidence,
    summary: &ArchiveSummary,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    let metadata_json = serde_json::to_string(&serde_json::json!({
        "entries": summary.entries.len(),
        "expanded_bytes": summary.expanded_bytes,
        "format": kind.reader(),
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(kind.media_type()),
        evidence,
        metadata_json,
    })
}

fn entries_output(
    kind: ArchiveKind,
    summary: &ArchiveSummary,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
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
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

async fn read_all(source: &dyn VerifiedBlobSource) -> Result<Vec<u8>, ProcessorFailure> {
    read_range(source, 0, source.byte_length().get()).await
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

fn require_reader(reader: &ReaderIdentity) -> Result<ArchiveKind, ProcessorFailure> {
    if reader.provider().as_str() != PROVIDER_NAME || reader.revision().as_str() != READER_REVISION
    {
        return Err(ProcessorFailure::Protocol);
    }
    match reader.reader().as_str() {
        "zip" => Ok(ArchiveKind::Zip),
        "tar" => Ok(ArchiveKind::Tar),
        "gzip" => Ok(ArchiveKind::Gzip),
        "zstd" => Ok(ArchiveKind::Zstd),
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

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn malformed_validation(kind: ArchiveKind, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(kind.media_type()),
        reason_code: String::from(reason),
    }
}
