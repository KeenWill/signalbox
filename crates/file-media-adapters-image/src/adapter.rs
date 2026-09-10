use std::io::Cursor;

use image::{GenericImageView, ImageDecoder, ImageReader, Limits};
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
};

use crate::{
    AdapterFormat, DIMENSION_LIMIT_EXCEEDED_REASON, MALFORMED_IMAGE_REASON, MAX_IMAGE_AXIS,
    MAX_IMAGE_DECODED_PIXELS, METADATA_VIEW_NAME, PIXEL_LIMIT_EXCEEDED_REASON, options_are_empty,
    source,
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
    let bytes =
        source::read_inspection_prefix(source, cancellation, request.maximum_source_bytes).await?;
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format.image_format());
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION_BYTES);
    reader.limits(limits);
    let decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(_) => return Ok(malformed(format, MALFORMED_IMAGE_REASON)),
    };
    let (width, height) = decoder.dimensions();
    if width > request.maximum_image_axis.min(MAX_IMAGE_AXIS)
        || height > request.maximum_image_axis.min(MAX_IMAGE_AXIS)
    {
        return Ok(malformed(format, DIMENSION_LIMIT_EXCEEDED_REASON));
    }
    if u64::from(width) * u64::from(height)
        > request
            .maximum_decoded_image_pixels
            .min(MAX_IMAGE_DECODED_PIXELS)
    {
        return Ok(malformed(format, PIXEL_LIMIT_EXCEEDED_REASON));
    }
    let metadata = ImageMetadata {
        width,
        height,
        channels: decoder.color_type().channel_count(),
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
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    if request.detected_media_type.as_str() != format.media_type() {
        return Err(ProcessorFailure::Protocol);
    }
    let metadata = request.metadata.value();
    let metadata = ImageMetadata {
        width: metadata
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(ProcessorFailure::Protocol)?,
        height: metadata
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(ProcessorFailure::Protocol)?,
        channels: metadata
            .get("channels")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or(ProcessorFailure::Protocol)?,
    };
    if request.view.as_str() == METADATA_VIEW_NAME {
        if !options_are_empty(options) {
            return Ok(ProcessorReadOutput::InvalidViewArguments);
        }
        return Ok(ProcessorReadOutput::Structured {
            body_json: metadata_json(metadata)?,
            truncated: false,
            cursor: None,
        });
    }
    if request.view.as_str() == crate::IMAGE_VIEW_NAME {
        if !options_are_empty(options) {
            return Ok(ProcessorReadOutput::InvalidViewArguments);
        }
        if source.byte_length().get() > signalbox_file_media_runtime::MAX_PRESENTED_IMAGE_BYTES
            || metadata.width > request.maximum_image_axis
            || metadata.height > request.maximum_image_axis
            || u64::from(metadata.width) * u64::from(metadata.height)
                > request.maximum_decoded_image_pixels
        {
            return Ok(ProcessorReadOutput::ImageDescription);
        }
        let decoded = tokio::task::block_in_place(|| decode(format, source, &request, metadata));
        return match decoded {
            Ok(_) => Ok(ProcessorReadOutput::DirectReference {
                media_type: String::from(format.media_type()),
            }),
            Err(MALFORMED_IMAGE_REASON) => Err(ProcessorFailure::Failed),
            Err(reason) => Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: reason.into(),
            }),
        };
    }
    let transform = match crate::transform::Transform::parse(
        request.view.as_str(),
        options,
        (metadata.width, metadata.height),
    ) {
        Some(transform) => transform,
        None => return Ok(ProcessorReadOutput::InvalidViewArguments),
    };
    tokio::task::block_in_place(|| {
        let image = match decode(format, source, &request, metadata) {
            Ok(image) => image,
            Err(MALFORMED_IMAGE_REASON) => return Err(ProcessorFailure::Failed),
            Err(reason) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: reason.into(),
                });
            }
        };
        let Some(bytes) = transform.encode(image) else {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        };
        Ok(ProcessorReadOutput::GeneratedImage {
            media_type: String::from("image/png"),
            provider: String::from(crate::PROVIDER_NAME),
            reader: String::from("png"),
            revision: String::from(crate::READER_REVISION),
            byte_length: bytes.len() as u64,
            bytes,
        })
    })
}

fn decode(
    format: AdapterFormat,
    source: &dyn VerifiedBlobSource,
    request: &FileMediaProviderReadRequest,
    metadata: ImageMetadata,
) -> Result<image::DynamicImage, &'static str> {
    let maximum_axis = request.maximum_image_axis.min(MAX_IMAGE_AXIS);
    let maximum_pixels = request
        .maximum_decoded_image_pixels
        .min(MAX_IMAGE_DECODED_PIXELS);
    if metadata.width > maximum_axis || metadata.height > maximum_axis {
        return Err(DIMENSION_LIMIT_EXCEEDED_REASON);
    }
    if u64::from(metadata.width) * u64::from(metadata.height) > maximum_pixels {
        return Err(PIXEL_LIMIT_EXCEEDED_REASON);
    }
    let input = std::io::BufReader::with_capacity(
        signalbox_file_media_runtime::MAX_PROBE_PREFIX_BYTES as usize,
        source::DecoderSource::new(source),
    );
    let mut reader = ImageReader::with_format(input, format.image_format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(maximum_axis);
    limits.max_image_height = Some(maximum_axis);
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| MALFORMED_IMAGE_REASON)?;
    if image.dimensions() != (metadata.width, metadata.height) {
        return Err(MALFORMED_IMAGE_REASON);
    }
    Ok(image)
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
