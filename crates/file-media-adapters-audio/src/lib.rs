//! Isolated adapters for WAV, MP3, FLAC, and Ogg Opus bytes.

mod adapter;
mod source;

use std::{error::Error, str::FromStr};

use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFailure, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeDeclarationInput, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationDeclaration, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "signalbox_audio";
const READER_REVISION: &str = "v1";
const METADATA_VIEW_NAME: &str = "metadata";
/// Hard safety ceiling covering the prefix and two possible exact MP3 reads.
const AUDIO_PROBE_CUMULATIVE_BYTES: u64 = 78;

/// Hard safety ceiling bounding whole-source worker memory while admitting ordinary audio.
pub const MAX_AUDIO_SOURCE_BYTES: u64 = 64 * 1_024 * 1_024;
/// Exact-range budget for one whole-source audio read. Validation and the metadata view both
/// stream the complete source in `MAX_PROCESSOR_FRAME_BYTES / 2` chunks, so the declared envelope
/// must cover `MAX_AUDIO_SOURCE_BYTES` at that granularity.
pub(crate) const AUDIO_WHOLE_SOURCE_RANGES: u32 = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdapterFormat {
    Wav,
    Mp3,
    Flac,
    OggOpus,
}

impl AdapterFormat {
    const ALL: [Self; 4] = [Self::Wav, Self::Mp3, Self::Flac, Self::OggOpus];

    const fn reader_name(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::OggOpus => "ogg_opus",
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::OggOpus => "audio/ogg",
        }
    }

    fn matches_signature(self, prefix: &[u8]) -> bool {
        match self {
            Self::Wav => {
                prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WAVE".as_slice())
            }
            Self::Mp3 => mp3_signature(prefix),
            Self::Flac => prefix.starts_with(b"fLaC"),
            Self::OggOpus => ogg_opus_signature(prefix),
        }
    }
}

fn ogg_opus_signature(prefix: &[u8]) -> bool {
    let Some(header) = prefix.get(..27) else {
        return false;
    };
    if !header.starts_with(b"OggS")
        || header[4] != 0
        || header[5] & 0x02 == 0
        || header[5] & 0x01 != 0
        || header[6..14] != [0; 8]
    {
        return false;
    }
    let segment_count = usize::from(header[26]);
    let Some(packet_offset) = 27_usize.checked_add(segment_count) else {
        return false;
    };
    let Some(lacing) = prefix.get(27..packet_offset) else {
        return false;
    };
    let mut first_packet_length = 0_usize;
    let mut first_packet_complete = false;
    for segment_length in lacing {
        let Some(length) = first_packet_length.checked_add(usize::from(*segment_length)) else {
            return false;
        };
        first_packet_length = length;
        if *segment_length < 255 {
            first_packet_complete = true;
            break;
        }
    }
    if !first_packet_complete || first_packet_length < 19 {
        return false;
    }
    prefix
        .get(packet_offset..packet_offset.saturating_add(19))
        .is_some_and(|common| common.starts_with(b"OpusHead") && common[8] == 1 && common[9] != 0)
}

fn mp3_signature(prefix: &[u8]) -> bool {
    let audio = if prefix.starts_with(b"ID3") {
        let Some(audio_offset) = id3_audio_offset(prefix) else {
            return false;
        };
        let Some(audio) = prefix.get(audio_offset..) else {
            return false;
        };
        audio
    } else {
        prefix
    };
    audio.get(..4).is_some_and(valid_mp3_frame_header)
}

pub(crate) fn id3_tag_layout(bytes: &[u8]) -> Option<(usize, bool)> {
    let header = bytes.get(..10)?;
    let major = header[3];
    let revision = header[4];
    let flags = header[5];
    let legal_flags = match major {
        2 => 0xc0,
        3 => 0xe0,
        4 => 0xf0,
        _ => return None,
    };
    if revision == 0xff
        || flags & !legal_flags != 0
        || header[6..10].iter().any(|byte| byte & 0x80 != 0)
    {
        return None;
    }
    let tag_length = header[6..10].iter().try_fold(0_usize, |length, byte| {
        length.checked_mul(128)?.checked_add(usize::from(*byte))
    })?;
    if flags & 0x40 != 0 && !valid_id3_extended_header(major, tag_length, bytes) {
        return None;
    }
    Some((
        10_usize.checked_add(tag_length)?,
        major == 4 && flags & 0x10 != 0,
    ))
}

fn valid_id3_extended_header(major: u8, tag_length: usize, bytes: &[u8]) -> bool {
    let Some(size_bytes) = bytes
        .get(10..14)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
    else {
        return false;
    };
    match major {
        3 => valid_id3v23_extended_header(tag_length, size_bytes, bytes),
        4 => {
            if size_bytes.iter().any(|byte| byte & 0x80 != 0) {
                return false;
            }
            let Some(size) = size_bytes.iter().try_fold(0_usize, |length, byte| {
                length.checked_mul(128)?.checked_add(usize::from(*byte))
            }) else {
                return false;
            };
            valid_id3v24_extended_header(tag_length, size, bytes)
        }
        _ => false,
    }
}

