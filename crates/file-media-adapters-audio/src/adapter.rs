use std::{io::Cursor, num::NonZeroU64};

use ogg::PacketReader;
use opus_rs::OpusDecoder;
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderFailure, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReadInput, MAX_AUDIO_CHANNELS, MAX_AUDIO_CLIP_SECONDS,
    MAX_AUDIO_SAMPLE_RATE_HZ, ProbeStrength, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ValidationEvidence, VerifiedBlobSource,
};
use symphonia::{
    core::{
        codecs::audio::AudioDecoderOptions,
        formats::{FormatOptions, FormatReader, TrackType},
        io::{MediaSourceStream, MediaSourceStreamOptions},
    },
    default::formats::{FlacReader, MpaReader, WavReader},
};

use crate::{
    AdapterFormat, Id3Footer, MAX_AUDIO_SOURCE_BYTES, id3_audio_offset, id3_tag_layout,
    options_are_empty, source, valid_id3_footer, valid_mp3_frame_header,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioMetadata {
    channels: usize,
    sample_rate_hz: u32,
}

pub(crate) async fn probe(
    format: AdapterFormat,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, FileMediaProviderFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let matches = if format == AdapterFormat::Mp3 && prefix.starts_with(b"ID3") {
        let Some((tag_end, has_footer)) = id3_tag_layout(&prefix) else {
            return Ok(ProcessorProbeOutput::NoMatch);
        };
        let audio_offset = if has_footer {
            let Some(footer_end) = tag_end.checked_add(10) else {
                return Ok(ProcessorProbeOutput::NoMatch);
            };
            let footer = if let Some(footer) = prefix.get(tag_end..footer_end) {
                footer.to_vec()
            } else {
                let Ok(offset) = u64::try_from(tag_end) else {
                    return Ok(ProcessorProbeOutput::NoMatch);
                };
                let Some(remaining) = source.byte_length().get().checked_sub(offset) else {
                    return Ok(ProcessorProbeOutput::NoMatch);
                };
                if remaining < 10 {
                    return Ok(ProcessorProbeOutput::NoMatch);
                }
                source
                    .read_range(
                        offset,
                        NonZeroU64::new(10).ok_or(FileMediaProviderFailure::Failed)?,
                    )
                    .await
                    .map_err(|_| FileMediaProviderFailure::Failed)?
            };
            if !valid_id3_footer(Id3Footer {
                header: &prefix[..10],
                footer: &footer,
            }) {
                return Ok(ProcessorProbeOutput::NoMatch);
            }
            footer_end
        } else {
            tag_end
        };
        if let Some(header) = prefix.get(audio_offset..audio_offset.saturating_add(4)) {
            valid_mp3_frame_header(header)
        } else {
            let Ok(offset) = u64::try_from(audio_offset) else {
                return Ok(ProcessorProbeOutput::NoMatch);
            };
            let Some(remaining) = source.byte_length().get().checked_sub(offset) else {
                return Ok(ProcessorProbeOutput::NoMatch);
            };
            if remaining < 4 {
                return Ok(ProcessorProbeOutput::NoMatch);
            }
            let header = source
                .read_range(
                    offset,
                    NonZeroU64::new(4).ok_or(FileMediaProviderFailure::Failed)?,
                )
                .await
                .map_err(|_| FileMediaProviderFailure::Failed)?;
            valid_mp3_frame_header(&header)
        }
    } else {
        format.matches_signature(&prefix)
    };
    if matches {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(format.media_type()),
            strength: ProbeStrength::Strong,
        })
    } else {
        Ok(ProcessorProbeOutput::NoMatch)
    }
}

pub(crate) async fn inspect(
    format: AdapterFormat,
    request: FileMediaProviderValidationRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    if request.media_type.as_str() != format.media_type() {
        return Err(FileMediaProviderFailure::Failed);
    }
    let Some(bytes) = source::read_complete(
        source,
        cancellation,
        request.maximum_source_bytes.min(MAX_AUDIO_SOURCE_BYTES),
        request.maximum_ranges,
    )
    .await?
    else {
        return Ok(validation_failure(
            format,
            request.evidence,
            "source_too_large",
        ));
    };
    let metadata = match decode(format, &bytes) {
        Ok(metadata) => metadata,
        Err(reason) => return Ok(validation_failure(format, request.evidence, reason)),
    };
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(format.media_type()),
        evidence: request.evidence,
        metadata_json: metadata_json(metadata)?,
    })
}

