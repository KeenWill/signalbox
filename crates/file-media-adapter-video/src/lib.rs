//! Bounded MP4 and WebM container metadata inside the supervised worker.

use std::{error::Error, num::NonZeroU64, str::FromStr};

use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration, ProbeStrength,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration, ReaderDeclarationInput,
    ReaderIdentity, ReasonCode, StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "video";
const READER_REVISION: &str = "iso-bmff-ebml-v1";
const METADATA_VIEW: &str = "metadata";
const MALFORMED_REASON: &str = "malformed_video";
const RECURSIVE_REASON: &str = "recursive_container";
const STRUCTURE_REASON: &str = "structure_limit";
const PROBE_BYTES: u64 = 512;
// Effective metadata-work bound shared by validation and reads.
const METADATA_BYTES: u64 = 256 * 1024;
const OUTPUT_BYTES: usize = 16 * 1024;
// Hard safety ceiling: bounds adversarial recursive container descent.
const MAX_DEPTH: usize = 32;
// Hard safety ceiling: bounds CPU spent on attacker-controlled container structure.
const MAX_NODES: usize = 10_000;

const MP4_FTYP: [u8; 4] = *b"ftyp";
const MP4_MOOV: [u8; 4] = *b"moov";
const MP4_MVHD: [u8; 4] = *b"mvhd";
const MP4_TRAK: [u8; 4] = *b"trak";
const MP4_TKHD: [u8; 4] = *b"tkhd";
const MP4_MDIA: [u8; 4] = *b"mdia";
const MP4_MDHD: [u8; 4] = *b"mdhd";
const MP4_HDLR: [u8; 4] = *b"hdlr";
const MP4_MINF: [u8; 4] = *b"minf";
const MP4_STBL: [u8; 4] = *b"stbl";
const MP4_STSD: [u8; 4] = *b"stsd";
const MP4_MVEX: [u8; 4] = *b"mvex";
const MP4_MEHD: [u8; 4] = *b"mehd";

const EBML_HEADER: u64 = 0x1a45dfa3;
const EBML_HEADER_BYTES: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
const EBML_READ_VERSION: u64 = 0x42f7;
const EBML_DOCTYPE: u64 = 0x4282;
const EBML_DOCTYPE_READ_VERSION: u64 = 0x4285;
const EBML_SEGMENT: u64 = 0x18538067;
const EBML_INFO: u64 = 0x1549a966;
const EBML_TIMECODE_SCALE: u64 = 0x2ad7b1;
const EBML_DURATION: u64 = 0x4489;
const EBML_TRACKS: u64 = 0x1654ae6b;
const EBML_TRACK_ENTRY: u64 = 0xae;
const EBML_TRACK_NUMBER: u64 = 0xd7;
const EBML_TRACK_TYPE: u64 = 0x83;
const EBML_CODEC_ID: u64 = 0x86;
const EBML_VIDEO: u64 = 0xe0;
const EBML_PIXEL_WIDTH: u64 = 0xb0;
const EBML_PIXEL_HEIGHT: u64 = 0xba;
const EBML_CLUSTER: u64 = 0x1f43b675;
const EBML_CONTENT_ENCODINGS: u64 = 0x6d80;
const EBML_CONTENT_ENCODING: u64 = 0x6240;
const EBML_CONTENT_ENCRYPTION: u64 = 0x5035;

/// MP4 and WebM metadata provider for the isolated worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoProvider;

impl VideoProvider {
    /// Constructs the stateless video provider.
    pub const fn new() -> Self {
        Self
    }
}

impl FileMediaProvider for VideoProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        declaration().unwrap_or_else(|error| {
            eprintln!("video declaration failed: {error}");
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
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(kind.media_type()),
                    strength: ProbeStrength::Strong,
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
            let (bytes, source_bytes) = read_metadata_prefix(source).await?;
            require_active(cancellation)?;
            if !kind.matches_probe(&bytes) {
                return Ok(ProcessorValidationOutput::NoMatch);
            }
            match parse(kind, &bytes, source_bytes) {
                Ok(metadata) => validated_output(kind, request.evidence, &metadata),
                Err(VideoIssue::Encrypted) => Ok(ProcessorValidationOutput::EncryptedOrLocked {
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
            let FileReadInput::Initial { options } = &request.input else {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            };
            if !empty_options(options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            }
            if request.view.as_str() != METADATA_VIEW {
                return Ok(ProcessorReadOutput::UnsupportedView);
            }
            let (bytes, source_bytes) = read_metadata_prefix(source).await?;
            require_active(cancellation)?;
            match parse(kind, &bytes, source_bytes) {
                Ok(metadata) => metadata_output(kind, &metadata),
                Err(_) => Err(FileMediaProviderFailure::Failed),
            }
        })
    }
}

/// Returns the exact declaration shared by registration and worker composition.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = [VideoKind::Mp4, VideoKind::Webm]
        .into_iter()
        .map(|kind| reader_declaration(&provider, kind))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileMediaProviderDeclaration::try_new(provider, readers)?)
}

fn reader_declaration(
    provider: &FileReaderProviderName,
    kind: VideoKind,
) -> Result<ReaderDeclaration, Box<dyn Error>> {
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded container duration and video-track metadata."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Structured {
            source_bytes: METADATA_BYTES,
            output_bytes: OUTPUT_BYTES,
            depth: 4,
            nodes: 64,
            string_bytes: 1024,
        },
    )?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(kind.reader())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(kind.media_type())?],
        probe: ProbeDeclaration::new(PROBE_BYTES, 0, 1, METADATA_BYTES),
        views: vec![metadata_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(RECURSIVE_REASON)?,
            ReasonCode::try_new(STRUCTURE_REASON)?,
        ],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoKind {
    Mp4,
    Webm,
}

impl VideoKind {
    const fn reader(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
        }
    }

    fn matches_probe(self, bytes: &[u8]) -> bool {
        match self {
            Self::Mp4 => matches_mp4_probe(bytes),
            Self::Webm => matches_webm_probe(bytes),
        }
    }
}

