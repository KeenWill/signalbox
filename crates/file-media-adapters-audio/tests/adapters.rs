mod fixtures;
mod support;

use std::error::Error;

use fixtures::FixtureFormat;
use support::{DirectProcessor, MemorySource};

#[tokio::test]
async fn wav_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Wav;
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = serde_json::json!({"channels": 1, "sample_rate_hz": 8_000});

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, "audio/wav");
    let result = support::read(&source, format.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn mp3_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Mp3;
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = serde_json::json!({"channels": 1, "sample_rate_hz": 8_000});

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, "audio/mpeg");
    let result = support::read(&source, format.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn flac_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Flac;
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = serde_json::json!({"channels": 1, "sample_rate_hz": 8_000});

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, "audio/flac");
    let result = support::read(&source, format.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn ogg_opus_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = serde_json::json!({"channels": 1, "sample_rate_hz": 48_000});

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, "audio/ogg");
    let result = support::read(&source, format.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn isolated_worker_validates_audio_larger_than_one_broker_range() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::new(fixtures::wav_larger_than_one_broker_range()?);

    let inspection = support::inspect_sandboxed(&source, "audio/wav").await?;
    support::assert_validated_media(inspection, "audio/wav");
    Ok(())
}

#[tokio::test]
async fn failed_declared_audio_candidate_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"not audio bytes".to_vec());

    let inspection = support::inspect(&source, "audio/wav").await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn wav_truncation_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Wav,
        fixtures::truncated(FixtureFormat::Wav)?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn wav_malformed_bytes_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Wav,
        fixtures::malformed(FixtureFormat::Wav),
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn wav_oversized_source_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Wav,
        fixtures::oversized(FixtureFormat::Wav)?,
        "source_too_large",
    )
    .await
}

#[tokio::test]
async fn wav_duration_over_limit_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Wav,
        fixtures::duration_bomb(FixtureFormat::Wav)?,
        "duration_limit_exceeded",
    )
    .await
}

#[tokio::test]
async fn mp3_truncation_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::truncated(FixtureFormat::Mp3)?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn mp3_malformed_bytes_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::malformed(FixtureFormat::Mp3),
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn mp3_oversized_source_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::oversized(FixtureFormat::Mp3)?,
        "source_too_large",
    )
    .await
}

#[tokio::test]
async fn mp3_duration_over_limit_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::duration_bomb(FixtureFormat::Mp3)?,
        "duration_limit_exceeded",
    )
    .await
}

#[tokio::test]
async fn flac_truncation_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::truncated(FixtureFormat::Flac)?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn flac_malformed_bytes_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::malformed(FixtureFormat::Flac),
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn flac_oversized_source_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::oversized(FixtureFormat::Flac)?,
        "source_too_large",
    )
    .await
}

#[tokio::test]
async fn flac_duration_over_limit_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::duration_bomb(FixtureFormat::Flac)?,
        "duration_limit_exceeded",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_truncation_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::truncated(FixtureFormat::OggOpus)?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_malformed_bytes_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::malformed(FixtureFormat::OggOpus),
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_oversized_source_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::oversized(FixtureFormat::OggOpus)?,
        "source_too_large",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_duration_over_limit_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::duration_bomb(FixtureFormat::OggOpus)?,
        "duration_limit_exceeded",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_requires_an_end_of_stream_page() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::ogg_opus_without_end_of_stream()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_malformed_reason(inspection, "malformed_audio");
    Ok(())
}

#[tokio::test]
async fn ogg_opus_duration_uses_presented_samples_after_trimming() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::ogg_opus_trimmed_to_duration_limit()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, "audio/ogg");
    Ok(())
}

#[tokio::test]
async fn registry_sanitizer_keeps_injection_shaped_metadata_as_data() -> Result<(), Box<dyn Error>>
{
    let format = FixtureFormat::Wav;
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = serde_json::json!({
        "path":"../../etc/passwd",
        "text":"</tool><script>alert(1)</script>"
    });
    let decoder_output = serde_json::to_string(&expected)?;

    let result = support::read(
        &source,
        format.media_type(),
        &DirectProcessor::injecting(decoder_output),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn registry_sanitizer_rejects_nul_bearing_decoder_output() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Wav;
    let source = MemorySource::new(fixtures::valid(format)?);
    let decoder_output = String::from("{\"text\":\"prefix\0suffix\"}");

    let result = support::read(
        &source,
        format.media_type(),
        &DirectProcessor::injecting(decoder_output),
    )
    .await;
    support::assert_processor_failed(result);
    Ok(())
}

#[track_caller]
async fn assert_reason(
    format: FixtureFormat,
    bytes: Vec<u8>,
    expected_reason: &str,
) -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(bytes);
    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_malformed_reason(inspection, expected_reason);
    Ok(())
}
