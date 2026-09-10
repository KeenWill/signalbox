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

#[tokio::test(flavor = "multi_thread")]
async fn truncated_rasters_cannot_produce_direct_references() -> Result<(), Box<dyn Error>> {
    use signalbox_file_media_runtime::*;
    for format in [FixtureFormat::Png, FixtureFormat::Jpeg, FixtureFormat::WebP] {
        let source = MemorySource::new(fixtures::truncated(format)?);
        let registry = FileMediaRegistry::try_new(
            vec![
                signalbox_file_media_adapters_image::image_family_declaration()
                    .map_err(|e| e.to_string())?,
            ],
            FileMediaCeilings::version_one(),
            ProcessorIsolation::Available,
        )?;
        let result = registry
            .read(
                &DirectProcessor::provider(),
                FileReadRequest {
                    inspection: InspectionRequest {
                        source: source.file_use(format.media_type())?,
                        visible_part: None,
                    },
                    view: ReadViewName::try_new("image")?,
                    input: FileReadInput::Initial {
                        options: serde_json::json!({}),
                    },
                },
                &source,
                &NeverCancelled,
            )
            .await;
        assert!(result.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn malformed_images_are_reported_as_malformed() -> Result<(), Box<dyn Error>> {
    let png = MemorySource::new(fixtures::malformed(FixtureFormat::Png));
    let jpeg = MemorySource::new(fixtures::malformed(FixtureFormat::Jpeg));
    let webp = MemorySource::new(fixtures::malformed(FixtureFormat::WebP));
    let gif = MemorySource::new(fixtures::malformed(FixtureFormat::Gif));

    support::assert_malformed_reason(
        support::inspect(&png, "image/png").await?,
        "malformed_image",
    );
    support::assert_malformed_reason(
        support::inspect(&jpeg, "image/jpeg").await?,
        "malformed_image",
    );
    support::assert_malformed_reason(
        support::inspect(&webp, "image/webp").await?,
        "malformed_image",
    );
    support::assert_malformed_reason(
        support::inspect(&gif, "image/gif").await?,
        "malformed_image",
    );
    Ok(())
}

#[tokio::test]
async fn inspection_uses_a_prefix_when_the_source_exceeds_the_probe_bound()
-> Result<(), Box<dyn Error>> {
    for format in [
        FixtureFormat::Png,
        FixtureFormat::Jpeg,
        FixtureFormat::WebP,
        FixtureFormat::Gif,
    ] {
        let source = MemorySource::new(fixtures::oversized(format)?);
        support::assert_validated_media(
            support::inspect(&source, format.media_type()).await?,
            format.media_type(),
        );
    }
    Ok(())
}

#[tokio::test]
async fn dimension_bombs_are_reported_as_dimension_limit_exceeded() -> Result<(), Box<dyn Error>> {
    let png = MemorySource::new(fixtures::dimension_bomb(FixtureFormat::Png)?);
    let jpeg = MemorySource::new(fixtures::dimension_bomb(FixtureFormat::Jpeg)?);
    let webp = MemorySource::new(fixtures::dimension_bomb(FixtureFormat::WebP)?);
    let gif = MemorySource::new(fixtures::dimension_bomb(FixtureFormat::Gif)?);

    support::assert_malformed_reason(
        support::inspect(&png, "image/png").await?,
        "dimension_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&jpeg, "image/jpeg").await?,
        "dimension_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&webp, "image/webp").await?,
        "dimension_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&gif, "image/gif").await?,
        "dimension_limit_exceeded",
    );
    Ok(())
}

#[tokio::test]
async fn pixel_bombs_are_reported_as_pixel_limit_exceeded() -> Result<(), Box<dyn Error>> {
    let png = MemorySource::new(fixtures::pixel_bomb(FixtureFormat::Png)?);
    let jpeg = MemorySource::new(fixtures::pixel_bomb(FixtureFormat::Jpeg)?);
    let webp = MemorySource::new(fixtures::pixel_bomb(FixtureFormat::WebP)?);
    let gif = MemorySource::new(fixtures::pixel_bomb(FixtureFormat::Gif)?);

    support::assert_malformed_reason(
        support::inspect(&png, "image/png").await?,
        "pixel_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&jpeg, "image/jpeg").await?,
        "pixel_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&webp, "image/webp").await?,
        "pixel_limit_exceeded",
    );
    support::assert_malformed_reason(
        support::inspect(&gif, "image/gif").await?,
        "pixel_limit_exceeded",
    );
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
async fn insufficient_validation_prefix_rejects_incomplete_headers() -> Result<(), Box<dyn Error>> {
    let format = FixtureFormat::Png;
    let source = MemorySource::new(fixtures::valid(format)?);
    let ceilings = FileMediaCeilings {
        validation_source_bytes: 16,
        ..FileMediaCeilings::version_one()
    };

    let inspection = support::inspect_with_ceilings(&source, format.media_type(), ceilings).await?;
    support::assert_malformed_reason(inspection, "malformed_image");
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

#[tokio::test]
async fn missing_image_metadata_fails_without_decoding_again() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::valid(FixtureFormat::Png)?);
    let result = support::read(
        &source,
        "image/png",
        &DirectProcessor::with_missing_metadata(),
    )
    .await;
    assert_eq!(
        result,
        Err(signalbox_file_media_runtime::FileMediaFailure::ProcessorFailed)
    );
    Ok(())
}