fn matches_mp4_probe(bytes: &[u8]) -> bool {
    let Ok((box_type, payload, _)) = mp4_box_at(bytes, 0, true) else {
        return false;
    };
    box_type == MP4_FTYP && supported_ftyp(payload)
}

fn supported_ftyp(payload: &[u8]) -> bool {
    payload.len() >= 8
        && (payload.len() - 8).is_multiple_of(4)
        && (supported_mp4_brand(&payload[..4])
            || payload[8..].chunks_exact(4).any(supported_mp4_brand))
}

fn matches_webm_probe(bytes: &[u8]) -> bool {
    let Ok((id, id_bytes, _)) = read_ebml_vint(bytes, 0, EbmlVintKind::Identifier, 4) else {
        return false;
    };
    if id != EBML_HEADER {
        return false;
    }
    let Ok((size, size_bytes, unknown)) = read_ebml_vint(bytes, id_bytes, EbmlVintKind::Size, 8)
    else {
        return false;
    };
    if unknown {
        return false;
    }
    let Some(payload_start) = id_bytes.checked_add(size_bytes) else {
        return false;
    };
    let Ok(payload_size) = usize::try_from(size) else {
        return false;
    };
    let Some(payload_end) = payload_start.checked_add(payload_size) else {
        return false;
    };
    let Some(payload) = bytes.get(payload_start..payload_end) else {
        return false;
    };
    ebml_header_has_webm_doc_type(payload)
}

