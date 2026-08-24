use std::{error::Error, num::NonZeroU64};

use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};

const METADATA_BYTES: usize = 256 * 1024;
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
    reported_byte_length: Option<u64>,
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

    pub fn durationless_webm() -> Self {
        Self::new(
            FixtureKind::Webm,
            webm_bytes_with_duration(None, ContentProtection::Clear),
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
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(MP4_TIMESCALE, MP4_DURATION_UNITS, mp4_box(*b"encv", &[])),
        )
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

    pub fn ordinary_large_mp4() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        let source_bytes = METADATA_BYTES + 1024;
        let remaining_box_bytes = source_bytes - bytes.len();
        let remaining_box_bytes = u32::try_from(remaining_box_bytes).unwrap_or(u32::MAX);
        bytes.extend_from_slice(&remaining_box_bytes.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.resize(METADATA_BYTES, 0);
        Self::new(FixtureKind::Mp4, bytes)
            .with_reported_byte_length(u64::try_from(source_bytes).unwrap_or(u64::MAX))
    }

    pub fn large_mp4_with_partial_header_at_metadata_cutoff() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        let filler_payload_bytes = METADATA_BYTES - bytes.len() - 8 - 4;
        bytes.extend_from_slice(&mp4_box(*b"free", &vec![0_u8; filler_payload_bytes]));
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        Self::new(FixtureKind::Mp4, bytes)
            .with_reported_byte_length(u64::try_from(METADATA_BYTES + 8).unwrap_or(u64::MAX))
    }

    pub fn mp4_with_incomplete_header_at_actual_eof() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        let filler_payload_bytes = METADATA_BYTES - bytes.len() - 8 - 4;
        bytes.extend_from_slice(&mp4_box(*b"free", &vec![0_u8; filler_payload_bytes]));
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        Self::new(FixtureKind::Mp4, bytes)
            .with_reported_byte_length(u64::try_from(METADATA_BYTES + 1).unwrap_or(u64::MAX))
    }

    pub fn large_mp4_with_partial_extended_header_at_metadata_cutoff() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        let filler_payload_bytes = METADATA_BYTES - bytes.len() - 8 - 12;
        bytes.extend_from_slice(&mp4_box(*b"free", &vec![0_u8; filler_payload_bytes]));
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&[0_u8; 4]);
        Self::new(FixtureKind::Mp4, bytes)
            .with_reported_byte_length(u64::try_from(METADATA_BYTES + 64).unwrap_or(u64::MAX))
    }

    pub fn large_mp4_with_box_past_source_end() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        let filler_payload_bytes = METADATA_BYTES - bytes.len() - 8 - 8;
        bytes.extend_from_slice(&mp4_box(*b"free", &vec![0_u8; filler_payload_bytes]));
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        Self::new(FixtureKind::Mp4, bytes)
            .with_reported_byte_length(u64::try_from(METADATA_BYTES + 1024).unwrap_or(u64::MAX))
    }

    pub fn header_only_avc1_mp4() -> Self {
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(MP4_TIMESCALE, MP4_DURATION_UNITS, mp4_box(*b"avc1", &[])),
        )
    }

    pub fn unsupported_brand_mp4() -> Self {
        Self::new(FixtureKind::Mp4, mp4_box(*b"ftyp", b"avif\0\0\0\0avif"))
    }

    pub fn matroska_ebml() -> Self {
        let doc_type = ebml_element(&[0x42, 0x82], b"matroska");
        let header = ebml_element(&[0x1a, 0x45, 0xdf, 0xa3], &doc_type);
        Self::new(FixtureKind::Webm, header)
    }

    pub fn clear_mp4_with_encryption_like_payload_bytes() -> Self {
        let payload = [0_u32.to_be_bytes().as_slice(), b"sinf"].concat();
        let bytes = [
            mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS),
            mp4_box(*b"free", &payload),
        ]
        .concat();
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn truncated_mvhd_mp4() -> Self {
        let mut movie_header = vec![0_u8; 20];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let movie = mp4_box(*b"moov", &mp4_box(*b"mvhd", &movie_header));
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn duplicate_timestamp_scale_webm() -> Self {
        let scale = ebml_element(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
        let duration = ebml_element(
            &[0x44, 0x89],
            &WEBM_DURATION_TIMECODE_UNITS.to_bits().to_be_bytes(),
        );
        let info = ebml_element(
            &[0x15, 0x49, 0xa9, 0x66],
            &[scale.clone(), scale, duration].concat(),
        );
        let track_entry = webm_track_entry(ContentProtection::Clear);
        let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn mp4_with_iso6_brand() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        bytes[8..12].copy_from_slice(b"iso6");
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn mp4_with_space_padded_brand() -> Self {
        let mut bytes = mp4_bytes(MP4_TIMESCALE, MP4_DURATION_UNITS);
        bytes[8..12].copy_from_slice(b"M4V ");
        Self::new(FixtureKind::Mp4, bytes)
    }

    pub fn hevc_mp4() -> Self {
        let mut configuration = vec![0_u8; 23];
        configuration[0] = 1;
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(
                MP4_TIMESCALE,
                MP4_DURATION_UNITS,
                visual_sample_entry(*b"hvc1", *b"hvcC", &configuration),
            ),
        )
    }

    pub fn hevc_mp4_with_truncated_configuration() -> Self {
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(
                MP4_TIMESCALE,
                MP4_DURATION_UNITS,
                visual_sample_entry(*b"hvc1", *b"hvcC", &[1]),
            ),
        )
    }

    pub fn avc_mp4_with_truncated_parameter_set() -> Self {
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(
                MP4_TIMESCALE,
                MP4_DURATION_UNITS,
                visual_sample_entry(*b"avc1", *b"avcC", &[1, 0x64, 0, 0x1f, 0xff, 0xe1, 0]),
            ),
        )
    }

    pub fn high_profile_avc_mp4() -> Self {
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(
                MP4_TIMESCALE,
                MP4_DURATION_UNITS,
                visual_sample_entry(
                    *b"avc1",
                    *b"avcC",
                    &[1, 100, 0, 31, 0xff, 0xe0, 0, 0xfc, 0xf8, 0xf8, 0],
                ),
            ),
        )
    }

    pub fn mp4_with_unknown_movie_duration() -> Self {
        Self::new(FixtureKind::Mp4, mp4_bytes(MP4_TIMESCALE, u32::MAX))
    }

    pub fn mp4_with_zero_sized_nested_configuration() -> Self {
        let mut payload = vec![0_u8; 78];
        payload[6..8].copy_from_slice(&1_u16.to_be_bytes());
        payload[24..26].copy_from_slice(&1920_u16.to_be_bytes());
        payload[26..28].copy_from_slice(&1080_u16.to_be_bytes());
        payload[40..42].copy_from_slice(&1_u16.to_be_bytes());
        payload[74..76].copy_from_slice(&24_u16.to_be_bytes());
        payload[76..78].copy_from_slice(&u16::MAX.to_be_bytes());
        payload.extend_from_slice(&0_u32.to_be_bytes());
        payload.extend_from_slice(b"avcC");
        payload.extend_from_slice(&[1, 100, 0, 31, 0xff, 0xe0, 0]);
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_sample_entry(
                MP4_TIMESCALE,
                MP4_DURATION_UNITS,
                mp4_box(*b"avc1", &payload),
            ),
        )
    }

    pub fn webm_with_webvtt_track() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let video_track = webm_track_entry(ContentProtection::Clear);
        let track_number = ebml_element(&[0xd7], &[2]);
        let track_type = ebml_element(&[0x83], &[0x11]);
        let codec_id = ebml_element(&[0x86], b"D_WEBVTT/SUBTITLES");
        let subtitle_track = ebml_element(&[0xae], &[track_number, track_type, codec_id].concat());
        let tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &[video_track, subtitle_track].concat(),
        );
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn mp4_video_track_without_mandatory_headers() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
        let media_information = mp4_box(*b"minf", &sample_table);
        let media = mp4_box(
            *b"mdia",
            &[mp4_box(*b"hdlr", &handler), media_information].concat(),
        );
        let track = mp4_box(*b"trak", &media);
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn mp4_media_with_duplicate_handlers() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
        let media_information = mp4_box(*b"minf", &sample_table);
        let media = mp4_box(
            *b"mdia",
            &[
                mp4_box(*b"hdlr", &handler),
                mp4_box(*b"hdlr", &handler),
                media_information,
            ]
            .concat(),
        );
        let track = mp4_box(*b"trak", &media);
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn mp4_with_duplicate_track_ids() -> Self {
        let first_track = mp4_track(1, avc1_sample_entry());
        let second_track = mp4_track(1, avc1_sample_entry());
        Self::new(
            FixtureKind::Mp4,
            mp4_bytes_with_tracks(&[first_track, second_track]),
        )
    }

    pub fn mp4_with_duplicate_sample_descriptions() -> Self {
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let stsd = mp4_box(*b"stsd", &sample_description);
        let sample_table = mp4_box(*b"stbl", &[stsd.clone(), stsd].concat());
        let media_information = mp4_box(*b"minf", &sample_table);
        let media_header = mp4_box(*b"mdhd", &[0_u8; 24]);
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let media = mp4_box(
            *b"mdia",
            &[media_header, mp4_box(*b"hdlr", &handler), media_information].concat(),
        );
        let track = mp4_box(*b"trak", &[tkhd(1), media].concat());
        Self::new(FixtureKind::Mp4, mp4_bytes_with_tracks(&[track]))
    }

    pub fn webm_with_unsupported_ebml_read_version() -> Self {
        let read_version = ebml_element(&[0x42, 0xf7], &[0xff]);
        Self::new(
            FixtureKind::Webm,
            webm_bytes_with_header(ebml_header_with_extra(&read_version)),
        )
    }

    pub fn webm_with_unsupported_doctype_read_version() -> Self {
        let read_version = ebml_element(&[0x42, 0x85], &[0xff]);
        Self::new(
            FixtureKind::Webm,
            webm_bytes_with_header(ebml_header_with_extra(&read_version)),
        )
    }

    pub fn webm_video_track_with_audio_codec() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let track_number = ebml_element(&[0xd7], &[1]);
        let track_type = ebml_element(&[0x83], &[1]);
        let codec_id = ebml_element(&[0x86], b"A_OPUS");
        let track_entry = ebml_element(&[0xae], &[track_number, track_type, codec_id].concat());
        let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn fragmented_mp4_with_movie_extends_duration() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
        let media_information = mp4_box(*b"minf", &sample_table);
        let media_header = mp4_box(*b"mdhd", &[0_u8; 24]);
        let media = mp4_box(
            *b"mdia",
            &[media_header, mp4_box(*b"hdlr", &handler), media_information].concat(),
        );
        let track_header = tkhd(1);
        let track = mp4_box(*b"trak", &[track_header, media].concat());
        let mut fragment_duration = vec![0_u8; 8];
        fragment_duration[4..8].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let movie_extends = mp4_box(*b"mvex", &mp4_box(*b"mehd", &fragment_duration));
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track, movie_extends].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn mp4_video_track_without_sample_description() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let media = mp4_box(*b"mdia", &mp4_box(*b"hdlr", &handler));
        let track = mp4_box(*b"trak", &media);
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn fragmented_mp4_without_movie_extends_duration() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
        let media_information = mp4_box(*b"minf", &sample_table);
        let media_header = mp4_box(*b"mdhd", &[0_u8; 24]);
        let media = mp4_box(
            *b"mdia",
            &[media_header, mp4_box(*b"hdlr", &handler), media_information].concat(),
        );
        let track_header = tkhd(1);
        let track = mp4_box(*b"trak", &[track_header, media].concat());
        let movie_extends = mp4_box(*b"mvex", &[]);
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track, movie_extends].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn mp4_track_with_split_media_evidence() -> Self {
        let mut movie_header = vec![0_u8; 100];
        movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
        movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
        let mut handler = vec![0_u8; 24];
        handler[8..12].copy_from_slice(b"vide");
        let handler_media = mp4_box(*b"mdia", &mp4_box(*b"hdlr", &handler));
        let mut sample_description = vec![0_u8; 8];
        sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
        sample_description.extend_from_slice(&avc1_sample_entry());
        let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
        let sample_media = mp4_box(*b"mdia", &mp4_box(*b"minf", &sample_table));
        let track = mp4_box(*b"trak", &[handler_media, sample_media].concat());
        let movie = mp4_box(
            *b"moov",
            &[mp4_box(*b"mvhd", &movie_header), track].concat(),
        );
        Self::new(FixtureKind::Mp4, [ftyp(), movie].concat())
    }

    pub fn webm_track_missing_number_and_codec() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let track_type = ebml_element(&[0x83], &[1]);
        let track_entry = ebml_element(&[0xae], &track_type);
        let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn webm_video_track_without_video_settings() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let track_number = ebml_element(&[0xd7], &[1]);
        let track_type = ebml_element(&[0x83], &[1]);
        let codec_id = ebml_element(&[0x86], b"V_VP9");
        let track_entry = ebml_element(&[0xae], &[track_number, track_type, codec_id].concat());
        let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn webm_with_duplicate_track_numbers() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let first_track = webm_track_entry(ContentProtection::Clear);
        let second_track = webm_track_entry(ContentProtection::Clear);
        let tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &[first_track, second_track].concat(),
        );
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn webm_with_duplicate_tracks_elements() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let first_tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &webm_track_entry_with_number(1, ContentProtection::Clear),
        );
        let second_tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &webm_track_entry_with_number(2, ContentProtection::Clear),
        );
        let segment = ebml_element(
            &[0x18, 0x53, 0x80, 0x67],
            &[info, first_tracks, second_tracks].concat(),
        );
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
    }

    pub fn large_webm_with_partial_header_at_metadata_cutoff() -> Self {
        let header = ebml_header();
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &webm_track_entry(ContentProtection::Clear),
        );
        let fixed_bytes = header.len() + 5 + info.len() + tracks.len() + 1;
        let filler_payload_bytes = METADATA_BYTES - fixed_bytes - 4;
        let filler = ebml_element(&[0xec], &vec![0_u8; filler_payload_bytes]);
        let mut bytes = header;
        bytes.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
        bytes.extend_from_slice(&info);
        bytes.extend_from_slice(&tracks);
        bytes.extend_from_slice(&filler);
        bytes.push(0x1f);
        Self::new(FixtureKind::Webm, bytes)
            .with_reported_byte_length(u64::try_from(METADATA_BYTES + 64).unwrap_or(u64::MAX))
    }

    pub fn webm_with_partial_vint_at_actual_eof() -> Self {
        let mut fixture = Self::large_webm_with_partial_header_at_metadata_cutoff();
        fixture.reported_byte_length = Some(u64::try_from(METADATA_BYTES + 1).unwrap_or(u64::MAX));
        fixture
    }

    pub fn webm_with_declared_one_byte_id_limit() -> Self {
        let maximum_id_length = ebml_element(&[0x42, 0xf2], &[1]);
        Self::new(
            FixtureKind::Webm,
            webm_bytes_with_header(ebml_header_with_extra(&maximum_id_length)),
        )
    }

    pub fn webm_with_unknown_sized_final_cluster() -> Self {
        let info = webm_info(Some(WEBM_DURATION_TIMECODE_UNITS));
        let tracks = ebml_element(
            &[0x16, 0x54, 0xae, 0x6b],
            &webm_track_entry(ContentProtection::Clear),
        );
        let cluster = [vec![0x1f, 0x43, 0xb6, 0x75, 0xff], vec![0xec, 0x80]].concat();
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks, cluster].concat());
        Self::new(FixtureKind::Webm, [ebml_header(), segment].concat())
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
        MemorySource::new_with_reported_length(
            self.bytes,
            self.kind.media_type(),
            self.reported_byte_length,
        )
    }

    fn new(kind: FixtureKind, bytes: Vec<u8>) -> Self {
        Self {
            kind,
            bytes,
            reported_byte_length: None,
        }
    }

    fn with_reported_byte_length(mut self, byte_length: u64) -> Self {
        self.reported_byte_length = Some(byte_length);
        self
    }
}

