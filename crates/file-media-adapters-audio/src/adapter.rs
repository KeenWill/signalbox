use std::io::Cursor;

use ogg::PacketReader;
use opus_rs::OpusDecoder;
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
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
    AdapterFormat, MAX_AUDIO_CHANNELS, MAX_AUDIO_DURATION_SECONDS, MAX_AUDIO_SAMPLE_RATE_HZ,
    MAX_AUDIO_SOURCE_BYTES, options_are_empty, source,
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
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    if format.matches_signature(&prefix) {
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
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    if request.media_type.as_str() != format.media_type() {
        return Err(ProcessorFailure::Protocol);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(malformed(format, "source_too_large"));
    };
    let metadata = match decode(format, &bytes) {
        Ok(metadata) => metadata,
        Err(reason) => return Ok(malformed(format, reason)),
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
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    if request.view.as_str() != "metadata" || !options_are_empty(&request.options) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_AUDIO_SOURCE_BYTES,
        });
    };
    let metadata = decode(format, &bytes).map_err(|_| ProcessorFailure::Failed)?;
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
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_parameters, &AudioDecoderOptions::default())
        .map_err(|_| "malformed_audio")?;
    let mut metadata = None;
    let mut decoded_frames = 0_u64;

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
        if metadata.is_some_and(|prior| prior != observed) {
            return Err("malformed_audio");
        }
        metadata = Some(observed);
        decoded_frames = decoded_frames
            .checked_add(u64::try_from(decoded.frames()).map_err(|_| "duration_limit_exceeded")?)
            .ok_or("duration_limit_exceeded")?;
        validate_duration(decoded_frames, observed.sample_rate_hz)?;
    }
    metadata.ok_or("malformed_audio")
}

fn decode_ogg_opus(bytes: &[u8]) -> Result<AudioMetadata, &'static str> {
    let mut packets = PacketReader::new(Cursor::new(bytes));
    let head = packets
        .read_packet()
        .map_err(|_| "malformed_audio")?
        .ok_or("malformed_audio")?;
    let (metadata, pre_skip) = parse_opus_head(&head.data)?;
    let serial = head.stream_serial();
    let tags = packets
        .read_packet()
        .map_err(|_| "malformed_audio")?
        .ok_or("malformed_audio")?;
    if tags.stream_serial() != serial || !valid_opus_tags(&tags.data) {
        return Err("malformed_audio");
    }

    let mut decoder =
        OpusDecoder::new(48_000, metadata.channels).map_err(|_| "unsupported_opus_mapping")?;
    let mut output = vec![0.0_f32; 5_760 * metadata.channels];
    let mut decoded_frames = 0_u64;
    let mut audio_packets = 0_u64;
    while let Some(packet) = packets.read_packet().map_err(|_| "malformed_audio")? {
        if packet.stream_serial() != serial || packet.data.is_empty() {
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
        validate_duration(decoded_frames, metadata.sample_rate_hz)?;
    }
    if audio_packets == 0 || decoded_frames < u64::from(pre_skip) {
        return Err("malformed_audio");
    }
    Ok(metadata)
}

fn mp3_audio_bytes(bytes: &[u8]) -> Result<&[u8], &'static str> {
    if !bytes.starts_with(b"ID3") {
        return Ok(bytes);
    }
    let header = bytes.get(..10).ok_or("malformed_audio")?;
    if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
        return Err("malformed_audio");
    }
    let tag_length = header[6..10]
        .iter()
        .try_fold(0_usize, |length, byte| {
            length
                .checked_mul(128)
                .and_then(|value| value.checked_add(usize::from(*byte)))
        })
        .ok_or("malformed_audio")?;
    let footer_length = if header[5] & 0x10 == 0 { 0 } else { 10 };
    let audio_offset = 10_usize
        .checked_add(tag_length)
        .and_then(|value| value.checked_add(footer_length))
        .ok_or("malformed_audio")?;
    bytes.get(audio_offset..).ok_or("malformed_audio")
}

fn parse_opus_head(bytes: &[u8]) -> Result<(AudioMetadata, u16), &'static str> {
    if bytes.len() != 19 || !bytes.starts_with(b"OpusHead") || bytes[8] != 1 {
        return Err("malformed_audio");
    }
    let channels = usize::from(bytes[9]);
    if channels == 0 || channels > 2 {
        return Err("channel_limit_exceeded");
    }
    if bytes[18] != 0 {
        return Err("unsupported_opus_mapping");
    }
    let metadata = AudioMetadata {
        channels,
        sample_rate_hz: 48_000,
    };
    validate_shape(metadata)?;
    Ok((metadata, u16::from_le_bytes([bytes[10], bytes[11]])))
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
    bytes
        .get(comment_count_offset..comment_count_offset.saturating_add(4))
        .is_some()
}

fn validate_shape(metadata: AudioMetadata) -> Result<(), &'static str> {
    if metadata.channels == 0 || metadata.channels > MAX_AUDIO_CHANNELS {
        return Err("channel_limit_exceeded");
    }
    if metadata.sample_rate_hz == 0 || metadata.sample_rate_hz > MAX_AUDIO_SAMPLE_RATE_HZ {
        return Err("sample_rate_limit_exceeded");
    }
    Ok(())
}

fn validate_duration(decoded_frames: u64, sample_rate_hz: u32) -> Result<(), &'static str> {
    let maximum_frames = u64::from(sample_rate_hz)
        .checked_mul(MAX_AUDIO_DURATION_SECONDS)
        .ok_or("duration_limit_exceeded")?;
    if decoded_frames > maximum_frames {
        return Err("duration_limit_exceeded");
    }
    Ok(())
}

fn metadata_json(metadata: AudioMetadata) -> Result<String, ProcessorFailure> {
    serde_json::to_string(&serde_json::json!({
        "channels": metadata.channels,
        "sample_rate_hz": metadata.sample_rate_hz,
    }))
    .map_err(|_| ProcessorFailure::Failed)
}

fn malformed(format: AdapterFormat, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(format.media_type()),
        reason_code: String::from(reason),
    }
}
