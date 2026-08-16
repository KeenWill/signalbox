use std::{error::Error, num::NonZeroU64};

use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};

const SOURCE_BYTES: usize = 256 * 1024;
const MP4_TIMESCALE: u32 = 1_000;
const MP4_DURATION_UNITS: u32 = 5_500;
const WEBM_DURATION_TIMECODE_UNITS: f64 = 5_500.0;
const EXPECTED_DURATION_MILLISECONDS: u64 = 5_500;
const EXPECTED_VIDEO_TRACKS: u64 = 1;

#[derive(Clone, Copy)]
pub enum FixtureKind {
    Mp4,
    Webm,
}

#[derive(Clone, Copy)]
enum ContentProtection {
    Clear,
    Encrypted,
}

impl FixtureKind {
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
        }
    }

    pub const fn container(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
        }
    }

    pub const fn profile(self) -> &'static str {
        match self {
            Self::Mp4 => "isom",
            Self::Webm => "webm",
        }
    }
}

pub struct VideoFixture {
    kind: FixtureKind,
    bytes: Vec<u8>,
}

impl VideoFixture {
    pub fn ordinary_mp4() -> Self {
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS),
        )
    }

    pub fn ordinary_webm() -> Self {
        Self::new(
            FixtureKind::Webm,
            webm_bytes(WEBM_DURATION_TIMECODE_UNITS, ContentProtection::Clear),
        )
    }

    pub fn truncated_mp4() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        bytes.pop();
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn truncated_webm() -> Self {
        let mut bytes = webm_bytes(WEBM_DURATION_TIMECODE_UNITS, ContentProtection::Clear);
        bytes.pop();
        Self::new(FixtureKind::Webm, bytes)
    }

    pub fn encrypted_mp4() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        bytes.extend_from_slice(&mp4_extended_box(*b"sinf", &[]));
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn encrypted_webm() -> Self {
        Self::new(
            FixtureKind::Webm,
            webm_bytes(WEBM_DURATION_TIMECODE_UNITS, ContentProtection::Encrypted),
        )
    }

    pub fn recursive_mp4() -> Self {
        let ftyp = ftyp();
        let inner = mp4_box(*b"moov", &[]);
        let outer = mp4_box(*b"moov", &inner);
        Self::new(FixtureKind::Mp4, [ftyp, outer].concat())
    }

    pub fn recursive_webm() -> Self {
        let header = ebml_header();
        let nested = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[]);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &nested);
        Self::new(FixtureKind::Webm, [header, segment].concat())
    }

    pub fn excessive_mp4_boxes() -> Self {
        let mut bytes = ftyp();
        for _ in 0..10_001 {
            bytes.extend_from_slice(&mp4_box(*b"free", &[]));
        }
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn excessive_webm_elements() -> Self {
        let mut payload = Vec::new();
        for _ in 0..10_001 {
            payload.extend_from_slice(&[0xec, 0x80]);
        }
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &payload);
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn zero_timescale_mp4() -> Self {
        Self::new(FixtureKind::Mp4, mp4_bytes(0, MP4_DURATION_UNITS))
    }

    pub fn nonfinite_duration_webm() -> Self {
        Self::new(
            FixtureKind::Webm,
            webm_bytes(f64::NAN, ContentProtection::Clear),
        )
    }

    pub fn oversized_mp4() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        bytes.resize(SOURCE_BYTES + 1, 0);
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn oversized_webm() -> Self {
        let mut bytes = webm_bytes(WEBM_DURATION_TIMECODE_UNITS, ContentProtection::Clear);
        bytes.resize(SOURCE_BYTES + 1, 0);
        Self::new(FixtureKind::Webm, bytes)
    }

    pub const fn expected_duration_milliseconds(&self) -> u64 {
        EXPECTED_DURATION_MILLISECONDS
    }

    pub const fn expected_video_tracks(&self) -> u64 {
        EXPECTED_VIDEO_TRACKS
    }

    pub const fn expected_container(&self) -> &'static str {
        self.kind.container()
    }

    pub const fn expected_profile(&self) -> &'static str {
        self.kind.profile()
    }

    pub fn into_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::new(self.bytes, self.kind.media_type())
    }

    fn new(kind: FixtureKind, bytes: Vec<u8>) -> Self {
        Self { kind, bytes }
    }
}