fn mp4_bytes(timescale: u32, duration: u32) -> Vec<u8> {
    mp4_bytes_with_sample_entry(timescale, duration, avc1_sample_entry())
}

fn avc1_sample_entry() -> Vec<u8> {
    let mut payload = vec![0_u8; 78];
    payload[6..8].copy_from_slice(&1_u16.to_be_bytes());
    payload[24..26].copy_from_slice(&1920_u16.to_be_bytes());
    payload[26..28].copy_from_slice(&1080_u16.to_be_bytes());
    payload[40..42].copy_from_slice(&1_u16.to_be_bytes());
    payload[74..76].copy_from_slice(&24_u16.to_be_bytes());
    payload[76..78].copy_from_slice(&u16::MAX.to_be_bytes());
    payload.extend_from_slice(&mp4_box(*b"avcC", &[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]));
    mp4_box(*b"avc1", &payload)
}

fn visual_sample_entry(
    sample_entry_type: [u8; 4],
    configuration_type: [u8; 4],
    configuration: &[u8],
) -> Vec<u8> {
    let mut payload = vec![0_u8; 78];
    payload[6..8].copy_from_slice(&1_u16.to_be_bytes());
    payload[24..26].copy_from_slice(&1920_u16.to_be_bytes());
    payload[26..28].copy_from_slice(&1080_u16.to_be_bytes());
    payload[40..42].copy_from_slice(&1_u16.to_be_bytes());
    payload[74..76].copy_from_slice(&24_u16.to_be_bytes());
    payload[76..78].copy_from_slice(&u16::MAX.to_be_bytes());
    payload.extend_from_slice(&mp4_box(configuration_type, configuration));
    mp4_box(sample_entry_type, &payload)
}

