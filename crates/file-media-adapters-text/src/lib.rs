//! Isolated adapters for UTF-8 text, JSON, and CSV bytes.

mod csv_adapter;
mod json_adapter;
mod source;
mod text_adapter;

use std::{error::Error, str::FromStr};

use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFailure, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReadInput, FileReaderName, FileReaderProviderName,
    FileReaderRevision, MAX_STRUCTURED_DEPTH, MAX_STRUCTURED_NODES, ProbeDeclaration,
    ProbeDeclarationInput, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, VerifiedBlobSource,
};

const PROVIDER_NAME: &str = "signalbox_text";
const TEXT_READER_NAME: &str = "utf8_text";
const JSON_READER_NAME: &str = "json";
const CSV_READER_NAME: &str = "csv";
const READER_REVISION: &str = "v1";
const TEXT_MEDIA_TYPE: &str = "text/plain";
const JSON_MEDIA_TYPE: &str = "application/json";
const CSV_MEDIA_TYPE: &str = "text/csv";
pub(crate) const TEXT_VIEW_NAME: &str = "text";
pub(crate) const STRUCTURED_VIEW_NAME: &str = "structured";
// Tunable effective ceiling; bounds detection I/O while retaining useful structure evidence.
const PROBE_PREFIX_BYTES: u64 = 4_096;

/// Hard safety ceiling; bounds whole-source parsing and result allocation.
pub const MAX_TEXT_FAMILY_BYTES: u64 = 131_072;

/// Compiled provider for the three version-one text-family readers.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextFamilyProvider;

impl FileMediaProvider for TextFamilyProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        match text_family_declaration() {
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
            match reader.reader().as_str() {
                TEXT_READER_NAME => text_adapter::probe(source, cancellation).await,
                JSON_READER_NAME => json_adapter::probe(source, cancellation).await,
                CSV_READER_NAME => csv_adapter::probe(source, cancellation).await,
                _ => Err(ProcessorFailure::Protocol),
            }
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
            match reader.reader().as_str() {
                TEXT_READER_NAME => text_adapter::inspect(request, source, cancellation).await,
                JSON_READER_NAME => json_adapter::inspect(request, source, cancellation).await,
                CSV_READER_NAME => csv_adapter::inspect(request, source, cancellation).await,
                _ => Err(ProcessorFailure::Protocol),
            }
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
            match reader.reader().as_str() {
                TEXT_READER_NAME => text_adapter::read(request, source, cancellation).await,
                JSON_READER_NAME => json_adapter::read(request, source, cancellation).await,
                CSV_READER_NAME => csv_adapter::read(request, source, cancellation).await,
                _ => Err(ProcessorFailure::Protocol),
            }
            .map_err(|_| FileMediaProviderFailure::Failed)
        })
    }
}

/// Builds the exact declaration registered by the text-family worker.
pub fn text_family_declaration()
-> Result<FileMediaProviderDeclaration, Box<dyn Error + Send + Sync>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let text = reader(ReaderInput {
        provider: &provider,
        name: TEXT_READER_NAME,
        media_type: TEXT_MEDIA_TYPE,
        view: text_view()?,
        reasons: vec!["invalid_utf8", "nul_byte", "source_too_large"],
        fallback: StreamingTextFallback::Enabled,
    })?;
    let json = reader(ReaderInput {
        provider: &provider,
        name: JSON_READER_NAME,
        media_type: JSON_MEDIA_TYPE,
        view: structured_view("Reads the complete JSON value as bounded structured data.")?,
        reasons: vec![
            "invalid_utf8",
            "nul_byte",
            "malformed_json",
            "source_too_large",
            "depth_limit_exceeded",
            "container_entry_limit_exceeded",
        ],
        fallback: StreamingTextFallback::Disabled,
    })?;
    let csv = reader(ReaderInput {
        provider: &provider,
        name: CSV_READER_NAME,
        media_type: CSV_MEDIA_TYPE,
        view: structured_view("Reads a rectangular CSV table as headers and rows.")?,
        reasons: vec![
            "invalid_utf8",
            "nul_byte",
            "malformed_csv",
            "source_too_large",
            "row_limit_exceeded",
            "column_limit_exceeded",
            "container_entry_limit_exceeded",
        ],
        fallback: StreamingTextFallback::Disabled,
    })?;
    Ok(FileMediaProviderDeclaration::try_new(
        provider,
        vec![text, json, csv],
    )?)
}

struct ReaderInput<'a> {
    provider: &'a FileReaderProviderName,
    name: &'a str,
    media_type: &'a str,
    view: ReadViewDeclaration,
    reasons: Vec<&'a str>,
    fallback: StreamingTextFallback,
}

fn reader(input: ReaderInput<'_>) -> Result<ReaderDeclaration, Box<dyn Error + Send + Sync>> {
    let reason_codes = input
        .reasons
        .into_iter()
        .map(ReasonCode::try_new)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: input.provider.clone(),
        reader: FileReaderName::try_new(input.name)?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(input.media_type)?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: PROBE_PREFIX_BYTES,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: PROBE_PREFIX_BYTES,
        }),
        views: vec![input.view],
        reason_codes,
        streaming_text_fallback: input.fallback,
    })?)
}

fn text_view() -> Result<ReadViewDeclaration, Box<dyn Error + Send + Sync>> {
    Ok(ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW_NAME)?,
        String::from("Reads the complete file as exact UTF-8 text."),
        empty_options_schema()?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Text {
            source_bytes: MAX_TEXT_FAMILY_BYTES,
            output_bytes: MAX_TEXT_FAMILY_BYTES as usize,
        },
    )?)
}

fn structured_view(description: &str) -> Result<ReadViewDeclaration, Box<dyn Error + Send + Sync>> {
    Ok(ReadViewDeclaration::try_new(
        ReadViewName::try_new(STRUCTURED_VIEW_NAME)?,
        String::from(description),
        empty_options_schema()?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Structured {
            source_bytes: MAX_TEXT_FAMILY_BYTES,
            output_bytes: MAX_TEXT_FAMILY_BYTES as usize,
            depth: MAX_STRUCTURED_DEPTH,
            nodes: MAX_STRUCTURED_NODES,
            string_bytes: MAX_TEXT_FAMILY_BYTES as usize,
        },
    )?)
}

fn empty_options_schema() -> Result<CanonicalJsonObjectSchema, Box<dyn Error + Send + Sync>> {
    Ok(CanonicalJsonObjectSchema::try_new(
        r#"{"additionalProperties":false,"type":"object"}"#,
    )?)
}

fn options_are_empty(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn read_input_is_empty(input: &FileReadInput) -> bool {
    match input {
        FileReadInput::Initial { options } => options_are_empty(options),
        FileReadInput::Continuation { .. } => false,
    }
}