fn valid_id3v23_extended_header(tag_length: usize, size_bytes: [u8; 4], bytes: &[u8]) -> bool {
    let Ok(size) = usize::try_from(u32::from_be_bytes(size_bytes)) else {
        return false;
    };
    let Some(total_size) = size.checked_add(4) else {
        return false;
    };
    let Some(body) = bytes.get(14..14_usize.saturating_add(size)) else {
        return false;
    };
    let Some(flags) = body
        .get(..2)
        .and_then(|value| <[u8; 2]>::try_from(value).ok())
        .map(u16::from_be_bytes)
    else {
        return false;
    };
    let Some(padding_size) = body
        .get(2..6)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .map(u32::from_be_bytes)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    // CRC verification is not implemented, so reject CRC-bearing tags rather
    // than silently trusting an advertised checksum.
    if flags & 0x8000 != 0 {
        return false;
    }
    let expected_size = 6;
    let Some(content_after_extended_header) = tag_length.checked_sub(total_size) else {
        return false;
    };
    if flags & !0x8000 != 0 || size != expected_size || padding_size > content_after_extended_header
    {
        return false;
    }
    if padding_size == 0 {
        return true;
    }
    let Some(tag_end) = 10_usize.checked_add(tag_length) else {
        return false;
    };
    let Some(padding_start) = tag_end.checked_sub(padding_size) else {
        return false;
    };
    bytes
        .get(padding_start..tag_end)
        .is_none_or(|padding| padding.iter().all(|byte| *byte == 0))
}

fn valid_id3v24_extended_header(tag_length: usize, size: usize, bytes: &[u8]) -> bool {
    if size < 6 || size > tag_length {
        return false;
    }
    let Some(body) = bytes.get(14..10_usize.saturating_add(size)) else {
        return false;
    };
    if body.first() != Some(&1) {
        return false;
    }
    let Some(flags) = body.get(1).copied() else {
        return false;
    };
    if flags & !0x70 != 0 {
        return false;
    }
    let mut fields = &body[2..];
    for (flag, expected_length) in [(0x40, 0_usize), (0x20, 5), (0x10, 1)] {
        if flags & flag == 0 {
            continue;
        }
        if fields.first().copied() != u8::try_from(expected_length).ok() {
            return false;
        }
        let Some(remaining) = fields.get(1_usize.saturating_add(expected_length)..) else {
            return false;
        };
        fields = remaining;
    }
    fields.is_empty()
}

pub(crate) struct Id3Footer<'a> {
    pub(crate) header: &'a [u8],
    pub(crate) footer: &'a [u8],
}

pub(crate) fn valid_id3_footer(input: Id3Footer<'_>) -> bool {
    input.header.len() == 10
        && input.footer.len() == 10
        && input.footer.starts_with(b"3DI")
        && input.footer[3..10] == input.header[3..10]
}

fn id3_audio_offset(bytes: &[u8]) -> Option<usize> {
    let (tag_end, has_footer) = id3_tag_layout(bytes)?;
    if !has_footer {
        return Some(tag_end);
    }
    let footer_end = tag_end.checked_add(10)?;
    if !valid_id3_footer(Id3Footer {
        header: bytes.get(..10)?,
        footer: bytes.get(tag_end..footer_end)?,
    }) {
        return None;
    }
    Some(footer_end)
}

fn valid_mp3_frame_header(bytes: &[u8]) -> bool {
    let version = (bytes[1] >> 3) & 0x03;
    let layer = (bytes[1] >> 1) & 0x03;
    let bitrate = (bytes[2] >> 4) & 0x0f;
    let sample_rate = (bytes[2] >> 2) & 0x03;
    bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && version != 0x01
        && layer != 0x00
        && bitrate != 0x00
        && bitrate != 0x0f
        && sample_rate != 0x03
}

#[cfg(test)]
mod tests {
    use ogg::{PacketWriteEndInfo, PacketWriter};

    use super::{AdapterFormat, id3_tag_layout};

    #[test]
    fn mp3_probe_rejects_aac_adts_header() {
        let aac_adts_header = [0xff, 0xf1, 0x50, 0x80];

        assert!(!AdapterFormat::Mp3.matches_signature(&aac_adts_header));
    }

    #[test]
    fn mp3_probe_rejects_aac_adts_after_id3_metadata() {
        let mut id3_prefixed_aac = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        id3_prefixed_aac.extend_from_slice(&[0xff, 0xf1, 0x50, 0x80]);

        assert!(!AdapterFormat::Mp3.matches_signature(&id3_prefixed_aac));
    }

    #[test]
    fn id3v23_extended_header_rejects_padding_larger_than_the_tag_body() {
        let bytes = [
            b'I', b'D', b'3', 3, 0, 0x40, 0, 0, 0, 10, 0, 0, 0, 6, 0, 0, 0, 0, 0, 1,
        ];

        assert_eq!(id3_tag_layout(&bytes), None);
    }

