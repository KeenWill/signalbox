//! Data-only SVG interpretation inside the supervised file-media worker.

use std::{error::Error, num::NonZeroU64, str::FromStr};

use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};

const MEDIA_TYPE: &str = "image/svg+xml";
const PROVIDER_NAME: &str = "svg";
const READER_NAME: &str = "quick-xml";
const READER_REVISION: &str = "quick-xml-0-41-data-only-v1";
const SVG_NAMESPACE: &[u8] = b"http://www.w3.org/2000/svg";
const TEXT_VIEW: &str = "text";
const METADATA_VIEW: &str = "metadata";
const MALFORMED_REASON: &str = "malformed_svg";
const ACTIVE_CONTENT_REASON: &str = "active_content";
const EXTERNAL_REFERENCE_REASON: &str = "external_reference";
const NESTED_SVG_REASON: &str = "nested_svg";
const STRUCTURE_LIMIT_REASON: &str = "structure_limit";
const SOURCE_SIZE_REASON: &str = "source_size_limit";
const PROBE_BYTES: u64 = 65_536;
const SOURCE_BYTES: u64 = 256 * 1024;
const TEXT_OUTPUT_BYTES: usize = 128 * 1024;
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_ELEMENTS: usize = 10_000;
const MAX_ATTRIBUTES: usize = 50_000;
const MAX_ATTRIBUTE_BYTES: usize = 4_096;

/// SVG adapter registered in the dedicated worker catalog.
#[derive(Clone, Copy, Debug, Default)]
pub struct SvgProvider;

impl SvgProvider {
    /// Constructs the stateless data-only SVG provider.
    pub const fn new() -> Self {
        Self
    }
}

impl FileMediaProvider for SvgProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        declaration().unwrap_or_else(|error| {
            eprintln!("SVG declaration failed: {error}");
            std::process::exit(2);
        })
    }

    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            let length = source.byte_length().get().min(PROBE_BYTES);
            let bytes = read_range(source, 0, length).await?;
            require_active(cancellation)?;
            Ok(match probe_root(&bytes) {
                ProbeRoot::Svg => ProcessorProbeOutput::Candidate {
                    media_type: String::from(MEDIA_TYPE),
                    strength: ProbeStrength::StructuralCandidate,
                },
                ProbeRoot::MalformedSvg => malformed_probe(MALFORMED_REASON),
                ProbeRoot::Other => ProcessorProbeOutput::NoMatch,
            })
        })
    }

    fn inspect<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            if request.media_type.as_str() != MEDIA_TYPE {
                return Err(ProcessorFailure::Protocol);
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(malformed_validation(SOURCE_SIZE_REASON));
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            match parse_svg(&bytes, false) {
                Ok(parsed) => validated_output(request.evidence, &parsed),
                Err(issue) => Ok(malformed_validation(issue.reason())),
            }
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            if request.file.detected_media_type().as_str() != MEDIA_TYPE {
                return Err(ProcessorFailure::Protocol);
            }
            if !empty_options(&request.options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            }
            if source.byte_length().get() > SOURCE_BYTES {
                return Ok(ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: SOURCE_BYTES,
                });
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            let collect_text = request.view.as_str() == TEXT_VIEW;
            let parsed = match parse_svg(&bytes, collect_text) {
                Ok(parsed) => parsed,
                Err(ParseIssue::TextOutput) => return Ok(ProcessorReadOutput::OutputUnitTooLarge),
                Err(_) => return Err(ProcessorFailure::Failed),
            };
            match request.view.as_str() {
                TEXT_VIEW => Ok(ProcessorReadOutput::Text {
                    body: parsed.text,
                    truncated: false,
                    cursor: None,
                }),
                METADATA_VIEW => metadata_output(&parsed),
                _ => Ok(ProcessorReadOutput::UnsupportedView),
            }
        })
    }
}

