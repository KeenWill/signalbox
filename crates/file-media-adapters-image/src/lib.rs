//! Isolated adapters for PNG, JPEG, WebP, and GIF bytes.

mod adapter;
mod source;

use std::{error::Error, str::FromStr};

use image::ImageFormat;
use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFailure, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ReadAccessPattern, ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration,
    ReaderDeclarationInput, ReaderIdentity, ReasonCode, StreamingTextFallback, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "signalbox_image";
const READER_REVISION: &str = "v1";
const METADATA_VIEW_NAME: &str = "metadata";

/// Maximum encoded bytes one image adapter accepts.
pub const MAX_IMAGE_SOURCE_BYTES: u64 = 262_144;
/// Maximum width or height decoded by an image adapter.
pub const MAX_IMAGE_AXIS: u32 = 8_192;
/// Maximum decoded pixels admitted by an image adapter.
pub const MAX_IMAGE_DECODED_PIXELS: u64 = 16_777_216;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdapterFormat {
    Png,
    Jpeg,
    WebP,
    Gif,
}

impl AdapterFormat {
    const ALL: [Self; 4] = [Self::Png, Self::Jpeg, Self::WebP, Self::Gif];

    const fn reader_name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::WebP => "webp",
            Self::Gif => "gif",
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    const fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::WebP => ImageFormat::WebP,
            Self::Gif => ImageFormat::Gif,
        }
    }

    fn matches_signature(self, prefix: &[u8]) -> bool {
        match self {
            Self::Png => prefix.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => prefix.starts_with(&[0xff, 0xd8, 0xff]),
            Self::WebP => {
                prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP".as_slice())
            }
            Self::Gif => prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a"),
        }
    }
}

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
    let reasons = [
        "malformed_image",
        "source_too_large",
        "dimension_limit_exceeded",
        "pixel_limit_exceeded",
    ]
    .into_iter()
    .map(ReasonCode::try_new)
    .collect::<Result<Vec<_>, _>>()?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(format.reader_name())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(format.media_type())?],
        probe: ProbeDeclaration::new(16, 1, 2, MAX_IMAGE_SOURCE_BYTES),
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
    AdapterFormat::ALL
        .into_iter()
        .find(|format| format.reader_name() == reader.reader().as_str())
        .ok_or(FileMediaProviderFailure::Failed)
}

fn options_are_empty(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}