fn mp4_bytes_with_sample_entry(timescale: u32, duration: u32, sample_entry: Vec<u8>) -> Vec<u8> {
    let mut movie_header = vec![0_u8; 100];
    movie_header[12..16].copy_from_slice(&timescale.to_be_bytes());
    movie_header[16..20].copy_from_slice(&duration.to_be_bytes());
    let mut handler = vec![0_u8; 24];
    handler[8..12].copy_from_slice(b"vide");
    let mut sample_description = vec![0_u8; 8];
    sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
    sample_description.extend_from_slice(&sample_entry);
    let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
    let media_information = mp4_box(*b"minf", &sample_table);
    let media_header = mp4_box(*b"mdhd", &[0_u8; 24]);
    let media = mp4_box(
        *b"mdia",
        &[media_header, mp4_box(*b"hdlr", &handler), media_information].concat(),
    );
    let track_header = tkhd(1);
    let track = mp4_box(*b"trak", &[track_header, media].concat());
    let movie = mp4_box(
        *b"moov",
        &[mp4_box(*b"mvhd", &movie_header), track].concat(),
    );
    [ftyp(), movie].concat()
}

fn mp4_track(track_id: u32, sample_entry: Vec<u8>) -> Vec<u8> {
    let mut handler = vec![0_u8; 24];
    handler[8..12].copy_from_slice(b"vide");
    let mut sample_description = vec![0_u8; 8];
    sample_description[4..8].copy_from_slice(&1_u32.to_be_bytes());
    sample_description.extend_from_slice(&sample_entry);
    let sample_table = mp4_box(*b"stbl", &mp4_box(*b"stsd", &sample_description));
    let media_information = mp4_box(*b"minf", &sample_table);
    let media = mp4_box(
        *b"mdia",
        &[
            mp4_box(*b"mdhd", &[0_u8; 24]),
            mp4_box(*b"hdlr", &handler),
            media_information,
        ]
        .concat(),
    );
    mp4_box(*b"trak", &[tkhd(track_id), media].concat())
}

