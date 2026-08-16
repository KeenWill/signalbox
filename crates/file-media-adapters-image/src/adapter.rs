use std::io::Cursor;

use image::{GenericImageView, ImageReader, Limits};
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
};

use crate::{
    AdapterFormat, MAX_IMAGE_AXIS, MAX_IMAGE_DECODED_PIXELS, MAX_IMAGE_SOURCE_BYTES,
    options_are_empty, source,
};

const MAX_DECODER_ALLOCATION_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageMetadata {
    width: u32,
    height: u32,
    channels: u8,
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
            maximum_bytes: MAX_IMAGE_SOURCE_BYTES,
        });
    };
    let metadata = decode(format, &bytes).map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(metadata)?,
        truncated: false,
        cursor: None,
    })
}

fn decode(format: AdapterFormat, bytes: &[u8]) -> Result<ImageMetadata, &'static str> {
    let dimensions = ImageReader::with_format(Cursor::new(bytes), format.image_format())
        .into_dimensions()
        .map_err(|_| "malformed_image")?;
    if dimensions.0 > MAX_IMAGE_AXIS || dimensions.1 > MAX_IMAGE_AXIS {
        return Err("dimension_limit_exceeded");
    }
    let pixels = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .ok_or("pixel_limit_exceeded")?;
    if pixels > MAX_IMAGE_DECODED_PIXELS {
        return Err("pixel_limit_exceeded");
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.image_format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_AXIS);
    limits.max_image_height = Some(MAX_IMAGE_AXIS);
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| "malformed_image")?;
    let decoded_dimensions = image.dimensions();
    if decoded_dimensions != dimensions {
        return Err("malformed_image");
    }
    Ok(ImageMetadata {
        width: dimensions.0,
        height: dimensions.1,
        channels: image.color().channel_count(),
    })
}

fn metadata_json(metadata: ImageMetadata) -> Result<String, ProcessorFailure> {
    serde_json::to_string(&serde_json::json!({
        "channels": metadata.channels,
        "height": metadata.height,
        "width": metadata.width,
    }))
    .map_err(|_| ProcessorFailure::Failed)
}

fn malformed(format: AdapterFormat, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(format.media_type()),
        reason_code: String::from(reason),
    }
}
