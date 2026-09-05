use std::io::Cursor;

use image::{GenericImageView, ImageReader, Limits};
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
};

use crate::{
    AdapterFormat, DIMENSION_LIMIT_EXCEEDED_REASON, MALFORMED_IMAGE_REASON, MAX_IMAGE_AXIS,
    MAX_IMAGE_DECODED_PIXELS, MAX_IMAGE_SOURCE_BYTES, METADATA_VIEW_NAME,
    PIXEL_LIMIT_EXCEEDED_REASON, SOURCE_TOO_LARGE_REASON, options_are_empty, source,
};

// numeric-bound: ceiling - protects worker memory from runaway decoder allocation
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
    if image::guess_format(&prefix).ok() == Some(format.image_format()) {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(format.media_type()),
            strength: ProbeStrength::Strong,
            evidence_bytes: u64::try_from(prefix.len()).map_err(|_| ProcessorFailure::Failed)?,
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
    let Some(bytes) =
        source::read_complete(source, cancellation, request.maximum_source_bytes).await?
    else {
        return Ok(malformed(format, SOURCE_TOO_LARGE_REASON));
    };
    let metadata = match decode(
        format,
        &bytes,
        request.maximum_image_axis,
        request.maximum_decoded_image_pixels,
    ) {
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
    let signalbox_file_media_runtime::FileReadInput::Initial { options } = &request.input else {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    };
    if request.view.as_str() != METADATA_VIEW_NAME || !options_are_empty(options) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation, MAX_IMAGE_SOURCE_BYTES).await?
    else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_IMAGE_SOURCE_BYTES,
        });
    };
    let metadata = decode(
        format,
        &bytes,
        request.maximum_image_axis,
        request.maximum_decoded_image_pixels,
    )
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(metadata)?,
        truncated: false,
        cursor: None,
    })
}

fn decode(
    format: AdapterFormat,
    bytes: &[u8],
    maximum_axis: u32,
    maximum_pixels: u64,
) -> Result<ImageMetadata, &'static str> {
    let dimensions = ImageReader::with_format(Cursor::new(bytes), format.image_format())
        .into_dimensions()
        .map_err(|_| MALFORMED_IMAGE_REASON)?;
    let maximum_axis = maximum_axis.min(MAX_IMAGE_AXIS);
    let maximum_pixels = maximum_pixels.min(MAX_IMAGE_DECODED_PIXELS);
    if dimensions.0 > maximum_axis || dimensions.1 > maximum_axis {
        return Err(DIMENSION_LIMIT_EXCEEDED_REASON);
    }
    let pixels = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .ok_or(PIXEL_LIMIT_EXCEEDED_REASON)?;
    if pixels > maximum_pixels {
        return Err(PIXEL_LIMIT_EXCEEDED_REASON);
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.image_format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(maximum_axis);
    limits.max_image_height = Some(maximum_axis);
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| MALFORMED_IMAGE_REASON)?;
    let decoded_dimensions = image.dimensions();
    if decoded_dimensions != dimensions {
        return Err(MALFORMED_IMAGE_REASON);
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