pub(crate) async fn read(
    format: AdapterFormat,
    request: FileMediaProviderReadRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    let valid_input = matches!(
        &request.input,
        FileReadInput::Initial { options } if options_are_empty(options)
    );
    if request.view.as_str() != "metadata" || !valid_input {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) =
        source::read_complete(source, cancellation, MAX_AUDIO_SOURCE_BYTES, 512).await?
    else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_AUDIO_SOURCE_BYTES,
        });
    };
    let metadata = decode(format, &bytes).map_err(|_| FileMediaProviderFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(metadata)?,
        truncated: false,
        cursor: None,
    })
}

fn decode(format: AdapterFormat, bytes: &[u8]) -> Result<AudioMetadata, &'static str> {
    match format {
        AdapterFormat::Wav | AdapterFormat::Mp3 | AdapterFormat::Flac => {
            decode_with_symphonia(format, bytes)
        }
        AdapterFormat::OggOpus => decode_ogg_opus(bytes),
    }
}

fn decode_with_symphonia(
    format: AdapterFormat,
    bytes: &[u8],
) -> Result<AudioMetadata, &'static str> {
    let bytes = if format == AdapterFormat::Mp3 {
        mp3_audio_bytes(bytes)?
    } else {
        bytes
    };
    let stream = MediaSourceStream::new(
        Box::new(Cursor::new(bytes.to_vec())),
        MediaSourceStreamOptions::default(),
    );
    let options = FormatOptions::default();
    let mut reader: Box<dyn FormatReader> = match format {
        AdapterFormat::Wav => {
            Box::new(WavReader::try_new(stream, options).map_err(|_| "malformed_audio")?)
        }
        AdapterFormat::Mp3 => {
            Box::new(MpaReader::try_new(stream, options).map_err(|_| "malformed_audio")?)
        }
        AdapterFormat::Flac => {
            Box::new(FlacReader::try_new(stream, options).map_err(|_| "malformed_audio")?)
        }
        AdapterFormat::OggOpus => return Err("malformed_audio"),
    };
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or("malformed_audio")?;
    let track_id = track.id;
    let codec_parameters = track
        .codec_params
        .as_ref()
        .and_then(symphonia::core::codecs::CodecParameters::audio)
        .cloned()
        .ok_or("malformed_audio")?;
    let declared_metadata = AudioMetadata {
        channels: codec_parameters
            .channels
            .as_ref()
            .ok_or("malformed_audio")?
            .count(),
        sample_rate_hz: codec_parameters.sample_rate.ok_or("malformed_audio")?,
    };
    validate_shape(declared_metadata)?;
    let declared_frames = match format {
        AdapterFormat::Flac => flac_declared_frames(bytes)?,
        AdapterFormat::Mp3 => mp3_declared_frames(bytes)?,
        AdapterFormat::Wav | AdapterFormat::OggOpus => None,
    };
    let mut decoder_options = AudioDecoderOptions::default();
    decoder_options.verify = true;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_parameters, &decoder_options)
        .map_err(|_| "malformed_audio")?;
    let mut metadata = (format == AdapterFormat::Wav).then_some(declared_metadata);
    let mut raw_decoded_frames = 0_u64;
    let mut presented_frames = 0_u64;

    while let Some(packet) = reader.next_packet().map_err(|_| "malformed_audio")? {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).map_err(|_| "malformed_audio")?;
        let observed = AudioMetadata {
            channels: decoded.spec().channels().count(),
            sample_rate_hz: decoded.spec().rate(),
        };
        validate_shape(observed)?;
        if (format == AdapterFormat::Flac && observed != declared_metadata)
            || metadata.is_some_and(|prior| prior != observed)
        {
            return Err("malformed_audio");
        }
        metadata = Some(observed);
        let decoded_packet_frames =
            u64::try_from(decoded.frames()).map_err(|_| "duration_limit_exceeded")?;
        raw_decoded_frames = raw_decoded_frames
            .checked_add(decoded_packet_frames)
            .ok_or("duration_limit_exceeded")?;
        let trimmed_frames = packet
            .trim_start
            .get()
            .checked_add(packet.trim_end.get())
            .ok_or("duration_limit_exceeded")?;
        let presented_packet_frames = decoded_packet_frames
            .checked_sub(trimmed_frames)
            .ok_or("malformed_audio")?;
        presented_frames = presented_frames
            .checked_add(presented_packet_frames)
            .ok_or("duration_limit_exceeded")?;
        validate_duration(presented_frames, observed.sample_rate_hz)?;
    }
    let metadata = metadata.ok_or("malformed_audio")?;
    if matches!(format, AdapterFormat::Mp3 | AdapterFormat::Flac)
        && declared_frames.is_some_and(|frames| frames != 0 && frames != raw_decoded_frames)
    {
        return Err("malformed_audio");
    }
    if decoder.finalize().verify_ok == Some(false) {
        return Err("malformed_audio");
    }
    Ok(metadata)
}