/// Returns the declaration shared by registration and the worker catalog.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let text_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW)?,
        String::from("Extracts bounded text elements without rendering."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming,
        ReadViewBounds::Text {
            source_bytes: SOURCE_BYTES,
            output_bytes: TEXT_OUTPUT_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded SVG dimensions, view box, and element count."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming,
        ReadViewBounds::Structured {
            source_bytes: SOURCE_BYTES,
            output_bytes: METADATA_OUTPUT_BYTES,
            depth: 4,
            nodes: 64,
            string_bytes: 1024,
        },
    )?;
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(READER_NAME)?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(MEDIA_TYPE)?],
        probe: ProbeDeclaration::new(PROBE_BYTES, 0, 1, SOURCE_BYTES),
        views: vec![text_view, metadata_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(ACTIVE_CONTENT_REASON)?,
            ReasonCode::try_new(EXTERNAL_REFERENCE_REASON)?,
            ReasonCode::try_new(NESTED_SVG_REASON)?,
            ReasonCode::try_new(STRUCTURE_LIMIT_REASON)?,
            ReasonCode::try_new(SOURCE_SIZE_REASON)?,
        ],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?;
    Ok(FileMediaProviderDeclaration::try_new(
        provider,
        vec![reader],
    )?)
}

#[derive(Debug)]
struct ParsedSvg {
    width: Option<f64>,
    height: Option<f64>,
    view_box: Option<[f64; 4]>,
    elements: usize,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseIssue {
    Malformed,
    ActiveContent,
    ExternalReference,
    NestedSvg,
    StructureLimit,
    TextOutput,
}

impl ParseIssue {
    const fn reason(self) -> &'static str {
        match self {
            Self::Malformed | Self::TextOutput => MALFORMED_REASON,
            Self::ActiveContent => ACTIVE_CONTENT_REASON,
            Self::ExternalReference => EXTERNAL_REFERENCE_REASON,
            Self::NestedSvg => NESTED_SVG_REASON,
            Self::StructureLimit => STRUCTURE_LIMIT_REASON,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeRoot {
    Svg,
    MalformedSvg,
    Other,
}

fn probe_root(bytes: &[u8]) -> ProbeRoot {
    if std::str::from_utf8(bytes).is_err() || declares_non_utf8(bytes) {
        return if looks_like_svg(bytes) {
            ProbeRoot::MalformedSvg
        } else {
            ProbeRoot::Other
        };
    }
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start) | Event::Empty(start)) => {
                if local_name(start.name().as_ref()) != b"svg" {
                    return ProbeRoot::Other;
                }
                return if root_has_namespace(&start) {
                    ProbeRoot::Svg
                } else {
                    ProbeRoot::MalformedSvg
                };
            }
            Ok(Event::DocType(_) | Event::PI(_) | Event::GeneralRef(_)) => {
                return if looks_like_svg(bytes) {
                    ProbeRoot::MalformedSvg
                } else {
                    ProbeRoot::Other
                };
            }
            Ok(Event::Text(text)) => match text.xml10_content() {
                Ok(text) if text.trim().is_empty() => {}
                _ => return ProbeRoot::Other,
            },
            Ok(Event::Eof) => {
                return if looks_like_svg(bytes) {
                    ProbeRoot::MalformedSvg
                } else {
                    ProbeRoot::Other
                };
            }
            Err(_) => {
                return if looks_like_svg(bytes) {
                    ProbeRoot::MalformedSvg
                } else {
                    ProbeRoot::Other
                };
            }
            _ => {}
        }
        buffer.clear();
    }
}