fn mp4_bytes_with_tracks(tracks: &[Vec<u8>]) -> Vec<u8> {
    let mut movie_header = vec![0_u8; 100];
    movie_header[12..16].copy_from_slice(&MP4_TIMESCALE.to_be_bytes());
    movie_header[16..20].copy_from_slice(&MP4_DURATION_UNITS.to_be_bytes());
    let mut payload = mp4_box(*b"mvhd", &movie_header);
    for track in tracks {
        payload.extend_from_slice(track);
    }
    [ftyp(), mp4_box(*b"moov", &payload)].concat()
}

fn tkhd(track_id: u32) -> Vec<u8> {
    let mut payload = vec![0_u8; 84];
    payload[12..16].copy_from_slice(&track_id.to_be_bytes());
    mp4_box(*b"tkhd", &payload)
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

fn webm_bytes(duration: f64, protection: ContentProtection) -> Vec<u8> {
    webm_bytes_with_duration(Some(duration), protection)
}

fn webm_bytes_with_duration(duration: Option<f64>, protection: ContentProtection) -> Vec<u8> {
    webm_bytes_with_header_and_duration(ebml_header(), duration, protection)
}

fn webm_bytes_with_header(header: Vec<u8>) -> Vec<u8> {
    webm_bytes_with_header_and_duration(
        header,
        Some(WEBM_DURATION_TIMECODE_UNITS),
        ContentProtection::Clear,
    )
}

fn webm_bytes_with_header_and_duration(
    header: Vec<u8>,
    duration: Option<f64>,
    protection: ContentProtection,
) -> Vec<u8> {
    let info = webm_info(duration);
    let track_entry = webm_track_entry(protection);
    let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], &track_entry);
    let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
    [header, segment].concat()
}