fn decode_ogg_opus(bytes: &[u8]) -> Result<AudioMetadata, &'static str> {
    let mut packets = PacketReader::new(Cursor::new(bytes));
    let head = packets
        .read_packet()
        .map_err(|_| "malformed_audio")?
        .ok_or("malformed_audio")?;
    if !head.first_in_stream()
        || !head.first_in_page()
        || !head.last_in_page()
        || head.last_in_stream()
        || head.absgp_page() != 0
    {
        return Err("malformed_audio");
    }
    let (metadata, pre_skip) = parse_opus_head(&head.data)?;
    let serial = head.stream_serial();
    let tags = packets
        .read_packet()
        .map_err(|_| "malformed_audio")?
        .ok_or("malformed_audio")?;
    if tags.stream_serial() != serial
        || tags.last_in_stream()
        || (tags.last_in_page() && tags.absgp_page() != 0)
        || !valid_opus_tags(&tags.data)
    {
        return Err("malformed_audio");
    }

    let mut decoder =
        OpusDecoder::new(48_000, metadata.channels).map_err(|_| "unsupported_opus_mapping")?;
    let mut output = vec![0.0_f32; 5_760 * metadata.channels];
    let mut decoded_frames = 0_u64;
    let mut audio_packets = 0_u64;
    let mut final_granule = None;
    let mut final_packet_frames = None;
    let mut completed_page_granule = 0_u64;
    let mut saw_end_of_stream = false;
    while let Some(packet) = packets.read_packet().map_err(|_| "malformed_audio")? {
        if saw_end_of_stream || packet.stream_serial() != serial || packet.data.is_empty() {
            return Err("malformed_audio");
        }
        let frames = decoder
            .decode(&packet.data, 5_760, &mut output)
            .map_err(|_| "malformed_audio")?;
        decoded_frames = decoded_frames
            .checked_add(u64::try_from(frames).map_err(|_| "duration_limit_exceeded")?)
            .ok_or("duration_limit_exceeded")?;
        audio_packets = audio_packets
            .checked_add(1)
            .ok_or("duration_limit_exceeded")?;
        validate_ogg_decode_bound(decoded_frames, pre_skip)?;
        if packet.last_in_page() {
            let granule = packet.absgp_page();
            if granule < completed_page_granule
                || (!packet.last_in_stream() && granule != decoded_frames)
            {
                return Err("malformed_audio");
            }
            completed_page_granule = granule;
            final_granule = Some(granule);
        }
        if packet.last_in_stream() {
            saw_end_of_stream = true;
            final_packet_frames = Some(u64::try_from(frames).map_err(|_| "malformed_audio")?);
        }
    }
    if audio_packets == 0 || decoded_frames < u64::from(pre_skip) || !saw_end_of_stream {
        return Err("malformed_audio");
    }
    let final_granule = final_granule.ok_or("malformed_audio")?;
    let final_packet_frames = final_packet_frames.ok_or("malformed_audio")?;
    let presented_frames = final_granule
        .checked_sub(u64::from(pre_skip))
        .ok_or("malformed_audio")?;
    if final_granule > decoded_frames
        || decoded_frames.saturating_sub(final_granule) > final_packet_frames
    {
        return Err("malformed_audio");
    }
    validate_duration(presented_frames, metadata.sample_rate_hz)?;
    Ok(metadata)
}

fn flac_declared_frames(bytes: &[u8]) -> Result<Option<u64>, &'static str> {
    if !bytes.starts_with(b"fLaC") || bytes.get(4).is_none_or(|header| header & 0x7f != 0) {
        return Err("malformed_audio");
    }
    let encoded = bytes
        .get(18..26)
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or("malformed_audio")?;
    let frames = u64::from_be_bytes(encoded) & 0x0f_ff_ff_ff_ff;
    Ok((frames != 0).then_some(frames))
}