fn parse_svg(bytes: &[u8], collect_text: bool) -> Result<ParsedSvg, ParseIssue> {
    std::str::from_utf8(bytes).map_err(|_| ParseIssue::Malformed)?;
    if declares_non_utf8(bytes) {
        return Err(ParseIssue::Malformed);
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut parsed = ParsedSvg {
        width: None,
        height: None,
        view_box: None,
        elements: 0,
        text: String::new(),
    };
    let mut depth = 0_usize;
    let mut attributes = 0_usize;
    let mut text_depth = 0_usize;
    let mut root_seen = false;
    let mut root_closed = false;
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| ParseIssue::Malformed)?
        {
            Event::Start(start) => {
                inspect_element(&start, depth, &mut attributes, &mut parsed, root_seen)?;
                root_seen = true;
                depth = depth.checked_add(1).ok_or(ParseIssue::StructureLimit)?;
                if depth > MAX_DEPTH {
                    return Err(ParseIssue::StructureLimit);
                }
                if local_name(start.name().as_ref()) == b"text" {
                    text_depth = text_depth
                        .checked_add(1)
                        .ok_or(ParseIssue::StructureLimit)?;
                }
            }
            Event::Empty(empty) => {
                inspect_element(&empty, depth, &mut attributes, &mut parsed, root_seen)?;
                if depth == 0 {
                    root_seen = true;
                    root_closed = true;
                }
            }
            Event::End(end) => {
                if depth == 0 {
                    return Err(ParseIssue::Malformed);
                }
                if local_name(end.name().as_ref()) == b"text" {
                    text_depth = text_depth.checked_sub(1).ok_or(ParseIssue::Malformed)?;
                    if collect_text && !parsed.text.ends_with('\n') {
                        append_text(&mut parsed.text, "\n")?;
                    }
                }
                depth -= 1;
                if depth == 0 {
                    root_closed = true;
                }
            }
            Event::Text(text) => {
                let decoded = text.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if depth == 0 && !decoded.trim().is_empty() {
                    return Err(ParseIssue::Malformed);
                }
                if collect_text && text_depth > 0 {
                    append_text(&mut parsed.text, &decoded)?;
                }
            }
            Event::GeneralRef(reference) => {
                let decoded = reference.decode().map_err(|_| ParseIssue::Malformed)?;
                let value = match decoded.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "apos" => "'",
                    "quot" => "\"",
                    _ => return Err(ParseIssue::Malformed),
                };
                if collect_text && text_depth > 0 {
                    append_text(&mut parsed.text, value)?;
                }
            }
            Event::Decl(_) if !root_seen => {}
            Event::Comment(_) => {}
            Event::DocType(_) | Event::PI(_) | Event::CData(_) | Event::Decl(_) => {
                return Err(ParseIssue::ActiveContent);
            }
            Event::Eof if root_seen && root_closed && depth == 0 => break,
            Event::Eof => return Err(ParseIssue::Malformed),
        }
        buffer.clear();
    }
    Ok(parsed)
}

fn inspect_element(
    element: &BytesStart<'_>,
    depth: usize,
    attribute_count: &mut usize,
    parsed: &mut ParsedSvg,
    root_seen: bool,
) -> Result<(), ParseIssue> {
    if depth == 0 && root_seen {
        return Err(ParseIssue::Malformed);
    }
    let binding = element.name();
    let name = local_name(binding.as_ref());
    if depth == 0 {
        if name != b"svg" || !root_has_namespace(element) {
            return Err(ParseIssue::Malformed);
        }
    } else if name == b"svg" {
        return Err(ParseIssue::NestedSvg);
    }
    if active_element(name) {
        return Err(ParseIssue::ActiveContent);
    }
    if resource_element(name) {
        return Err(ParseIssue::ExternalReference);
    }
    parsed.elements = parsed
        .elements
        .checked_add(1)
        .ok_or(ParseIssue::StructureLimit)?;
    if parsed.elements > MAX_ELEMENTS {
        return Err(ParseIssue::StructureLimit);
    }
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ParseIssue::Malformed)?;
        *attribute_count = attribute_count
            .checked_add(1)
            .ok_or(ParseIssue::StructureLimit)?;
        if *attribute_count > MAX_ATTRIBUTES || attribute.value.len() > MAX_ATTRIBUTE_BYTES {
            return Err(ParseIssue::StructureLimit);
        }
        let key = local_name(attribute.key.as_ref());
        let value =
            std::str::from_utf8(attribute.value.as_ref()).map_err(|_| ParseIssue::Malformed)?;
        if value.contains('&') {
            return Err(ParseIssue::Malformed);
        }
        if key.starts_with(b"on") || key == b"style" {
            return Err(ParseIssue::ActiveContent);
        }
        if key == b"href" || contains_ascii_case_insensitive(value.as_bytes(), b"url(") {
            return Err(ParseIssue::ExternalReference);
        }
        if depth == 0 {
            match key {
                b"width" => parsed.width = Some(parse_dimension(value)?),
                b"height" => parsed.height = Some(parse_dimension(value)?),
                b"viewBox" => parsed.view_box = Some(parse_view_box(value)?),
                _ => {}
            }
        }
    }
    Ok(())
}