fn ebml_header_has_webm_doc_type(bytes: &[u8]) -> bool {
    let mut cursor = 0_usize;
    let mut doc_type_seen = false;
    while cursor < bytes.len() {
        let Ok((id, id_bytes, _)) = read_ebml_vint(bytes, cursor, EbmlVintKind::Identifier, 4)
        else {
            return false;
        };
        let Some(size_offset) = cursor.checked_add(id_bytes) else {
            return false;
        };
        let Ok((size, size_bytes, unknown)) =
            read_ebml_vint(bytes, size_offset, EbmlVintKind::Size, 8)
        else {
            return false;
        };
        if unknown {
            return false;
        }
        let Some(payload_offset) = size_offset.checked_add(size_bytes) else {
            return false;
        };
        let Ok(payload_size) = usize::try_from(size) else {
            return false;
        };
        let Some(payload_end) = payload_offset.checked_add(payload_size) else {
            return false;
        };
        let Some(payload) = bytes.get(payload_offset..payload_end) else {
            return false;
        };
        if id == EBML_DOCTYPE {
            if doc_type_seen || payload != b"webm" {
                return false;
            }
            doc_type_seen = true;
        }
        cursor = payload_end;
    }
    doc_type_seen
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VideoMetadata {
    duration_milliseconds: Option<u64>,
    video_tracks: u64,
    profile: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoIssue {
    Malformed,
    Encrypted,
    Recursive,
    Structure,
}

impl VideoIssue {
    const fn reason(self) -> &'static str {
        match self {
            Self::Malformed | Self::Encrypted => MALFORMED_REASON,
            Self::Recursive => RECURSIVE_REASON,
            Self::Structure => STRUCTURE_REASON,
        }
    }
}

fn parse(kind: VideoKind, bytes: &[u8], source_bytes: u64) -> Result<VideoMetadata, VideoIssue> {
    match kind {
        VideoKind::Mp4 => parse_mp4(bytes, source_bytes),
        VideoKind::Webm => parse_webm(bytes, source_bytes),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mp4Scope {
    Root,
    Movie,
    Track,
    Media,
    MediaInformation,
    SampleTable,
    MovieExtends,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoTrackPresence {
    Absent,
    Present,
}

impl VideoTrackPresence {
    fn is_present(self) -> bool {
        self == Self::Present
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Mp4TrackEvidence {
    track_header: bool,
    media_header: bool,
    video_handler: bool,
    sample_description: bool,
}

impl Mp4TrackEvidence {
    fn include(&mut self, other: Self) {
        self.track_header |= other.track_header;
        self.media_header |= other.media_header;
        self.video_handler |= other.video_handler;
        self.sample_description |= other.sample_description;
    }

    const fn is_video_track(self) -> bool {
        self.track_header && self.media_header && self.video_handler && self.sample_description
    }
}

#[derive(Debug, Default)]
struct Mp4State {
    nodes: usize,
    movie_seen: bool,
    movie_header_seen: bool,
    brand: Option<String>,
    movie_timescale: Option<u64>,
    movie_duration: Option<u64>,
    fragment_duration: Option<u64>,
    fragmented: bool,
    video_tracks: u64,
}

fn parse_mp4(bytes: &[u8], source_bytes: u64) -> Result<VideoMetadata, VideoIssue> {
    if bytes.get(4..8) != Some(MP4_FTYP.as_slice()) {
        return Err(VideoIssue::Malformed);
    }
    let mut state = Mp4State::default();
    let prefix_bytes = u64::try_from(bytes.len()).map_err(|_| VideoIssue::Structure)?;
    parse_mp4_boxes(
        bytes,
        0,
        Mp4Scope::Root,
        source_bytes > prefix_bytes,
        source_bytes,
        &mut state,
    )?;
    let profile = state.brand.ok_or(VideoIssue::Malformed)?;
    let timescale = state.movie_timescale.ok_or(VideoIssue::Malformed)?;
    let duration = if state.fragmented && state.movie_duration == Some(0) {
        state.fragment_duration
    } else {
        state.movie_duration
    };
    let duration_milliseconds = duration
        .map(|duration| {
            let milliseconds = u128::from(duration)
                .checked_mul(1000)
                .ok_or(VideoIssue::Structure)?
                / u128::from(timescale);
            u64::try_from(milliseconds).map_err(|_| VideoIssue::Structure)
        })
        .transpose()?;
    if !state.movie_seen || state.video_tracks == 0 {
        return Err(VideoIssue::Malformed);
    }
    Ok(VideoMetadata {
        duration_milliseconds,
        video_tracks: state.video_tracks,
        profile,
    })
}

fn parse_mp4_boxes(
    bytes: &[u8],
    depth: usize,
    scope: Mp4Scope,
    allow_truncated_tail: bool,
    source_bytes: u64,
    state: &mut Mp4State,
) -> Result<Mp4TrackEvidence, VideoIssue> {
    if depth > MAX_DEPTH {
        return Err(VideoIssue::Structure);
    }
    let mut cursor = 0_usize;
    let mut track_evidence = Mp4TrackEvidence::default();
    let mut media_seen = false;
    let mut handler_seen = false;
    let mut track_header_seen = false;
    let mut media_header_seen = false;
    while cursor < bytes.len() {
        let (box_type, payload, consumed) = match mp4_box_at(bytes, cursor, scope == Mp4Scope::Root)
        {
            Ok(parsed) => parsed,
            Err(VideoIssue::Malformed)
                if allow_truncated_tail
                    && mp4_box_is_truncated_prefix(bytes, cursor, source_bytes)? =>
            {
                break;
            }
            Err(issue) => return Err(issue),
        };
        state.nodes = state.nodes.checked_add(1).ok_or(VideoIssue::Structure)?;
        if state.nodes > MAX_NODES {
            return Err(VideoIssue::Structure);
        }
        match box_type {
            MP4_FTYP if scope == Mp4Scope::Root => parse_ftyp(payload, state)?,
            MP4_MOOV if scope == Mp4Scope::Root => {
                if state.movie_seen {
                    return Err(VideoIssue::Recursive);
                }
                state.movie_seen = true;
                parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::Movie,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            MP4_MOOV => return Err(VideoIssue::Recursive),
            MP4_MVHD if scope == Mp4Scope::Movie => parse_mvhd(payload, state)?,
            MP4_TRAK if scope == Mp4Scope::Movie => {
                if parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::Track,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?
                .is_video_track()
                {
                    state.video_tracks = state
                        .video_tracks
                        .checked_add(1)
                        .ok_or(VideoIssue::Structure)?;
                }
            }
            MP4_TKHD if scope == Mp4Scope::Track => {
                if track_header_seen {
                    return Err(VideoIssue::Malformed);
                }
                track_header_seen = true;
                validate_mp4_full_box(payload, 84, 96)?;
                track_evidence.track_header = true;
            }
            MP4_MDIA if scope == Mp4Scope::Track => {
                if media_seen {
                    return Err(VideoIssue::Malformed);
                }
                media_seen = true;
                track_evidence.include(parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::Media,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?);
            }
            MP4_MDHD if scope == Mp4Scope::Media => {
                if media_header_seen {
                    return Err(VideoIssue::Malformed);
                }
                media_header_seen = true;
                validate_mp4_full_box(payload, 24, 36)?;
                track_evidence.media_header = true;
            }
            MP4_HDLR if scope == Mp4Scope::Media => {
                if handler_seen {
                    return Err(VideoIssue::Malformed);
                }
                handler_seen = true;
                track_evidence.video_handler = parse_handler(payload)?;
            }
            MP4_MINF if scope == Mp4Scope::Media => {
                track_evidence.include(parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::MediaInformation,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?);
            }
            MP4_STBL if scope == Mp4Scope::MediaInformation => {
                track_evidence.include(parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::SampleTable,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?);
            }
            MP4_STSD if scope == Mp4Scope::SampleTable => {
                track_evidence.sample_description |= parse_stsd(payload, state)?;
            }
            MP4_MVEX if scope == Mp4Scope::Movie => {
                if state.fragmented {
                    return Err(VideoIssue::Malformed);
                }
                state.fragmented = true;
                parse_mp4_boxes(
                    payload,
                    depth + 1,
                    Mp4Scope::MovieExtends,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            MP4_MEHD if scope == Mp4Scope::MovieExtends => parse_mehd(payload, state)?,
            _ => {}
        }
        cursor = cursor.checked_add(consumed).ok_or(VideoIssue::Structure)?;
    }
    Ok(track_evidence)
}

fn mp4_box_at(
    bytes: &[u8],
    cursor: usize,
    allow_zero_size: bool,
) -> Result<([u8; 4], &[u8], usize), VideoIssue> {
    let header = bytes
        .get(cursor..cursor.checked_add(8).ok_or(VideoIssue::Structure)?)
        .ok_or(VideoIssue::Malformed)?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let box_type = [header[4], header[5], header[6], header[7]];
    let (header_bytes, size) = if size32 == 1 {
        let extended = bytes
            .get(cursor + 8..cursor + 16)
            .ok_or(VideoIssue::Malformed)?;
        let size = u64::from_be_bytes([
            extended[0],
            extended[1],
            extended[2],
            extended[3],
            extended[4],
            extended[5],
            extended[6],
            extended[7],
        ]);
        (
            16_usize,
            usize::try_from(size).map_err(|_| VideoIssue::Structure)?,
        )
    } else if size32 == 0 && allow_zero_size {
        (8_usize, bytes.len() - cursor)
    } else if size32 == 0 {
        return Err(VideoIssue::Malformed);
    } else {
        (
            8_usize,
            usize::try_from(size32).map_err(|_| VideoIssue::Structure)?,
        )
    };
    if size < header_bytes {
        return Err(VideoIssue::Malformed);
    }
    let end = cursor.checked_add(size).ok_or(VideoIssue::Structure)?;
    let payload_start = cursor
        .checked_add(header_bytes)
        .ok_or(VideoIssue::Structure)?;
    let payload = bytes.get(payload_start..end).ok_or(VideoIssue::Malformed)?;
    Ok((box_type, payload, size))
}

fn mp4_box_is_truncated_prefix(
    bytes: &[u8],
    cursor: usize,
    source_bytes: u64,
) -> Result<bool, VideoIssue> {
    let prefix_bytes = u64::try_from(bytes.len()).map_err(|_| VideoIssue::Structure)?;
    if source_bytes <= prefix_bytes {
        return Ok(false);
    }
    let Some(header_end) = cursor.checked_add(8) else {
        return Err(VideoIssue::Structure);
    };
    let Some(header) = bytes.get(cursor..header_end) else {
        let required_end = u64::try_from(header_end).map_err(|_| VideoIssue::Structure)?;
        return Ok(source_bytes >= required_end);
    };
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let available = bytes.len() - cursor;
    if size32 == 0 {
        return Ok(true);
    }
    if size32 == 1 {
        let Some(extended_end) = cursor.checked_add(16) else {
            return Err(VideoIssue::Structure);
        };
        let Some(extended) = bytes.get(cursor + 8..extended_end) else {
            let required_end = u64::try_from(extended_end).map_err(|_| VideoIssue::Structure)?;
            return Ok((8..16).contains(&available) && source_bytes >= required_end);
        };
        let size = u64::from_be_bytes([
            extended[0],
            extended[1],
            extended[2],
            extended[3],
            extended[4],
            extended[5],
            extended[6],
            extended[7],
        ]);
        if size < 16 {
            return Ok(false);
        }
        let declared_end = u64::try_from(cursor)
            .map_err(|_| VideoIssue::Structure)?
            .checked_add(size)
            .ok_or(VideoIssue::Structure)?;
        return Ok(
            size > u64::try_from(available).map_err(|_| VideoIssue::Structure)?
                && declared_end <= source_bytes,
        );
    }
    let size = u64::from(size32);
    let declared_end = u64::try_from(cursor)
        .map_err(|_| VideoIssue::Structure)?
        .checked_add(size)
        .ok_or(VideoIssue::Structure)?;
    Ok(size >= 8
        && size > u64::try_from(available).map_err(|_| VideoIssue::Structure)?
        && declared_end <= source_bytes)
}

fn parse_ftyp(payload: &[u8], state: &mut Mp4State) -> Result<(), VideoIssue> {
    if state.brand.is_some() || !supported_ftyp(payload) {
        return Err(VideoIssue::Malformed);
    }
    let major_brand = payload.get(..4).ok_or(VideoIssue::Malformed)?;
    let brand = if supported_mp4_brand(major_brand) {
        major_brand
    } else {
        payload[8..]
            .chunks_exact(4)
            .find(|brand| supported_mp4_brand(brand))
            .ok_or(VideoIssue::Malformed)?
    };
    state.brand = Some(String::from_utf8(brand.to_vec()).map_err(|_| VideoIssue::Malformed)?);
    Ok(())
}

fn supported_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"iso7"
            | b"iso8"
            | b"iso9"
            | b"mp41"
            | b"mp42"
            | b"avc1"
            | b"dash"
            | b"M4V "
            | b"M4VH"
            | b"M4VP"
            | b"F4V "
            | b"F4P "
    )
}

fn parse_mvhd(payload: &[u8], state: &mut Mp4State) -> Result<(), VideoIssue> {
    if state.movie_header_seen {
        return Err(VideoIssue::Malformed);
    }
    state.movie_header_seen = true;
    let version = *payload.first().ok_or(VideoIssue::Malformed)?;
    let (timescale, duration) = match version {
        0 => {
            if payload.len() < 100 {
                return Err(VideoIssue::Malformed);
            }
            let duration = read_u32(payload, 16)?;
            (
                u64::from(read_u32(payload, 12)?),
                (duration != u32::MAX).then_some(u64::from(duration)),
            )
        }
        1 => {
            if payload.len() < 112 {
                return Err(VideoIssue::Malformed);
            }
            let duration = read_u64(payload, 24)?;
            (
                u64::from(read_u32(payload, 20)?),
                (duration != u64::MAX).then_some(duration),
            )
        }
        _ => return Err(VideoIssue::Malformed),
    };
    if timescale == 0 {
        return Err(VideoIssue::Malformed);
    }
    state.movie_timescale = Some(timescale);
    state.movie_duration = duration;
    Ok(())
}

fn parse_mehd(payload: &[u8], state: &mut Mp4State) -> Result<(), VideoIssue> {
    if state.fragment_duration.is_some() {
        return Err(VideoIssue::Malformed);
    }
    let version = *payload.first().ok_or(VideoIssue::Malformed)?;
    let duration = match version {
        0 if payload.len() >= 8 => u64::from(read_u32(payload, 4)?),
        1 if payload.len() >= 12 => read_u64(payload, 4)?,
        _ => return Err(VideoIssue::Malformed),
    };
    if duration == 0 {
        return Err(VideoIssue::Malformed);
    }
    state.fragment_duration = Some(duration);
    Ok(())
}

fn parse_handler(payload: &[u8]) -> Result<bool, VideoIssue> {
    if payload.len() < 24 {
        return Err(VideoIssue::Malformed);
    }
    let handler = payload.get(8..12).ok_or(VideoIssue::Malformed)?;
    Ok(handler == b"vide")
}

fn validate_mp4_full_box(
    payload: &[u8],
    version_zero_bytes: usize,
    version_one_bytes: usize,
) -> Result<(), VideoIssue> {
    let required = match payload.first() {
        Some(0) => version_zero_bytes,
        Some(1) => version_one_bytes,
        _ => return Err(VideoIssue::Malformed),
    };
    if payload.len() < required {
        return Err(VideoIssue::Malformed);
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, VideoIssue> {
    let value = bytes.get(offset..offset + 4).ok_or(VideoIssue::Malformed)?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, VideoIssue> {
    let value = bytes.get(offset..offset + 8).ok_or(VideoIssue::Malformed)?;
    Ok(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn parse_stsd(payload: &[u8], state: &mut Mp4State) -> Result<bool, VideoIssue> {
    if payload.len() < 8 {
        return Err(VideoIssue::Malformed);
    }
    let entry_count = usize::try_from(read_u32(payload, 4)?).map_err(|_| VideoIssue::Structure)?;
    let mut cursor = 8_usize;
    let mut video_sample_entry_seen = false;
    for _ in 0..entry_count {
        let (box_type, entry_payload, consumed) = mp4_box_at(payload, cursor, false)?;
        state.nodes = state.nodes.checked_add(1).ok_or(VideoIssue::Structure)?;
        if state.nodes > MAX_NODES {
            return Err(VideoIssue::Structure);
        }
        if box_type == *b"encv" || box_type == *b"enca" {
            return Err(VideoIssue::Encrypted);
        }
        if let Some(configuration_type) = visual_sample_entry_configuration(box_type) {
            parse_visual_sample_entry(entry_payload, configuration_type, state)?;
            video_sample_entry_seen = true;
        }
        cursor = cursor.checked_add(consumed).ok_or(VideoIssue::Structure)?;
    }
    if cursor != payload.len() {
        return Err(VideoIssue::Malformed);
    }
    Ok(video_sample_entry_seen)
}

fn visual_sample_entry_configuration(box_type: [u8; 4]) -> Option<[u8; 4]> {
    match box_type {
        [b'a', b'v', b'c', b'1'] | [b'a', b'v', b'c', b'3'] => Some(*b"avcC"),
        [b'h', b'v', b'c', b'1'] | [b'h', b'e', b'v', b'1'] => Some(*b"hvcC"),
        [b'a', b'v', b'0', b'1'] => Some(*b"av1C"),
        [b'v', b'p', b'0', b'8'] | [b'v', b'p', b'0', b'9'] => Some(*b"vpcC"),
        _ => None,
    }
}

fn parse_visual_sample_entry(
    payload: &[u8],
    configuration_type: [u8; 4],
    state: &mut Mp4State,
) -> Result<(), VideoIssue> {
    const VISUAL_SAMPLE_ENTRY_BYTES: usize = 78;

    let children = payload
        .get(VISUAL_SAMPLE_ENTRY_BYTES..)
        .ok_or(VideoIssue::Malformed)?;
    let mut cursor = 0_usize;
    let mut configuration_seen = false;
    while cursor < children.len() {
        let (box_type, configuration, consumed) = mp4_box_at(children, cursor, false)?;
        state.nodes = state.nodes.checked_add(1).ok_or(VideoIssue::Structure)?;
        if state.nodes > MAX_NODES {
            return Err(VideoIssue::Structure);
        }
        if box_type == configuration_type {
            if configuration_seen {
                return Err(VideoIssue::Malformed);
            }
            validate_visual_configuration(configuration_type, configuration)?;
            configuration_seen = true;
        }
        cursor = cursor.checked_add(consumed).ok_or(VideoIssue::Structure)?;
    }
    if !configuration_seen {
        return Err(VideoIssue::Malformed);
    }
    Ok(())
}

fn validate_visual_configuration(
    configuration_type: [u8; 4],
    configuration: &[u8],
) -> Result<(), VideoIssue> {
    match configuration_type {
        [b'a', b'v', b'c', b'C'] => {
            validate_avc_configuration(configuration)?;
        }
        [b'h', b'v', b'c', b'C'] => validate_hevc_configuration(configuration)?,
        [b'a', b'v', b'1', b'C'] => {
            if configuration.len() < 4 || configuration[0] != 0x81 {
                return Err(VideoIssue::Malformed);
            }
            if configuration[3] & 0x10 == 0 && configuration[3] & 0x0f != 0 {
                return Err(VideoIssue::Malformed);
            }
        }
        [b'v', b'p', b'c', b'C'] => validate_vp_configuration(configuration)?,
        _ => return Err(VideoIssue::Malformed),
    }
    Ok(())
}

fn validate_avc_configuration(configuration: &[u8]) -> Result<(), VideoIssue> {
    if configuration.len() < 7 || configuration.first() != Some(&1) {
        return Err(VideoIssue::Malformed);
    }
    let sequence_parameter_sets = usize::from(configuration[5] & 0x1f);
    let mut cursor = 6_usize;
    for _ in 0..sequence_parameter_sets {
        cursor = consume_avc_parameter_set(configuration, cursor)?;
    }
    let picture_parameter_sets =
        usize::from(*configuration.get(cursor).ok_or(VideoIssue::Malformed)?);
    cursor = cursor.checked_add(1).ok_or(VideoIssue::Structure)?;
    for _ in 0..picture_parameter_sets {
        cursor = consume_avc_parameter_set(configuration, cursor)?;
    }
    if cursor < configuration.len() {
        cursor = consume_avc_high_profile_extension(configuration, cursor)?;
    }
    if cursor != configuration.len() {
        return Err(VideoIssue::Malformed);
    }
    Ok(())
}

fn consume_avc_high_profile_extension(
    configuration: &[u8],
    cursor: usize,
) -> Result<usize, VideoIssue> {
    if !matches!(
        configuration[1],
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 144
    ) {
        return Err(VideoIssue::Malformed);
    }
    let extension_end = cursor.checked_add(4).ok_or(VideoIssue::Structure)?;
    let extension = configuration
        .get(cursor..extension_end)
        .ok_or(VideoIssue::Malformed)?;
    if extension[0] & 0xfc != 0xfc || extension[1] & 0xf8 != 0xf8 || extension[2] & 0xf8 != 0xf8 {
        return Err(VideoIssue::Malformed);
    }
    let mut cursor = extension_end;
    for _ in 0..usize::from(extension[3]) {
        cursor = consume_avc_parameter_set(configuration, cursor)?;
    }
    Ok(cursor)
}

fn consume_avc_parameter_set(configuration: &[u8], cursor: usize) -> Result<usize, VideoIssue> {
    let length_end = cursor.checked_add(2).ok_or(VideoIssue::Structure)?;
    let length_bytes = configuration
        .get(cursor..length_end)
        .ok_or(VideoIssue::Malformed)?;
    let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
    if length == 0 {
        return Err(VideoIssue::Malformed);
    }
    let end = length_end
        .checked_add(length)
        .ok_or(VideoIssue::Structure)?;
    configuration
        .get(length_end..end)
        .ok_or(VideoIssue::Malformed)?;
    Ok(end)
}

fn validate_hevc_configuration(configuration: &[u8]) -> Result<(), VideoIssue> {
    if configuration.len() < 23 || configuration[0] != 1 {
        return Err(VideoIssue::Malformed);
    }
    let array_count = usize::from(configuration[22]);
    let mut cursor = 23_usize;
    for _ in 0..array_count {
        let array_header = configuration
            .get(cursor..cursor.checked_add(3).ok_or(VideoIssue::Structure)?)
            .ok_or(VideoIssue::Malformed)?;
        let nal_count = usize::from(u16::from_be_bytes([array_header[1], array_header[2]]));
        cursor = cursor.checked_add(3).ok_or(VideoIssue::Structure)?;
        for _ in 0..nal_count {
            let length_bytes = configuration
                .get(cursor..cursor.checked_add(2).ok_or(VideoIssue::Structure)?)
                .ok_or(VideoIssue::Malformed)?;
            let nal_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
            if nal_length == 0 {
                return Err(VideoIssue::Malformed);
            }
            cursor = cursor
                .checked_add(2)
                .and_then(|offset| offset.checked_add(nal_length))
                .ok_or(VideoIssue::Structure)?;
            if cursor > configuration.len() {
                return Err(VideoIssue::Malformed);
            }
        }
    }
    if cursor != configuration.len() {
        return Err(VideoIssue::Malformed);
    }
    Ok(())
}

fn validate_vp_configuration(configuration: &[u8]) -> Result<(), VideoIssue> {
    if configuration.len() < 12 || configuration[0] != 1 || configuration[1..4] != [0, 0, 0] {
        return Err(VideoIssue::Malformed);
    }
    let initialization_size =
        usize::from(u16::from_be_bytes([configuration[10], configuration[11]]));
    let expected_size = 12_usize
        .checked_add(initialization_size)
        .ok_or(VideoIssue::Structure)?;
    if configuration.len() != expected_size {
        return Err(VideoIssue::Malformed);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EbmlScope {
    Root,
    Header,
    Segment,
    Info,
    Tracks,
    TrackEntry,
    Video,
    ContentEncodings,
    ContentEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EbmlVintKind {
    Identifier,
    Size,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EbmlTrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Debug)]
struct EbmlState {
    nodes: usize,
    header_seen: bool,
    segment_seen: bool,
    doc_type_seen: bool,
    ebml_read_version_seen: bool,
    doc_type_read_version_seen: bool,
    info_seen: bool,
    tracks_seen: bool,
    timecode_scale_seen: bool,
    timecode_scale: u64,
    duration: Option<f64>,
    video_tracks: u64,
    track_numbers: Vec<u64>,
}

impl Default for EbmlState {
    fn default() -> Self {
        Self {
            nodes: 0,
            header_seen: false,
            segment_seen: false,
            doc_type_seen: false,
            ebml_read_version_seen: false,
            doc_type_read_version_seen: false,
            info_seen: false,
            tracks_seen: false,
            timecode_scale_seen: false,
            timecode_scale: 1_000_000,
            duration: None,
            video_tracks: 0,
            track_numbers: Vec::new(),
        }
    }
}

fn parse_webm(bytes: &[u8], source_bytes: u64) -> Result<VideoMetadata, VideoIssue> {
    if !bytes.starts_with(&EBML_HEADER_BYTES) {
        return Err(VideoIssue::Malformed);
    }
    let mut state = EbmlState::default();
    let prefix_bytes = u64::try_from(bytes.len()).map_err(|_| VideoIssue::Structure)?;
    parse_ebml_scope(
        bytes,
        0,
        EbmlScope::Root,
        source_bytes > prefix_bytes,
        source_bytes,
        &mut state,
    )?;
    if !state.header_seen
        || !state.doc_type_seen
        || !state.segment_seen
        || !state.info_seen
        || state.video_tracks == 0
    {
        return Err(VideoIssue::Malformed);
    }
    let duration_milliseconds = state
        .duration
        .map(|duration| duration * state.timecode_scale as f64 / 1_000_000.0)
        .map(|milliseconds| {
            if !milliseconds.is_finite() || milliseconds < 0.0 || milliseconds > u64::MAX as f64 {
                return Err(VideoIssue::Malformed);
            }
            Ok(milliseconds.round() as u64)
        })
        .transpose()?;
    Ok(VideoMetadata {
        duration_milliseconds,
        video_tracks: state.video_tracks,
        profile: String::from("webm"),
    })
}

fn parse_ebml_scope(
    bytes: &[u8],
    depth: usize,
    scope: EbmlScope,
    allow_truncated_tail: bool,
    source_bytes: u64,
    state: &mut EbmlState,
) -> Result<VideoTrackPresence, VideoIssue> {
    if depth > MAX_DEPTH {
        return Err(VideoIssue::Structure);
    }
    let mut cursor = 0_usize;
    let mut track_number_seen = false;
    let mut track_type = None;
    let mut codec_kind = None;
    let mut video_settings_seen = false;
    let mut pixel_width_seen = false;
    let mut pixel_height_seen = false;
    while cursor < bytes.len() {
        let (id, id_bytes, _) = match read_ebml_vint(bytes, cursor, EbmlVintKind::Identifier, 4) {
            Ok(parsed) => parsed,
            Err(VideoIssue::Malformed)
                if allow_truncated_tail
                    && scope == EbmlScope::Segment
                    && ebml_vint_extends_beyond(bytes, cursor, 4) =>
            {
                break;
            }
            Err(issue) => return Err(issue),
        };
        let size_offset = cursor.checked_add(id_bytes).ok_or(VideoIssue::Structure)?;
        let (size, size_bytes, unknown) =
            match read_ebml_vint(bytes, size_offset, EbmlVintKind::Size, 8) {
                Ok(parsed) => parsed,
                Err(VideoIssue::Malformed)
                    if allow_truncated_tail
                        && scope == EbmlScope::Segment
                        && ebml_vint_extends_beyond(bytes, size_offset, 8) =>
                {
                    break;
                }
                Err(issue) => return Err(issue),
            };
        let payload_offset = size_offset
            .checked_add(size_bytes)
            .ok_or(VideoIssue::Structure)?;
        let declared_payload_end = if unknown {
            if id != EBML_SEGMENT && !(id == EBML_CLUSTER && scope == EbmlScope::Segment) {
                return Err(VideoIssue::Malformed);
            }
            bytes.len()
        } else {
            payload_offset
                .checked_add(usize::try_from(size).map_err(|_| VideoIssue::Structure)?)
                .ok_or(VideoIssue::Structure)?
        };
        let payload_truncated = declared_payload_end > bytes.len();
        if payload_truncated {
            if u64::try_from(declared_payload_end).map_err(|_| VideoIssue::Structure)?
                > source_bytes
            {
                return Err(VideoIssue::Malformed);
            }
            if allow_truncated_tail && scope == EbmlScope::Segment {
                break;
            }
            if !(allow_truncated_tail && scope == EbmlScope::Root && id == EBML_SEGMENT) {
                return Err(VideoIssue::Malformed);
            }
        }
        let payload_end = declared_payload_end.min(bytes.len());
        let payload = bytes
            .get(payload_offset..payload_end)
            .ok_or(VideoIssue::Malformed)?;
        state.nodes = state.nodes.checked_add(1).ok_or(VideoIssue::Structure)?;
        if state.nodes > MAX_NODES {
            return Err(VideoIssue::Structure);
        }
        match (id, scope) {
            (EBML_HEADER, EbmlScope::Root) => {
                if state.header_seen {
                    return Err(VideoIssue::Recursive);
                }
                state.header_seen = true;
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::Header,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            (EBML_SEGMENT, EbmlScope::Root) => {
                if state.segment_seen {
                    return Err(VideoIssue::Recursive);
                }
                state.segment_seen = true;
                let segment_source_bytes = source_bytes
                    .checked_sub(u64::try_from(payload_offset).map_err(|_| VideoIssue::Structure)?)
                    .ok_or(VideoIssue::Malformed)?;
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::Segment,
                    allow_truncated_tail && (payload_truncated || unknown),
                    segment_source_bytes,
                    state,
                )?;
            }
            (EBML_HEADER, _) | (EBML_SEGMENT, _) => return Err(VideoIssue::Recursive),
            (EBML_INFO, EbmlScope::Segment) => {
                if state.info_seen {
                    return Err(VideoIssue::Malformed);
                }
                state.info_seen = true;
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::Info,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            (EBML_TRACKS, EbmlScope::Segment) => {
                if state.tracks_seen {
                    return Err(VideoIssue::Malformed);
                }
                state.tracks_seen = true;
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::Tracks,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            (EBML_TRACK_ENTRY, EbmlScope::Tracks) => {
                if parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::TrackEntry,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?
                .is_present()
                {
                    state.video_tracks = state
                        .video_tracks
                        .checked_add(1)
                        .ok_or(VideoIssue::Structure)?;
                }
            }
            (EBML_CONTENT_ENCODINGS, EbmlScope::TrackEntry) => {
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::ContentEncodings,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            (EBML_CONTENT_ENCODING, EbmlScope::ContentEncodings) => {
                parse_ebml_scope(
                    payload,
                    depth + 1,
                    EbmlScope::ContentEncoding,
                    false,
                    u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                    state,
                )?;
            }
            (EBML_CONTENT_ENCRYPTION, EbmlScope::ContentEncoding) => {
                return Err(VideoIssue::Encrypted);
            }
            (EBML_DOCTYPE, EbmlScope::Header) => parse_doc_type(payload, state)?,
            (EBML_READ_VERSION, EbmlScope::Header) => {
                if state.ebml_read_version_seen || parse_ebml_uint(payload)? != 1 {
                    return Err(VideoIssue::Malformed);
                }
                state.ebml_read_version_seen = true;
            }
            (EBML_DOCTYPE_READ_VERSION, EbmlScope::Header) => {
                let version = parse_ebml_uint(payload)?;
                if state.doc_type_read_version_seen || version == 0 || version > 2 {
                    return Err(VideoIssue::Malformed);
                }
                state.doc_type_read_version_seen = true;
            }
            (EBML_TIMECODE_SCALE, EbmlScope::Info) => {
                if state.timecode_scale_seen {
                    return Err(VideoIssue::Malformed);
                }
                state.timecode_scale_seen = true;
                state.timecode_scale = parse_ebml_uint(payload)?;
                if state.timecode_scale == 0 {
                    return Err(VideoIssue::Malformed);
                }
            }
            (EBML_DURATION, EbmlScope::Info) => parse_ebml_duration(payload, state)?,
            (EBML_TRACK_TYPE, EbmlScope::TrackEntry) => {
                if track_type.is_some() {
                    return Err(VideoIssue::Malformed);
                }
                track_type = Some(parse_ebml_track_type(payload)?);
            }
            (EBML_TRACK_NUMBER, EbmlScope::TrackEntry) => {
                let track_number = parse_ebml_uint(payload)?;
                if track_number_seen
                    || track_number == 0
                    || state.track_numbers.contains(&track_number)
                {
                    return Err(VideoIssue::Malformed);
                }
                track_number_seen = true;
                state.track_numbers.push(track_number);
            }
            (EBML_CODEC_ID, EbmlScope::TrackEntry) => {
                if codec_kind.is_some() {
                    return Err(VideoIssue::Malformed);
                }
                codec_kind = Some(parse_ebml_codec_kind(payload)?);
            }
            (EBML_VIDEO, EbmlScope::TrackEntry) => {
                if video_settings_seen
                    || !parse_ebml_scope(
                        payload,
                        depth + 1,
                        EbmlScope::Video,
                        false,
                        u64::try_from(payload.len()).map_err(|_| VideoIssue::Structure)?,
                        state,
                    )?
                    .is_present()
                {
                    return Err(VideoIssue::Malformed);
                }
                video_settings_seen = true;
            }
            (EBML_PIXEL_WIDTH, EbmlScope::Video) => {
                if pixel_width_seen || parse_ebml_uint(payload)? == 0 {
                    return Err(VideoIssue::Malformed);
                }
                pixel_width_seen = true;
            }
            (EBML_PIXEL_HEIGHT, EbmlScope::Video) => {
                if pixel_height_seen || parse_ebml_uint(payload)? == 0 {
                    return Err(VideoIssue::Malformed);
                }
                pixel_height_seen = true;
            }
            _ => {}
        }
        cursor = payload_end;
        if unknown {
            break;
        }
    }
    if scope == EbmlScope::TrackEntry {
        let track_type = track_type.ok_or(VideoIssue::Malformed)?;
        let codec_kind = codec_kind.ok_or(VideoIssue::Malformed)?;
        if !track_number_seen
            || (track_type == EbmlTrackKind::Video
                && (codec_kind != EbmlTrackKind::Video || !video_settings_seen))
            || (track_type == EbmlTrackKind::Audio && codec_kind != EbmlTrackKind::Audio)
        {
            return Err(VideoIssue::Malformed);
        }
        return Ok(if track_type == EbmlTrackKind::Video {
            VideoTrackPresence::Present
        } else {
            VideoTrackPresence::Absent
        });
    }
    if scope == EbmlScope::Video {
        if !pixel_width_seen || !pixel_height_seen {
            return Err(VideoIssue::Malformed);
        }
        return Ok(VideoTrackPresence::Present);
    }
    Ok(VideoTrackPresence::Absent)
}

fn ebml_vint_extends_beyond(bytes: &[u8], offset: usize, maximum_bytes: usize) -> bool {
    let Some(first) = bytes.get(offset).copied() else {
        return true;
    };
    if first == 0 {
        return false;
    }
    let Ok(length) = usize::try_from(first.leading_zeros()) else {
        return false;
    };
    let length = length + 1;
    length <= maximum_bytes
        && offset
            .checked_add(length)
            .is_none_or(|end| end > bytes.len())
}

fn read_ebml_vint(
    bytes: &[u8],
    offset: usize,
    kind: EbmlVintKind,
    maximum_bytes: usize,
) -> Result<(u64, usize, bool), VideoIssue> {
    let first = *bytes.get(offset).ok_or(VideoIssue::Malformed)?;
    if first == 0 {
        return Err(VideoIssue::Malformed);
    }
    let length = usize::try_from(first.leading_zeros()).map_err(|_| VideoIssue::Structure)? + 1;
    if length > maximum_bytes {
        return Err(VideoIssue::Malformed);
    }
    let encoded = bytes
        .get(offset..offset + length)
        .ok_or(VideoIssue::Malformed)?;
    let marker = 0x80_u8 >> (length - 1);
    let mut value = if kind == EbmlVintKind::Size {
        u64::from(first & (marker - 1))
    } else {
        u64::from(first)
    };
    for byte in &encoded[1..] {
        value = value.checked_shl(8).ok_or(VideoIssue::Structure)? | u64::from(*byte);
    }
    let unknown = kind == EbmlVintKind::Size
        && encoded[0] & (marker - 1) == marker - 1
        && encoded[1..].iter().all(|byte| *byte == 0xff);
    Ok((value, length, unknown))
}

fn parse_doc_type(payload: &[u8], state: &mut EbmlState) -> Result<(), VideoIssue> {
    if state.doc_type_seen || payload != b"webm" {
        return Err(VideoIssue::Malformed);
    }
    state.doc_type_seen = true;
    Ok(())
}

fn parse_ebml_uint(payload: &[u8]) -> Result<u64, VideoIssue> {
    if payload.is_empty() || payload.len() > 8 {
        return Err(VideoIssue::Malformed);
    }
    let mut value = 0_u64;
    for byte in payload {
        value = value.checked_shl(8).ok_or(VideoIssue::Structure)? | u64::from(*byte);
    }
    Ok(value)
}

fn parse_ebml_track_type(payload: &[u8]) -> Result<EbmlTrackKind, VideoIssue> {
    match parse_ebml_uint(payload)? {
        1 => Ok(EbmlTrackKind::Video),
        2 => Ok(EbmlTrackKind::Audio),
        _ => Ok(EbmlTrackKind::Other),
    }
}

fn parse_ebml_codec_kind(payload: &[u8]) -> Result<EbmlTrackKind, VideoIssue> {
    match payload {
        b"V_VP8" | b"V_VP9" | b"V_AV1" => Ok(EbmlTrackKind::Video),
        b"A_VORBIS" | b"A_OPUS" => Ok(EbmlTrackKind::Audio),
        b"S_TEXT/WEBVTT"
        | b"D_WEBVTT/SUBTITLES"
        | b"D_WEBVTT/CAPTIONS"
        | b"D_WEBVTT/DESCRIPTIONS"
        | b"D_WEBVTT/METADATA" => Ok(EbmlTrackKind::Other),
        _ => Err(VideoIssue::Malformed),
    }
}

fn parse_ebml_duration(payload: &[u8], state: &mut EbmlState) -> Result<(), VideoIssue> {
    if state.duration.is_some() {
        return Err(VideoIssue::Malformed);
    }
    let value = match payload.len() {
        4 => f64::from(f32::from_bits(u32::from_be_bytes([
            payload[0], payload[1], payload[2], payload[3],
        ]))),
        8 => f64::from_bits(u64::from_be_bytes([
            payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
            payload[7],
        ])),
        _ => return Err(VideoIssue::Malformed),
    };
    if !value.is_finite() || value < 0.0 {
        return Err(VideoIssue::Malformed);
    }
    state.duration = Some(value);
    Ok(())
}

fn validated_output(
    kind: VideoKind,
    evidence: ValidationEvidence,
    metadata: &VideoMetadata,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(kind.media_type()),
        evidence,
        metadata_json: metadata_json(kind, metadata)?,
    })
}

fn metadata_output(
    kind: VideoKind,
    metadata: &VideoMetadata,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(kind, metadata)?,
        truncated: false,
        cursor: None,
    })
}

fn metadata_json(
    kind: VideoKind,
    metadata: &VideoMetadata,
) -> Result<String, FileMediaProviderFailure> {
    serde_json::to_string(&serde_json::json!({
        "container": kind.reader(),
        "duration_milliseconds": metadata.duration_milliseconds,
        "profile": metadata.profile,
        "video_tracks": metadata.video_tracks,
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)
}

async fn read_metadata_prefix(
    source: &dyn VerifiedBlobSource,
) -> Result<(Vec<u8>, u64), FileMediaProviderFailure> {
    let source_bytes = source.byte_length().get();
    let length = source_bytes.min(METADATA_BYTES);
    Ok((read_range(source, 0, length).await?, source_bytes))
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

fn require_reader(reader: &ReaderIdentity) -> Result<VideoKind, FileMediaProviderFailure> {
    if reader.provider().as_str() != PROVIDER_NAME || reader.revision().as_str() != READER_REVISION
    {
        return Err(FileMediaProviderFailure::Failed);
    }
    match reader.reader().as_str() {
        "mp4" => Ok(VideoKind::Mp4),
        "webm" => Ok(VideoKind::Webm),
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

fn malformed_validation(kind: VideoKind, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(kind.media_type()),
        reason_code: String::from(reason),
    }
}