fn mp4_bytes(timescale: u32, duration: u32) -> Vec<u8> {
    let mut movie_header = vec![0_u8; 20];
    movie_header[12..16].copy_from_slice(&timescale.to_be_bytes());
    movie_header[16..20].copy_from_slice(&duration.to_be_bytes());
    let mut handler = vec![0_u8; 12];
    handler[8..12].copy_from_slice(b"vide");
    let media = mp4_box(*b"mdia", &mp4_box(*b"hdlr", &handler));
    let track = mp4_box(*b"trak", &media);
    let movie = mp4_box(
        *b"moov",
        &[mp4_box(*b"mvhd", &movie_header), track].concat(),
    );
    [ftyp(), movie].concat()
}

fn ftyp() -> Vec<u8> {
    mp4_box(*b"ftyp", b"isom\0\0\0\0mp42")
}

fn mp4_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = u32::try_from(payload.len() + 8).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(payload.len() + 8);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(&box_type);
    bytes.extend_from_slice(payload);
    bytes
}

fn mp4_extended_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = u64::try_from(payload.len() + 16).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(payload.len() + 16);
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&box_type);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn webm_bytes(duration: f64, protection: ContentProtection) -> Vec<u8> {
    let scale = ebml_element(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
    let duration = ebml_element(&[0x44, 0x89], &duration.to_bits().to_be_bytes());
    let info = ebml_element(&[0x15, 0x49, 0xa9, 0x66], &[scale, duration].concat());
    let track_type = ebml_element(&[0x83], &[1]);
    let encryption = match protection {
        ContentProtection::Clear => Vec::new(),
        ContentProtection::Encrypted => {
            let content_encryption = ebml_element(&[0x50, 0x35], &[]);
            let content_encoding = ebml_element(&[0x62, 0x40], &content_encryption);
            ebml_element(&[0x6d, 0x80], &content_encoding)
        }
    };
    let track_entry = ebml_element(&[0xae], &[track_type, encryption].concat());
    let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
    let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
    [ebml_header(), segment].concat()
}

fn ebml_header() -> Vec<u8> {
    let doc_type = ebml_element(&[0x42, 0x82], b"webm");
    ebml_element(&[0x1a, 0x45, 0xdf, 0xa3], &doc_type)
}

fn ebml_element(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(id.len() + payload.len() + 8);
    bytes.extend_from_slice(id);
    bytes.extend_from_slice(&ebml_size(payload.len()));
    bytes.extend_from_slice(payload);
    bytes
}

fn ebml_size(size: usize) -> Vec<u8> {
    let length = (1_usize..=8)
        .find(|length| {
            let bits = 7 * length;
            u128::try_from(size).is_ok_and(|size| size < (1_u128 << bits) - 1)
        })
        .unwrap_or(8);
    let mut encoded = vec![0_u8; length];
    let mut remaining = size;
    for byte in encoded.iter_mut().rev() {
        *byte = (remaining & 0xff) as u8;
        remaining >>= 8;
    }
    encoded[0] |= 0x80 >> (length - 1);
    encoded
}

#[derive(Clone)]
pub struct MemorySource {
    bytes: Vec<u8>,
    byte_length: NonZeroU64,
    media_type: &'static str,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>, media_type: &'static str) -> Result<Self, Box<dyn Error>> {
        let byte_length = NonZeroU64::new(u64::try_from(bytes.len())?)
            .ok_or("fixture source must be nonempty")?;
        Ok(Self {
            bytes,
            byte_length,
            media_type,
        })
    }

    pub fn unknown(bytes: Vec<u8>) -> Result<Self, Box<dyn Error>> {
        Self::new(bytes, "application/octet-stream")
    }

    pub fn file_use(&self) -> Result<FileUse, Box<dyn Error>> {
        self.file_use_as(self.media_type)
    }

    pub fn file_use_as(&self, media_type: &str) -> Result<FileUse, Box<dyn Error>> {
        Ok(FileUse::new(
            self.digest(),
            self.byte_length,
            AttachmentKind::File,
            DeclaredMediaType::try_new(media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([0x56; 32])
    }

    fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        let outcome = usize::try_from(offset)
            .ok()
            .and_then(|start| {
                usize::try_from(length.get())
                    .ok()
                    .and_then(|length| start.checked_add(length).map(|end| (start, end)))
            })
            .and_then(|(start, end)| self.bytes.get(start..end).map(<[u8]>::to_vec))
            .ok_or(SourceReadError::RangeOutOfBounds);
        Box::pin(async move { outcome })
    }
}
