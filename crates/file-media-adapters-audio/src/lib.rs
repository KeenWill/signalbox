//! Isolated adapters for WAV, MP3, FLAC, and Ogg Opus bytes.

mod adapter;
mod source;

use std::{error::Error, str::FromStr};

use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFuture, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration, ProcessorFailure,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration, ReaderDeclarationInput,
    ReaderIdentity, ReasonCode, StreamingTextFallback, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "signalbox_audio";
const READER_REVISION: &str = "v1";
const METADATA_VIEW_NAME: &str = "metadata";
/// Hard safety ceiling covering the exact probe prefix and possible one-byte suffix.
const AUDIO_PROBE_CUMULATIVE_BYTES: u64 = 65;

/// Hard safety ceiling bounding whole-source worker memory while admitting ordinary audio.
pub const MAX_AUDIO_SOURCE_BYTES: u64 = 64 * 1_024 * 1_024;
/// Hard safety ceiling bounding decoder expansion by channel count.
pub const MAX_AUDIO_CHANNELS: usize = 8;
/// Hard safety ceiling bounding decoder work by sample rate.
pub const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 192_000;
/// Hard safety ceiling bounding full-decode latency by presented duration.
pub const MAX_AUDIO_DURATION_SECONDS: u64 = 60;

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
            Self::OggOpus => {
                prefix.starts_with(b"OggS")
                    && prefix
                        .windows(b"OpusHead".len())
                        .any(|window| window == b"OpusHead")
            }
        }
    }
}

fn mp3_signature(prefix: &[u8]) -> bool {
    let audio = if prefix.starts_with(b"ID3") {
        let Some(header) = prefix.get(..10) else {
            return false;
        };
        if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return false;
        }
        let Some(tag_length) = header[6..10].iter().try_fold(0_usize, |length, byte| {
            length
                .checked_mul(128)
                .and_then(|value| value.checked_add(usize::from(*byte)))
        }) else {
            return false;
        };
        let footer_length = if header[5] & 0x10 == 0 { 0 } else { 10 };
        let Some(audio_offset) = 10_usize
            .checked_add(tag_length)
            .and_then(|value| value.checked_add(footer_length))
        else {
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
    use super::AdapterFormat;

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
        probe: ProbeDeclaration::new(64, 1, 2, AUDIO_PROBE_CUMULATIVE_BYTES),
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
        ReadAccessPattern::Streaming,
        ReadViewBounds::Structured {
            source_bytes: MAX_AUDIO_SOURCE_BYTES,
            output_bytes: 256,
            depth: 2,
            nodes: 8,
            string_bytes: 64,
        },
    )?)
}

fn format_for_reader(reader: &ReaderIdentity) -> Result<AdapterFormat, ProcessorFailure> {
    AdapterFormat::ALL
        .into_iter()
        .find(|format| format.reader_name() == reader.reader().as_str())
        .ok_or(ProcessorFailure::Protocol)
}

fn options_are_empty(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}