fn root_has_namespace(element: &BytesStart<'_>) -> bool {
    element
        .attributes()
        .filter_map(Result::ok)
        .any(|attribute| {
            attribute.key.as_ref() == b"xmlns" && attribute.value.as_ref() == SVG_NAMESPACE
        })
}

fn active_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"script" | b"style" | b"foreignObject" | b"iframe" | b"object" | b"embed"
    )
}

fn resource_element(name: &[u8]) -> bool {
    matches!(name, b"image" | b"audio" | b"video")
}

fn parse_dimension(value: &str) -> Result<f64, ParseIssue> {
    let number = value.strip_suffix("px").unwrap_or(value);
    let parsed = number.parse::<f64>().map_err(|_| ParseIssue::Malformed)?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err(ParseIssue::Malformed)
    }
}

fn parse_view_box(value: &str) -> Result<[f64; 4], ParseIssue> {
    let mut values = value
        .split(|character: char| character.is_ascii_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<f64>().map_err(|_| ParseIssue::Malformed));
    let view_box = [
        values.next().ok_or(ParseIssue::Malformed)??,
        values.next().ok_or(ParseIssue::Malformed)??,
        values.next().ok_or(ParseIssue::Malformed)??,
        values.next().ok_or(ParseIssue::Malformed)??,
    ];
    if values.next().is_some()
        || view_box.iter().any(|value| !value.is_finite())
        || view_box[2] <= 0.0
        || view_box[3] <= 0.0
    {
        Err(ParseIssue::Malformed)
    } else {
        Ok(view_box)
    }
}

fn append_text(output: &mut String, value: &str) -> Result<(), ParseIssue> {
    let total = output
        .len()
        .checked_add(value.len())
        .ok_or(ParseIssue::TextOutput)?;
    if total > TEXT_OUTPUT_BYTES {
        return Err(ParseIssue::TextOutput);
    }
    output.push_str(value);
    Ok(())
}

fn validated_output(
    evidence: ValidationEvidence,
    parsed: &ParsedSvg,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    let metadata_json = metadata_json(parsed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(MEDIA_TYPE),
        evidence,
        metadata_json,
    })
}

fn metadata_output(parsed: &ParsedSvg) -> Result<ProcessorReadOutput, ProcessorFailure> {
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(parsed)?,
        truncated: false,
        cursor: None,
    })
}

fn metadata_json(parsed: &ParsedSvg) -> Result<String, ProcessorFailure> {
    serde_json::to_string(&serde_json::json!({
        "elements": parsed.elements,
        "height": parsed.height,
        "view_box": parsed.view_box,
        "width": parsed.width,
    }))
    .map_err(|_| ProcessorFailure::Failed)
}

async fn read_all(source: &dyn VerifiedBlobSource) -> Result<Vec<u8>, ProcessorFailure> {
    read_range(source, 0, source.byte_length().get()).await
}

async fn read_range(
    source: &dyn VerifiedBlobSource,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, ProcessorFailure> {
    let length = NonZeroU64::new(length).ok_or(ProcessorFailure::Failed)?;
    source
        .read_range(offset, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}

fn require_reader(reader: &ReaderIdentity) -> Result<(), ProcessorFailure> {
    if reader.provider().as_str() == PROVIDER_NAME
        && reader.reader().as_str() == READER_NAME
        && reader.revision().as_str() == READER_REVISION
    {
        Ok(())
    } else {
        Err(ProcessorFailure::Protocol)
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), ProcessorFailure> {
    if cancellation.is_cancelled() {
        Err(ProcessorFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"<svg")
}

fn declares_non_utf8(bytes: &[u8]) -> bool {
    let lowered: Vec<u8> = bytes.iter().take(256).map(u8::to_ascii_lowercase).collect();
    lowered.windows(8).any(|window| window == b"encoding")
        && !lowered.windows(5).any(|window| window == b"utf-8")
        && !lowered.windows(4).any(|window| window == b"utf8")
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn malformed_probe(reason: &str) -> ProcessorProbeOutput {
    ProcessorProbeOutput::RecognizedMalformed {
        media_type: String::from(MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}

fn malformed_validation(reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}
