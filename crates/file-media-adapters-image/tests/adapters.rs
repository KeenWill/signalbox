mod fixtures;
mod support;

use std::error::Error;

use fixtures::FixtureFormat;
use signalbox_file_media_runtime::FileMediaCeilings;
use support::{DirectProcessor, MemorySource};

#[tokio::test]
async fn png_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let source = MemorySource::new(fixtures::valid(format)?);
    let (width, height) = fixtures::valid_dimensions();
    let expected = serde_json::json!({"channels": 4, "height": height, "width": width});

    let inspection = support::inspect(&source, "image/png").await?;
    support::assert_validated_media(inspection, "image/png");
    let result = support::read(&source, "image/png", &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn jpeg_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Jpeg;
    let source = MemorySource::new(fixtures::valid(format)?);
    let (width, height) = fixtures::valid_dimensions();
    let expected = serde_json::json!({"channels": 3, "height": height, "width": width});

    let inspection = support::inspect(&source, "image/jpeg").await?;
    support::assert_validated_media(inspection, "image/jpeg");
    let result = support::read(&source, "image/jpeg", &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn webp_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::WebP;
    let source = MemorySource::new(fixtures::valid(format)?);
    let (width, height) = fixtures::valid_dimensions();
    let expected = serde_json::json!({"channels": 4, "height": height, "width": width});

    let inspection = support::inspect(&source, "image/webp").await?;
    support::assert_validated_media(inspection, "image/webp");
    let result = support::read(&source, "image/webp", &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn gif_detects_validates_and_reads_metadata() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Gif;
    let source = MemorySource::new(fixtures::valid(format)?);
    let (width, height) = fixtures::valid_dimensions();
    let expected = serde_json::json!({"channels": 4, "height": height, "width": width});

    let inspection = support::inspect(&source, "image/gif").await?;
    support::assert_validated_media(inspection, "image/gif");
    let result = support::read(&source, "image/gif", &DirectProcessor::provider()).await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn png_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let truncated = MemorySource::new(fixtures::truncated(format)?);
    let malformed = MemorySource::new(fixtures::malformed(format));
    let oversized = MemorySource::new(fixtures::oversized(format)?);
    let dimension_bomb = MemorySource::new(fixtures::dimension_bomb(format)?);
    let pixel_bomb = MemorySource::new(fixtures::pixel_bomb(format)?);

    let truncated_result = support::inspect(&truncated, "image/png").await?;
    support::assert_malformed_reason(truncated_result, "malformed_image");
    let malformed_result = support::inspect(&malformed, "image/png").await?;
    support::assert_malformed_reason(malformed_result, "malformed_image");
    let oversized_result = support::inspect(&oversized, "image/png").await?;
    support::assert_malformed_reason(oversized_result, "source_too_large");
    let dimension_result = support::inspect(&dimension_bomb, "image/png").await?;
    support::assert_malformed_reason(dimension_result, "dimension_limit_exceeded");
    let pixel_result = support::inspect(&pixel_bomb, "image/png").await?;
    support::assert_malformed_reason(pixel_result, "pixel_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn jpeg_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Jpeg;
    let truncated = MemorySource::new(fixtures::truncated(format)?);
    let malformed = MemorySource::new(fixtures::malformed(format));
    let oversized = MemorySource::new(fixtures::oversized(format)?);
    let dimension_bomb = MemorySource::new(fixtures::dimension_bomb(format)?);
    let pixel_bomb = MemorySource::new(fixtures::pixel_bomb(format)?);

    let truncated_result = support::inspect(&truncated, "image/jpeg").await?;
    support::assert_malformed_reason(truncated_result, "malformed_image");
    let malformed_result = support::inspect(&malformed, "image/jpeg").await?;
    support::assert_malformed_reason(malformed_result, "malformed_image");
    let oversized_result = support::inspect(&oversized, "image/jpeg").await?;
    support::assert_malformed_reason(oversized_result, "source_too_large");
    let dimension_result = support::inspect(&dimension_bomb, "image/jpeg").await?;
    support::assert_malformed_reason(dimension_result, "dimension_limit_exceeded");
    let pixel_result = support::inspect(&pixel_bomb, "image/jpeg").await?;
    support::assert_malformed_reason(pixel_result, "pixel_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn webp_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::WebP;
    let truncated = MemorySource::new(fixtures::truncated(format)?);
    let malformed = MemorySource::new(fixtures::malformed(format));
    let oversized = MemorySource::new(fixtures::oversized(format)?);
    let dimension_bomb = MemorySource::new(fixtures::dimension_bomb(format)?);
    let pixel_bomb = MemorySource::new(fixtures::pixel_bomb(format)?);

    let truncated_result = support::inspect(&truncated, "image/webp").await?;
    support::assert_malformed_reason(truncated_result, "malformed_image");
    let malformed_result = support::inspect(&malformed, "image/webp").await?;
    support::assert_malformed_reason(malformed_result, "malformed_image");
    let oversized_result = support::inspect(&oversized, "image/webp").await?;
    support::assert_malformed_reason(oversized_result, "source_too_large");
    let dimension_result = support::inspect(&dimension_bomb, "image/webp").await?;
    support::assert_malformed_reason(dimension_result, "dimension_limit_exceeded");
    let pixel_result = support::inspect(&pixel_bomb, "image/webp").await?;
    support::assert_malformed_reason(pixel_result, "pixel_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn gif_hostile_inputs_fail_typed_within_ceilings() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Gif;
    let truncated = MemorySource::new(fixtures::truncated(format)?);
    let malformed = MemorySource::new(fixtures::malformed(format));
    let oversized = MemorySource::new(fixtures::oversized(format)?);
    let dimension_bomb = MemorySource::new(fixtures::dimension_bomb(format)?);
    let pixel_bomb = MemorySource::new(fixtures::pixel_bomb(format)?);

    let truncated_result = support::inspect(&truncated, "image/gif").await?;
    support::assert_malformed_reason(truncated_result, "malformed_image");
    let malformed_result = support::inspect(&malformed, "image/gif").await?;
    support::assert_malformed_reason(malformed_result, "malformed_image");
    let oversized_result = support::inspect(&oversized, "image/gif").await?;
    support::assert_malformed_reason(oversized_result, "source_too_large");
    let dimension_result = support::inspect(&dimension_bomb, "image/gif").await?;
    support::assert_malformed_reason(dimension_result, "dimension_limit_exceeded");
    let pixel_result = support::inspect(&pixel_bomb, "image/gif").await?;
    support::assert_malformed_reason(pixel_result, "pixel_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn lowered_image_axis_is_enforced_during_metadata_validation() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let source = MemorySource::new(fixtures::valid(format)?);
    let ceilings = FileMediaCeilings {
        image_axis: 2,
        ..FileMediaCeilings::version_one()
    };

    let inspection = support::inspect_with_ceilings(&source, format.media_type(), ceilings).await?;
    support::assert_malformed_reason(inspection, "dimension_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn lowered_decoded_pixels_are_enforced_during_metadata_validation()
-> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let source = MemorySource::new(fixtures::valid(format)?);
    let ceilings = FileMediaCeilings {
        decoded_image_pixels: 5,
        ..FileMediaCeilings::version_one()
    };

    let inspection = support::inspect_with_ceilings(&source, format.media_type(), ceilings).await?;
    support::assert_malformed_reason(inspection, "pixel_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn lowered_validation_source_bytes_return_source_too_large() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let source = MemorySource::new(fixtures::valid(format)?);
    let ceilings = FileMediaCeilings {
        validation_source_bytes: 16,
        ..FileMediaCeilings::version_one()
    };

    let inspection = support::inspect_with_ceilings(&source, format.media_type(), ceilings).await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn registry_sanitizer_keeps_injection_shaped_metadata_as_data() -> Result<(), Box<dyn Error>>
{
    let format = FixtureFormat::Png;
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
    let format = FixtureFormat::Png;
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
