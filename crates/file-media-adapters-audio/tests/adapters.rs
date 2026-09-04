mod fixtures;
mod support;

use std::error::Error;

use fixtures::FixtureFormat;
use support::{DirectProcessor, MemorySource};

#[tokio::test]
async fn wav_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Wav;
    let fixture = fixtures::valid_fixture(format)?;
    let source = MemorySource::new(fixture.bytes().to_vec());

    let inspection = support::inspect(&source, fixture.media_type()).await?;
    support::assert_validated_media(inspection, fixture.media_type());
    let result = support::read(&source, fixture.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, fixture.expected_metadata());
    Ok(())
}

#[tokio::test]
async fn mp3_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Mp3;
    let fixture = fixtures::valid_fixture(format)?;
    let source = MemorySource::new(fixture.bytes().to_vec());

    let inspection = support::inspect(&source, fixture.media_type()).await?;
    support::assert_validated_media(inspection, fixture.media_type());
    let result = support::read(&source, fixture.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, fixture.expected_metadata());
    Ok(())
}

#[tokio::test]
async fn flac_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Flac;
    let fixture = fixtures::valid_fixture(format)?;
    let source = MemorySource::new(fixture.bytes().to_vec());

    let inspection = support::inspect(&source, fixture.media_type()).await?;
    support::assert_validated_media(inspection, fixture.media_type());
    let result = support::read(&source, fixture.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, fixture.expected_metadata());
    Ok(())
}

#[tokio::test]
async fn ogg_opus_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let fixture = fixtures::valid_fixture(format)?;
    let source = MemorySource::new(fixture.bytes().to_vec());

    let inspection = support::inspect(&source, fixture.media_type()).await?;
    support::assert_validated_media(inspection, fixture.media_type());
    let result = support::read(&source, fixture.media_type(), &DirectProcessor::provider()).await?;
    support::assert_structured(result, fixture.expected_metadata());
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
async fn wav_without_frames_is_valid() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::wav_without_frames()?);

    let inspection = support::inspect(&source, "audio/wav").await?;
    support::assert_validated_media(inspection, "audio/wav");
    Ok(())
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
    let format = FixtureFormat::Mp3;
    let source = MemorySource::new(fixtures::malformed(format));

    let inspection = support::inspect(&source, format.media_type()).await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
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
async fn mp3_probe_reads_the_frame_after_a_long_id3_tag() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Mp3;
    let source = MemorySource::new(fixtures::mp3_with_long_id3_tag()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_validated_media(inspection, format.media_type());
    Ok(())
}

#[tokio::test]
async fn mp3_probe_rejects_an_invalid_id3_version() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Mp3;
    let source = MemorySource::new(fixtures::mp3_with_id3_header(fixtures::Id3HeaderFixture {
        major: 5,
        flags: 0,
    })?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn mp3_probe_rejects_invalid_id3_flags() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Mp3;
    let source = MemorySource::new(fixtures::mp3_with_id3_header(fixtures::Id3HeaderFixture {
        major: 4,
        flags: 0x01,
    })?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn mp3_probe_rejects_an_invalid_id3v24_footer() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::mp3_with_invalid_id3v24_footer()?);

    let inspection = support::inspect(&source, "audio/mpeg").await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn mp3_probe_reads_an_id3v24_footer_past_the_probe_prefix() -> Result<(), Box<dyn Error>> {
    let (bytes, _) = fixtures::mp3_with_id3v24_footer_past_the_probe_prefix()?;
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "audio/mpeg").await?;
    support::assert_validated_media(inspection, "audio/mpeg");
    Ok(())
}

#[tokio::test]
async fn mp3_probe_propagates_an_unreadable_id3v24_footer() -> Result<(), Box<dyn Error>> {
    let (bytes, footer_offset) = fixtures::mp3_with_id3v24_footer_past_the_probe_prefix()?;
    let source = MemorySource::unavailable_at(bytes, footer_offset);

    assert_eq!(
        support::inspect_failure(&source, "audio/mpeg").await?,
        Some(signalbox_file_media_runtime::FileMediaFailure::ProcessorFailed)
    );
    Ok(())
}

#[tokio::test]
async fn mp3_rejects_an_empty_advertised_id3v24_extended_header() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::mp3_with_empty_id3v24_extended_header()?);

    let inspection = support::inspect(&source, "audio/mpeg").await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn mp3_rejects_fewer_frames_than_its_xing_header_advertises() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::mp3_with_excess_xing_frame_count()?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn mp3_rejects_audio_frames_against_a_xing_header_declaring_zero()
-> Result<(), Box<dyn Error>> {
    // A Xing header declaring a total of one frame (the header frame itself,
    // zero audio frames) must not take the zero-skip meant for FLAC's "unknown
    // total samples" STREAMINFO convention, which would silently validate any
    // decoded audio despite contradicting the declared frame count.
    assert_reason(
        FixtureFormat::Mp3,
        fixtures::mp3_with_xing_frame_count_of_one()?,
        "malformed_audio",
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
async fn flac_mismatched_streaminfo_md5_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::flac_with_mismatched_md5()?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn flac_truncated_between_complete_frames_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::flac_truncated_between_complete_frames()?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn flac_decoded_shape_must_match_streaminfo() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::Flac,
        fixtures::flac_with_mismatched_streaminfo_channels()?,
        "malformed_audio",
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
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::malformed(format));

    let inspection = support::inspect(&source, format.media_type()).await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
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
async fn ogg_opus_rejects_end_of_stream_on_tags() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::ogg_opus_with_tags_end_of_stream()?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_rejects_a_nonzero_tags_page_granule() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::ogg_opus_with_nonzero_tags_granule()?,
        "malformed_audio",
    )
    .await
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
async fn ogg_opus_requires_an_isolated_identification_header_page() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::ogg_opus_with_shared_identification_page()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    assert!(matches!(
        inspection,
        signalbox_file_media_runtime::FileInspection::Unknown { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn ogg_opus_rejects_regressing_page_granules() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::ogg_opus_with_regressing_granule()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_malformed_reason(inspection, "malformed_audio");
    Ok(())
}

#[tokio::test]
async fn ogg_opus_rejects_an_inaccurate_intermediate_granule() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::OggOpus;
    let source = MemorySource::new(fixtures::ogg_opus_with_inaccurate_intermediate_granule()?);

    let inspection = support::inspect(&source, format.media_type()).await?;
    support::assert_malformed_reason(inspection, "malformed_audio");
    Ok(())
}

#[tokio::test]
async fn ogg_opus_rejects_end_of_stream_on_identification_header() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::ogg_opus_with_head_end_of_stream()?,
        "malformed_audio",
    )
    .await
}

#[tokio::test]
async fn ogg_opus_rejects_eos_trimming_beyond_the_final_packet() -> Result<(), Box<dyn Error>> {
    assert_reason(
        FixtureFormat::OggOpus,
        fixtures::ogg_opus_with_excessive_end_trim()?,
        "malformed_audio",
    )
    .await
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
