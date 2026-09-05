//! Isolated adapters for PNG, JPEG, WebP, and GIF bytes.

mod adapter;
mod source;

use std::{error::Error, str::FromStr};

use image::ImageFormat;
use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFailure, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeDeclarationInput, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationDeclaration, VerifiedBlobSource,
};
pub use signalbox_file_media_runtime::{
    MAX_DECODED_IMAGE_PIXELS as MAX_IMAGE_DECODED_PIXELS, MAX_IMAGE_AXIS,
};

const PROVIDER_NAME: &str = "signalbox_image";
const READER_REVISION: &str = "v1";
pub(crate) const METADATA_VIEW_NAME: &str = "metadata";
pub(crate) const MALFORMED_IMAGE_REASON: &str = "malformed_image";
pub(crate) const SOURCE_TOO_LARGE_REASON: &str = "source_too_large";
pub(crate) const DIMENSION_LIMIT_EXCEEDED_REASON: &str = "dimension_limit_exceeded";
pub(crate) const PIXEL_LIMIT_EXCEEDED_REASON: &str = "pixel_limit_exceeded";

/// Maximum encoded bytes one image adapter accepts.
// numeric-bound: ceiling - protects worker memory and decode latency from oversized inputs
pub const MAX_IMAGE_SOURCE_BYTES: u64 = 262_144;

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdapterFormat {
    reader_name: &'static str,
    media_type: &'static str,
    image_format: ImageFormat,
}

impl AdapterFormat {
    const fn reader_name(self) -> &'static str {
        self.reader_name
    }

    const fn media_type(self) -> &'static str {
        self.media_type
    }

    const fn image_format(self) -> ImageFormat {
        self.image_format
    }
}

const ADAPTER_FORMATS: [AdapterFormat; 4] = [
    AdapterFormat {
        reader_name: "png",
        media_type: "image/png",
        image_format: ImageFormat::Png,
    },
    AdapterFormat {
        reader_name: "jpeg",
        media_type: "image/jpeg",
        image_format: ImageFormat::Jpeg,
    },
    AdapterFormat {
        reader_name: "webp",
        media_type: "image/webp",
        image_format: ImageFormat::WebP,
    },
    AdapterFormat {
        reader_name: "gif",
        media_type: "image/gif",
        image_format: ImageFormat::Gif,
    },
];

/// Compiled provider for the four version-one image readers.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageFamilyProvider;

impl FileMediaProvider for ImageFamilyProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        match image_family_declaration() {
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
            adapter::probe(format, source, cancellation)
                .await
                .map_err(|_| FileMediaProviderFailure::Failed)
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
            adapter::inspect(format, request, source, cancellation)
                .await
                .map_err(|_| FileMediaProviderFailure::Failed)
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
            adapter::read(format, request, source, cancellation)
                .await
                .map_err(|_| FileMediaProviderFailure::Failed)
        })
    }
}

/// Builds the exact declaration registered by the image-family worker.
pub fn image_family_declaration()
-> Result<FileMediaProviderDeclaration, Box<dyn Error + Send + Sync>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = ADAPTER_FORMATS
        .into_iter()
        .map(|format| reader(&provider, format))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileMediaProviderDeclaration::try_new(provider, readers)?)
}

fn reader(
    provider: &FileReaderProviderName,
    format: AdapterFormat,
) -> Result<ReaderDeclaration, Box<dyn Error + Send + Sync>> {
    let reasons = [
        MALFORMED_IMAGE_REASON,
        SOURCE_TOO_LARGE_REASON,
        DIMENSION_LIMIT_EXCEEDED_REASON,
        PIXEL_LIMIT_EXCEEDED_REASON,
    ]
    .into_iter()
    .map(ReasonCode::try_new)
    .collect::<Result<Vec<_>, _>>()?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(format.reader_name())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(format.media_type())?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 16,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 16,
        }),
        validation: ValidationDeclaration::new(MAX_IMAGE_SOURCE_BYTES, 1),
        views: vec![metadata_view()?],
        reason_codes: reasons,
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?)
}

fn metadata_view() -> Result<ReadViewDeclaration, Box<dyn Error + Send + Sync>> {
    Ok(ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW_NAME)?,
        String::from("Decodes the primary raster and returns dimensions and channel count."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Structured {
            source_bytes: MAX_IMAGE_SOURCE_BYTES,
            output_bytes: 256,
            depth: 2,
            nodes: 8,
            string_bytes: 64,
        },
    )?)
}

fn format_for_reader(reader: &ReaderIdentity) -> Result<AdapterFormat, FileMediaProviderFailure> {
    ADAPTER_FORMATS
        .into_iter()
        .find(|format| format.reader_name() == reader.reader().as_str())
        .ok_or(FileMediaProviderFailure::Failed)
}

fn options_are_empty(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}