fn webm_info(duration: Option<f64>) -> Vec<u8> {
    let scale = ebml_element(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
    let duration = duration
        .map(|duration| ebml_element(&[0x44, 0x89], &duration.to_bits().to_be_bytes()))
        .unwrap_or_default();
    ebml_element(&[0x15, 0x49, 0xa9, 0x66], &[scale, duration].concat())
}

fn webm_track_entry(protection: ContentProtection) -> Vec<u8> {
    webm_track_entry_with_number(1, protection)
}

fn webm_track_entry_with_number(number: u8, protection: ContentProtection) -> Vec<u8> {
    let track_number = ebml_element(&[0xd7], &[number]);
    let track_type = ebml_element(&[0x83], &[1]);
    let codec_id = ebml_element(&[0x86], b"V_VP9");
    let pixel_width = ebml_element(&[0xb0], &1920_u16.to_be_bytes());
    let pixel_height = ebml_element(&[0xba], &1080_u16.to_be_bytes());
    let video = ebml_element(&[0xe0], &[pixel_width, pixel_height].concat());
    let encryption = match protection {
        ContentProtection::Clear => Vec::new(),
        ContentProtection::Encrypted => {
            let content_encryption = ebml_element(&[0x50, 0x35], &[]);
            let content_encoding = ebml_element(&[0x62, 0x40], &content_encryption);
            ebml_element(&[0x6d, 0x80], &content_encoding)
        }
    };
    ebml_element(
        &[0xae],
        &[track_number, track_type, codec_id, video, encryption].concat(),
    )
}

fn ebml_header() -> Vec<u8> {
    ebml_header_with_extra(&[])
}

fn ebml_header_with_extra(extra: &[u8]) -> Vec<u8> {
    let doc_type = ebml_element(&[0x42, 0x82], b"webm");
    ebml_element(
        &[0x1a, 0x45, 0xdf, 0xa3],
        &[doc_type, extra.to_vec()].concat(),
    )
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
    maximum_read_length: Option<u64>,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>, media_type: &'static str) -> Result<Self, Box<dyn Error>> {
        Self::new_with_reported_length(bytes, media_type, None)
    }

    fn new_with_reported_length(
        bytes: Vec<u8>,
        media_type: &'static str,
        reported_byte_length: Option<u64>,
    ) -> Result<Self, Box<dyn Error>> {
        let byte_length =
            NonZeroU64::new(reported_byte_length.unwrap_or(u64::try_from(bytes.len())?))
                .ok_or("fixture source must be nonempty")?;
        Ok(Self {
            bytes,
            byte_length,
            media_type,
            maximum_read_length: None,
        })
    }

    pub fn unknown(bytes: Vec<u8>) -> Result<Self, Box<dyn Error>> {
        Self::new(bytes, "application/octet-stream")
    }

    pub fn with_maximum_read_length(mut self, maximum_read_length: u64) -> Self {
        self.maximum_read_length = Some(maximum_read_length);
        self
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
        let outcome = self
            .maximum_read_length
            .is_none_or(|maximum| length.get() <= maximum)
            .then_some(())
            .and_then(|()| usize::try_from(offset).ok())
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
