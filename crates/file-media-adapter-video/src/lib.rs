//! Bounded MP4 and WebM container metadata inside the supervised worker.

use std::{error::Error, num::NonZeroU64, str::FromStr};

use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "video";
const READER_REVISION: &str = "iso-bmff-ebml-v1";
const METADATA_VIEW: &str = "metadata";
const MALFORMED_REASON: &str = "malformed_video";
const RECURSIVE_REASON: &str = "recursive_container";
const STRUCTURE_REASON: &str = "structure_limit";
const SOURCE_SIZE_REASON: &str = "source_size_limit";
const PROBE_BYTES: u64 = 512;
const SOURCE_BYTES: u64 = 256 * 1024;
const OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 10_000;

const MP4_FTYP: [u8; 4] = *b"ftyp";
const MP4_MOOV: [u8; 4] = *b"moov";
const MP4_MVHD: [u8; 4] = *b"mvhd";
const MP4_TRAK: [u8; 4] = *b"trak";
const MP4_MDIA: [u8; 4] = *b"mdia";
const MP4_HDLR: [u8; 4] = *b"hdlr";

const EBML_HEADER: u64 = 0x1a45dfa3;
const EBML_HEADER_BYTES: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
const EBML_DOCTYPE: u64 = 0x4282;
const EBML_SEGMENT: u64 = 0x18538067;
const EBML_INFO: u64 = 0x1549a966;
const EBML_TIMECODE_SCALE: u64 = 0x2ad7b1;
const EBML_DURATION: u64 = 0x4489;
const EBML_TRACKS: u64 = 0x1654ae6b;
const EBML_TRACK_ENTRY: u64 = 0xae;
const EBML_TRACK_TYPE: u64 = 0x83;
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
                return Err(ProcessorFailure::Protocol);
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(malformed_validation(kind, SOURCE_SIZE_REASON));
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            match parse(kind, &bytes) {
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
            if request.file.detected_media_type().as_str() != kind.media_type() {
                return Err(ProcessorFailure::Protocol);
            }
            if !empty_options(&request.options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            }
            if request.view.as_str() != METADATA_VIEW {
                return Ok(ProcessorReadOutput::UnsupportedView);
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: SOURCE_BYTES,
                });
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            match parse(kind, &bytes) {
                Ok(metadata) => metadata_output(kind, &metadata),
                Err(_) => Err(ProcessorFailure::Failed),
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
        ReadAccessPattern::Streaming,
        ReadViewBounds::Structured {
            source_bytes: SOURCE_BYTES,
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
        probe: ProbeDeclaration::new(PROBE_BYTES, 0, 1, SOURCE_BYTES),
        views: vec![metadata_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(RECURSIVE_REASON)?,
            ReasonCode::try_new(STRUCTURE_REASON)?,
            ReasonCode::try_new(SOURCE_SIZE_REASON)?,
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
            Self::Mp4 => bytes.get(4..8) == Some(MP4_FTYP.as_slice()),
            Self::Webm => bytes.starts_with(&EBML_HEADER_BYTES),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VideoMetadata {
    duration_milliseconds: u64,
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

fn parse(kind: VideoKind, bytes: &[u8]) -> Result<VideoMetadata, VideoIssue> {
    match kind {
        VideoKind::Mp4 => parse_mp4(bytes),
        VideoKind::Webm => parse_webm(bytes),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mp4Scope {
    Root,
    Movie,
    Track,
    Media,
}

#[derive(Debug, Default)]
struct Mp4State {
    nodes: usize,
    movie_seen: bool,
    brand: Option<String>,
    duration_milliseconds: Option<u64>,
    video_tracks: u64,
}

fn parse_mp4(bytes: &[u8]) -> Result<VideoMetadata, VideoIssue> {
    if bytes.get(4..8) != Some(MP4_FTYP.as_slice()) {
        return Err(VideoIssue::Malformed);
    }
    if contains_encryption_box(bytes) {
        return Err(VideoIssue::Encrypted);
    }
    let mut state = Mp4State::default();
    parse_mp4_boxes(bytes, 0, Mp4Scope::Root, &mut state)?;
    let profile = state.brand.ok_or(VideoIssue::Malformed)?;
    let duration_milliseconds = state.duration_milliseconds.ok_or(VideoIssue::Malformed)?;
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
    state: &mut Mp4State,
) -> Result<bool, VideoIssue> {
    if depth > MAX_DEPTH {
        return Err(VideoIssue::Structure);
    }
    let mut cursor = 0_usize;
    let mut video_handler = false;
    while cursor < bytes.len() {
        let (box_type, payload, consumed) = mp4_box_at(bytes, cursor)?;
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
                parse_mp4_boxes(payload, depth + 1, Mp4Scope::Movie, state)?;
            }
            MP4_MOOV => return Err(VideoIssue::Recursive),
            MP4_MVHD if scope == Mp4Scope::Movie => parse_mvhd(payload, state)?,
            MP4_TRAK if scope == Mp4Scope::Movie => {
                if parse_mp4_boxes(payload, depth + 1, Mp4Scope::Track, state)? {
                    state.video_tracks = state
                        .video_tracks
                        .checked_add(1)
                        .ok_or(VideoIssue::Structure)?;
                }
            }
            MP4_MDIA if scope == Mp4Scope::Track => {
                video_handler |= parse_mp4_boxes(payload, depth + 1, Mp4Scope::Media, state)?;
            }
            MP4_HDLR if scope == Mp4Scope::Media => {
                video_handler |= parse_handler(payload)?;
            }
            _ => {}
        }
        cursor = cursor.checked_add(consumed).ok_or(VideoIssue::Structure)?;
    }
    Ok(video_handler)
}

fn mp4_box_at(bytes: &[u8], cursor: usize) -> Result<([u8; 4], &[u8], usize), VideoIssue> {
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
    } else if size32 == 0 {
        (8_usize, bytes.len() - cursor)
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

fn parse_ftyp(payload: &[u8], state: &mut Mp4State) -> Result<(), VideoIssue> {
    if state.brand.is_some() || payload.len() < 8 || !(payload.len() - 8).is_multiple_of(4) {
        return Err(VideoIssue::Malformed);
    }
    let brand = payload.get(..4).ok_or(VideoIssue::Malformed)?;
    let supported =
        supported_mp4_brand(brand) || payload[8..].chunks_exact(4).any(supported_mp4_brand);
    if !supported || !brand.iter().all(u8::is_ascii_graphic) {
        return Err(VideoIssue::Malformed);
    }
    state.brand = Some(String::from_utf8(brand.to_vec()).map_err(|_| VideoIssue::Malformed)?);
    Ok(())
}

fn supported_mp4_brand(brand: &[u8]) -> bool {
    matches!(brand, b"isom" | b"iso2" | b"mp41" | b"mp42" | b"avc1")
}

fn parse_mvhd(payload: &[u8], state: &mut Mp4State) -> Result<(), VideoIssue> {
    if state.duration_milliseconds.is_some() {
        return Err(VideoIssue::Malformed);
    }
    let version = *payload.first().ok_or(VideoIssue::Malformed)?;
    let (timescale, duration) = match version {
        0 => (
            u64::from(read_u32(payload, 12)?),
            u64::from(read_u32(payload, 16)?),
        ),
        1 => (u64::from(read_u32(payload, 20)?), read_u64(payload, 24)?),
        _ => return Err(VideoIssue::Malformed),
    };
    if timescale == 0 || duration == u64::MAX || duration == u64::from(u32::MAX) {
        return Err(VideoIssue::Malformed);
    }
    let milliseconds = u128::from(duration)
        .checked_mul(1000)
        .ok_or(VideoIssue::Structure)?
        / u128::from(timescale);
    state.duration_milliseconds =
        Some(u64::try_from(milliseconds).map_err(|_| VideoIssue::Structure)?);
    Ok(())
}

fn parse_handler(payload: &[u8]) -> Result<bool, VideoIssue> {
    let handler = payload.get(8..12).ok_or(VideoIssue::Malformed)?;
    Ok(handler == b"vide")
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

fn contains_encryption_box(bytes: &[u8]) -> bool {
    bytes.windows(8).any(|window| {
        let size = u32::from_be_bytes([window[0], window[1], window[2], window[3]]);
        size >= 8 && matches!(&window[4..8], b"sinf" | b"encv" | b"enca")
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EbmlScope {
    Root,
    Header,
    Segment,
    Info,
    Tracks,
    TrackEntry,
    ContentEncodings,
    ContentEncoding,
}

#[derive(Debug)]
struct EbmlState {
    nodes: usize,
    header_seen: bool,
    segment_seen: bool,
    doc_type_seen: bool,
    info_seen: bool,
    timecode_scale: u64,
    duration: Option<f64>,
    video_tracks: u64,
}

impl Default for EbmlState {
    fn default() -> Self {
        Self {
            nodes: 0,
            header_seen: false,
            segment_seen: false,
            doc_type_seen: false,
            info_seen: false,
            timecode_scale: 1_000_000,
            duration: None,
            video_tracks: 0,
        }
    }
}

fn parse_webm(bytes: &[u8]) -> Result<VideoMetadata, VideoIssue> {
    if !bytes.starts_with(&EBML_HEADER_BYTES) {
        return Err(VideoIssue::Malformed);
    }
    let mut state = EbmlState::default();
    parse_ebml_scope(bytes, 0, EbmlScope::Root, &mut state)?;
    if !state.header_seen
        || !state.doc_type_seen
        || !state.segment_seen
        || !state.info_seen
        || state.video_tracks == 0
    {
        return Err(VideoIssue::Malformed);
    }
    let duration = state.duration.ok_or(VideoIssue::Malformed)?;
    let milliseconds = duration * state.timecode_scale as f64 / 1_000_000.0;
    if !milliseconds.is_finite() || milliseconds < 0.0 || milliseconds > u64::MAX as f64 {
        return Err(VideoIssue::Malformed);
    }
    Ok(VideoMetadata {
        duration_milliseconds: milliseconds.round() as u64,
        video_tracks: state.video_tracks,
        profile: String::from("webm"),
    })
}

fn parse_ebml_scope(
    bytes: &[u8],
    depth: usize,
    scope: EbmlScope,
    state: &mut EbmlState,
) -> Result<bool, VideoIssue> {
    if depth > MAX_DEPTH {
        return Err(VideoIssue::Structure);
    }
    let mut cursor = 0_usize;
    let mut video_track = false;
    while cursor < bytes.len() {
        let (id, id_bytes, _) = read_ebml_vint(bytes, cursor, false, 4)?;
        let size_offset = cursor.checked_add(id_bytes).ok_or(VideoIssue::Structure)?;
        let (size, size_bytes, unknown) = read_ebml_vint(bytes, size_offset, true, 8)?;
        let payload_offset = size_offset
            .checked_add(size_bytes)
            .ok_or(VideoIssue::Structure)?;
        let payload_end = if unknown {
            if id != EBML_SEGMENT {
                return Err(VideoIssue::Malformed);
            }
            bytes.len()
        } else {
            payload_offset
                .checked_add(usize::try_from(size).map_err(|_| VideoIssue::Structure)?)
                .ok_or(VideoIssue::Structure)?
        };
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
                parse_ebml_scope(payload, depth + 1, EbmlScope::Header, state)?;
            }
            (EBML_SEGMENT, EbmlScope::Root) => {
                if state.segment_seen {
                    return Err(VideoIssue::Recursive);
                }
                state.segment_seen = true;
                parse_ebml_scope(payload, depth + 1, EbmlScope::Segment, state)?;
            }
            (EBML_HEADER, _) | (EBML_SEGMENT, _) => return Err(VideoIssue::Recursive),
            (EBML_INFO, EbmlScope::Segment) => {
                if state.info_seen {
                    return Err(VideoIssue::Malformed);
                }
                state.info_seen = true;
                parse_ebml_scope(payload, depth + 1, EbmlScope::Info, state)?;
            }
            (EBML_TRACKS, EbmlScope::Segment) => {
                parse_ebml_scope(payload, depth + 1, EbmlScope::Tracks, state)?;
            }
            (EBML_TRACK_ENTRY, EbmlScope::Tracks) => {
                if parse_ebml_scope(payload, depth + 1, EbmlScope::TrackEntry, state)? {
                    state.video_tracks = state
                        .video_tracks
                        .checked_add(1)
                        .ok_or(VideoIssue::Structure)?;
                }
            }
            (EBML_CONTENT_ENCODINGS, EbmlScope::TrackEntry) => {
                parse_ebml_scope(payload, depth + 1, EbmlScope::ContentEncodings, state)?;
            }
            (EBML_CONTENT_ENCODING, EbmlScope::ContentEncodings) => {
                parse_ebml_scope(payload, depth + 1, EbmlScope::ContentEncoding, state)?;
            }
            (EBML_CONTENT_ENCRYPTION, EbmlScope::ContentEncoding) => {
                return Err(VideoIssue::Encrypted);
            }
            (EBML_DOCTYPE, EbmlScope::Header) => parse_doc_type(payload, state)?,
            (EBML_TIMECODE_SCALE, EbmlScope::Info) => {
                state.timecode_scale = parse_ebml_uint(payload)?;
                if state.timecode_scale == 0 {
                    return Err(VideoIssue::Malformed);
                }
            }
            (EBML_DURATION, EbmlScope::Info) => parse_ebml_duration(payload, state)?,
            (EBML_TRACK_TYPE, EbmlScope::TrackEntry) => {
                video_track |= parse_ebml_uint(payload)? == 1;
            }
            _ => {}
        }
        cursor = payload_end;
        if unknown {
            break;
        }
    }
    Ok(video_track)
}

fn read_ebml_vint(
    bytes: &[u8],
    offset: usize,
    strip_marker: bool,
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
    let mut value = if strip_marker {
        u64::from(first & (marker - 1))
    } else {
        u64::from(first)
    };
    for byte in &encoded[1..] {
        value = value.checked_shl(8).ok_or(VideoIssue::Structure)? | u64::from(*byte);
    }
    let unknown = strip_marker
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
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(kind.media_type()),
        evidence,
        metadata_json: metadata_json(kind, metadata)?,
    })
}

fn metadata_output(
    kind: VideoKind,
    metadata: &VideoMetadata,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(kind, metadata)?,
        truncated: false,
        cursor: None,
    })
}

fn metadata_json(kind: VideoKind, metadata: &VideoMetadata) -> Result<String, ProcessorFailure> {
    serde_json::to_string(&serde_json::json!({
        "container": kind.reader(),
        "duration_milliseconds": metadata.duration_milliseconds,
        "profile": metadata.profile,
        "video_tracks": metadata.video_tracks,
    }))
    .map_err(|_| ProcessorFailure::Failed)
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

fn require_reader(reader: &ReaderIdentity) -> Result<VideoKind, ProcessorFailure> {
    if reader.provider().as_str() != PROVIDER_NAME || reader.revision().as_str() != READER_REVISION
    {
        return Err(ProcessorFailure::Protocol);
    }
    match reader.reader().as_str() {
        "mp4" => Ok(VideoKind::Mp4),
        "webm" => Ok(VideoKind::Webm),
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

fn malformed_validation(kind: VideoKind, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(kind.media_type()),
        reason_code: String::from(reason),
    }
}