fn mp3_declared_frames(bytes: &[u8]) -> Result<Option<u64>, &'static str> {
    let header = bytes.get(..4).ok_or("malformed_audio")?;
    if !valid_mp3_frame_header(header) {
        return Err("malformed_audio");
    }
    let version = (header[1] >> 3) & 0x03;
    let layer = (header[1] >> 1) & 0x03;
    let samples_per_frame = match (version, layer) {
        (_, 0x03) => 384_u64,
        (_, 0x02) | (0x03, 0x01) => 1_152,
        (_, 0x01) => 576,
        _ => return Err("malformed_audio"),
    };
    let has_crc = header[1] & 1 == 0;
    let mono = (header[3] >> 6) & 0x03 == 0x03;
    let side_information = match (version == 0x03, mono) {
        (true, true) => 17_usize,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    };
    let xing_offset = (if has_crc { 6_usize } else { 4 })
        .checked_add(side_information)
        .ok_or("malformed_audio")?;
    let xing_frames = bytes
        .get(xing_offset..xing_offset.saturating_add(12))
        .filter(|value| value.starts_with(b"Xing") || value.starts_with(b"Info"))
        .and_then(|value| {
            let flags = u32::from_be_bytes(value.get(4..8)?.try_into().ok()?);
            (flags & 1 != 0)
                .then(|| value.get(8..12)?.try_into().ok().map(u32::from_be_bytes))
                .flatten()
        });
    let vbri_frames = bytes
        .get(36..54)
        .filter(|value| value.starts_with(b"VBRI"))
        .and_then(|value| value.get(14..18)?.try_into().ok())
        .map(u32::from_be_bytes);
    xing_frames
        .or(vbri_frames)
        .map(u64::from)
        .map(|frames| {
            frames
                .checked_sub(1)
                .and_then(|audio_frames| audio_frames.checked_mul(samples_per_frame))
                .ok_or("malformed_audio")
        })
        .transpose()
}

fn mp3_audio_bytes(bytes: &[u8]) -> Result<&[u8], &'static str> {
    if !bytes.starts_with(b"ID3") {
        return Ok(bytes);
    }
    let audio_offset = id3_audio_offset(bytes).ok_or("malformed_audio")?;
    bytes.get(audio_offset..).ok_or("malformed_audio")
}

fn parse_opus_head(bytes: &[u8]) -> Result<(AudioMetadata, u16), &'static str> {
    let prefix = bytes.get(..19).ok_or("malformed_audio")?;
    if !prefix.starts_with(b"OpusHead") || prefix[8] != 1 {
        return Err("malformed_audio");
    }
    let channels = usize::from(prefix[9]);
    if channels == 0 {
        return Err("channel_limit_exceeded");
    }
    if prefix[18] != 0 {
        return Err("unsupported_opus_mapping");
    }
    if bytes.len() != 19 {
        return Err("malformed_audio");
    }
    if channels > 2 {
        return Err("channel_limit_exceeded");
    }
    let metadata = AudioMetadata {
        channels,
        sample_rate_hz: 48_000,
    };
    validate_shape(metadata)?;
    Ok((metadata, u16::from_le_bytes([prefix[10], prefix[11]])))
}

fn valid_opus_tags(bytes: &[u8]) -> bool {
    let Some(vendor_length) = bytes
        .get(8..12)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .map(u32::from_le_bytes)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    if !bytes.starts_with(b"OpusTags") {
        return false;
    }
    let Some(comment_count_offset) = 12_usize.checked_add(vendor_length) else {
        return false;
    };
    let Some(vendor) = bytes.get(12..comment_count_offset) else {
        return false;
    };
    if std::str::from_utf8(vendor).is_err() {
        return false;
    }
    let Some(comment_count_bytes) = bytes
        .get(comment_count_offset..comment_count_offset.saturating_add(4))
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
    else {
        return false;
    };
    let Ok(comment_count) = usize::try_from(u32::from_le_bytes(comment_count_bytes)) else {
        return false;
    };
    let Some(mut offset) = comment_count_offset.checked_add(4) else {
        return false;
    };
    if comment_count > bytes.len().saturating_sub(offset) / 4 {
        return false;
    }
    for _ in 0..comment_count {
        let Some(data_offset) = offset.checked_add(4) else {
            return false;
        };
        let Some(length_bytes) = bytes
            .get(offset..data_offset)
            .and_then(|value| <[u8; 4]>::try_from(value).ok())
        else {
            return false;
        };
        let Some(length) = usize::try_from(u32::from_le_bytes(length_bytes)).ok() else {
            return false;
        };
        let Some(next) = data_offset.checked_add(length) else {
            return false;
        };
        let Some(comment) = bytes.get(data_offset..next) else {
            return false;
        };
        if !valid_opus_comment(comment) {
            return false;
        }
        offset = next;
    }
    true
}

fn valid_opus_comment(comment: &[u8]) -> bool {
    let Some(separator) = comment.iter().position(|byte| *byte == b'=') else {
        return false;
    };
    separator > 0
        && comment[..separator]
            .iter()
            .all(|byte| matches!(*byte, 0x20..=0x3c | 0x3e..=0x7d))
        && std::str::from_utf8(comment).is_ok()
}

