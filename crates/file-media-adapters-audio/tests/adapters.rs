mod fixtures;
mod support;

use std::error::Error;

use fixtures::FixtureFormat;
use support::{DirectProcessor, MemorySource};

#[tokio::test]
async fn wav_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    assert_valid(FixtureFormat::Wav).await
}

#[tokio::test]
async fn mp3_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    assert_valid(FixtureFormat::Mp3).await
}

#[tokio::test]
async fn flac_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    assert_valid(FixtureFormat::Flac).await
}

#[tokio::test]
async fn ogg_opus_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    assert_valid(FixtureFormat::OggOpus).await
}

#[tokio::test]
async fn wav_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    assert_hostile(FixtureFormat::Wav).await
}

#[tokio::test]
async fn mp3_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    assert_hostile(FixtureFormat::Mp3).await
}

#[tokio::test]
async fn flac_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    assert_hostile(FixtureFormat::Flac).await
}

#[tokio::test]
async fn ogg_opus_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    assert_hostile(FixtureFormat::OggOpus).await
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

async fn assert_valid(format: FixtureFormat) -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::valid(format)?);
    let expected = fixtures::expected_metadata(format);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, format.media_type());
    let result = support::read(&source, format.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

async fn assert_hostile(format: FixtureFormat) -> Result<(), Box<dyn Error>> {
    let truncated = MemorySource::new(fixtures::truncated(format)?);
    let malformed = MemorySource::new(fixtures::malformed(format));
    let oversized = MemorySource::new(fixtures::oversized(format)?);
    let duration_bomb = MemorySource::new(fixtures::duration_bomb(format)?);

    let truncated_result = support::inspect(&truncated, format.media_type()).await?;
    support::assert_malformed_reason(truncated_result, "malformed_audio");
    let malformed_result = support::inspect(&malformed, format.media_type()).await?;
    support::assert_malformed_reason(malformed_result, "malformed_audio");
    let oversized_result = support::inspect(&oversized, format.media_type()).await?;
    support::assert_malformed_reason(oversized_result, "source_too_large");
    let bomb_result = support::inspect(&duration_bomb, format.media_type()).await?;
    support::assert_malformed_reason(bomb_result, "duration_limit_exceeded");
    Ok(())
}