    #[test]
    fn id3v23_extended_header_rejects_nonzero_declared_padding() {
        let bytes = [
            b'I', b'D', b'3', 3, 0, 0x40, 0, 0, 0, 11, 0, 0, 0, 6, 0, 0, 0, 0, 0, 1, 1,
        ];

        assert_eq!(id3_tag_layout(&bytes), None);
    }

    #[test]
    fn id3v23_extended_header_rejects_an_unverified_crc() {
        let bytes = [
            b'I', b'D', b'3', 3, 0, 0x40, 0, 0, 0, 14, 0, 0, 0, 10, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];

        assert_eq!(id3_tag_layout(&bytes), None);
    }

    #[test]
    fn ogg_opus_probe_rejects_magic_in_a_later_packet() {
        let mut writer = PacketWriter::new(Vec::new());
        writer
            .write_packet(
                b"not-an-opus-identification-packet".to_vec(),
                7,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .expect("first Ogg packet should encode");
        writer
            .write_packet(
                b"codec-version=OpusHead".to_vec(),
                7,
                PacketWriteEndInfo::EndStream,
                0,
            )
            .expect("second Ogg packet should encode");
        let bytes = writer.into_inner();

        assert!(!AdapterFormat::OggOpus.matches_signature(&bytes));
    }

    #[test]
    fn ogg_opus_probe_does_not_join_two_packets_into_an_identification_header() {
        let mut writer = PacketWriter::new(Vec::new());
        writer
            .write_packet(b"OpusHead".to_vec(), 7, PacketWriteEndInfo::NormalPacket, 0)
            .expect("first Ogg packet should encode");
        writer
            .write_packet(
                vec![1, 1, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0],
                7,
                PacketWriteEndInfo::EndStream,
                0,
            )
            .expect("second Ogg packet should encode");
        let bytes = writer.into_inner();

        assert!(!AdapterFormat::OggOpus.matches_signature(&bytes));
    }
}

/// Compiled provider for the four version-one audio readers.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioFamilyProvider;

impl FileMediaProvider for AudioFamilyProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        match audio_family_declaration() {
            Ok(declaration) => declaration,
            Err(_) => std::process::abort(),
        }
    }

    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            let format = format_for_reader(reader)?;
            adapter::probe(format, source, cancellation).await
        })
    }

    fn inspect<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            let format = format_for_reader(reader)?;
            adapter::inspect(format, request, source, cancellation).await
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            let format = format_for_reader(reader)?;
            adapter::read(format, request, source, cancellation).await
        })
    }
}

/// Builds the exact declaration registered by the audio-family worker.
pub fn audio_family_declaration()
-> Result<FileMediaProviderDeclaration, Box<dyn Error + Send + Sync>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = AdapterFormat::ALL
        .into_iter()
        .map(|format| reader(&provider, format))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileMediaProviderDeclaration::try_new(provider, readers)?)
}

fn reader(
    provider: &FileReaderProviderName,
    format: AdapterFormat,
) -> Result<ReaderDeclaration, Box<dyn Error + Send + Sync>> {
    let mut reasons = vec![
        ReasonCode::try_new("malformed_audio")?,
        ReasonCode::try_new("source_too_large")?,
        ReasonCode::try_new("channel_limit_exceeded")?,
        ReasonCode::try_new("sample_rate_limit_exceeded")?,
        ReasonCode::try_new("duration_limit_exceeded")?,
    ];
    if format == AdapterFormat::OggOpus {
        reasons.push(ReasonCode::try_new("unsupported_opus_mapping")?);
    }
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(format.reader_name())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(format.media_type())?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 64,
            suffix_bytes: 0,
            range_count: 2,
            cumulative_bytes: AUDIO_PROBE_CUMULATIVE_BYTES,
        }),
        validation: ValidationDeclaration::new(MAX_AUDIO_SOURCE_BYTES, AUDIO_WHOLE_SOURCE_RANGES),
        views: vec![metadata_view()?],
        reason_codes: reasons,
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?)
}

fn metadata_view() -> Result<ReadViewDeclaration, Box<dyn Error + Send + Sync>> {
    Ok(ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW_NAME)?,
        String::from("Decodes the audio and returns its channel count and sample rate."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming {
            maximum_ranges: AUDIO_WHOLE_SOURCE_RANGES,
        },
        ReadViewBounds::Structured {
            source_bytes: MAX_AUDIO_SOURCE_BYTES,
            output_bytes: 256,
            depth: 2,
            nodes: 8,
            string_bytes: 64,
        },
    )?)
}

fn format_for_reader(reader: &ReaderIdentity) -> Result<AdapterFormat, FileMediaProviderFailure> {
    AdapterFormat::ALL
        .into_iter()
        .find(|format| format.reader_name() == reader.reader().as_str())
        .ok_or(FileMediaProviderFailure::Failed)
}

fn options_are_empty(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}