fn validate_shape(metadata: AudioMetadata) -> Result<(), &'static str> {
    if metadata.channels == 0 || metadata.channels > usize::from(MAX_AUDIO_CHANNELS) {
        return Err("channel_limit_exceeded");
    }
    if metadata.sample_rate_hz == 0 || metadata.sample_rate_hz > MAX_AUDIO_SAMPLE_RATE_HZ {
        return Err("sample_rate_limit_exceeded");
    }
    Ok(())
}

fn validate_duration(decoded_frames: u64, sample_rate_hz: u32) -> Result<(), &'static str> {
    let maximum_frames = u64::from(sample_rate_hz)
        .checked_mul(u64::from(MAX_AUDIO_CLIP_SECONDS))
        .ok_or("duration_limit_exceeded")?;
    if decoded_frames > maximum_frames {
        return Err("duration_limit_exceeded");
    }
    Ok(())
}

fn validate_ogg_decode_bound(decoded_frames: u64, pre_skip: u16) -> Result<(), &'static str> {
    let maximum_decoded_frames = 48_000_u64
        .checked_mul(u64::from(MAX_AUDIO_CLIP_SECONDS))
        .and_then(|frames| frames.checked_add(u64::from(pre_skip)))
        .and_then(|frames| frames.checked_add(5_760))
        .ok_or("duration_limit_exceeded")?;
    if decoded_frames > maximum_decoded_frames {
        return Err("duration_limit_exceeded");
    }
    Ok(())
}

fn metadata_json(metadata: AudioMetadata) -> Result<String, FileMediaProviderFailure> {
    serde_json::to_string(&serde_json::json!({
        "channels": metadata.channels,
        "sample_rate_hz": metadata.sample_rate_hz,
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)
}

fn malformed(format: AdapterFormat, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(format.media_type()),
        reason_code: String::from(reason),
    }
}

fn validation_failure(
    format: AdapterFormat,
    evidence: ValidationEvidence,
    reason: &str,
) -> ProcessorValidationOutput {
    if evidence == ValidationEvidence::DeclaredCandidateStructurallyValidated {
        ProcessorValidationOutput::NoMatch
    } else {
        malformed(format, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_opus_head, valid_opus_tags};

    #[test]
    fn opus_tags_rejects_a_declared_comment_without_its_length_or_data() {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&0_u32.to_le_bytes());
        tags.extend_from_slice(&1_u32.to_le_bytes());

        assert!(!valid_opus_tags(&tags));
    }

    #[test]
    fn opus_tags_rejects_an_invalid_utf8_vendor() {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&1_u32.to_le_bytes());
        tags.push(0xff);
        tags.extend_from_slice(&0_u32.to_le_bytes());

        assert!(!valid_opus_tags(&tags));
    }

    #[test]
    fn opus_tags_rejects_an_invalid_utf8_comment() {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&0_u32.to_le_bytes());
        tags.extend_from_slice(&1_u32.to_le_bytes());
        tags.extend_from_slice(&1_u32.to_le_bytes());
        tags.push(0xff);

        assert!(!valid_opus_tags(&tags));
    }

    #[test]
    fn opus_tags_rejects_a_comment_without_a_field_name_separator() {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&0_u32.to_le_bytes());
        tags.extend_from_slice(&1_u32.to_le_bytes());
        tags.extend_from_slice(&11_u32.to_le_bytes());
        tags.extend_from_slice(b"not-a-field");

        assert!(!valid_opus_tags(&tags));
    }

    #[test]
    fn opus_tags_accepts_trailing_padding() {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&0_u32.to_le_bytes());
        tags.extend_from_slice(&0_u32.to_le_bytes());
        tags.extend_from_slice(&[0; 16]);

        assert!(valid_opus_tags(&tags));
    }

    #[test]
    fn opus_head_classifies_mapping_family_one_as_unsupported() {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(6);
        head.extend_from_slice(&0_u16.to_le_bytes());
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&0_i16.to_le_bytes());
        head.push(1);
        head.extend_from_slice(&[4, 2, 0, 1, 2, 3, 4, 5]);

        assert_eq!(parse_opus_head(&head), Err("unsupported_opus_mapping"));
    }

    #[test]
    fn opus_head_classifies_oversized_family_zero_as_malformed() {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(2);
        head.extend_from_slice(&0_u16.to_le_bytes());
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&0_i16.to_le_bytes());
        head.push(0);
        head.push(0);

        assert_eq!(parse_opus_head(&head), Err("malformed_audio"));
    }
}
