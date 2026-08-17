//! Data-only SVG interpretation inside the supervised file-media worker.

use std::{borrow::Cow, error::Error, num::NonZeroU64, str::FromStr};

use quick_xml::{
    NsReader,
    escape::unescape,
    events::{BytesDecl, BytesStart, Event},
    name::ResolveResult,
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
/// Hard safety ceiling bounding parser work during structural recognition.
const PROBE_BYTES: u64 = 65_536;
/// Hard safety ceiling bounding memory and CPU consumed by full SVG parsing.
const SOURCE_BYTES: u64 = 256 * 1024;
/// Hard safety ceiling bounding memory retained for extracted text.
const TEXT_OUTPUT_BYTES: usize = 128 * 1024;
/// Hard safety ceiling bounding serialized structured output.
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
/// Hard safety ceiling preventing adversarial nesting from exhausting the stack.
const MAX_DEPTH: usize = 128;
/// Hard safety ceiling bounding per-document element processing.
const MAX_ELEMENTS: usize = 10_000;
/// Hard safety ceiling bounding aggregate attribute processing.
const MAX_ATTRIBUTES: usize = 50_000;
/// Hard safety ceiling bounding allocation and scanning for one attribute value.
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
            match parse_svg(&bytes, ParseMode::MetadataOnly) {
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
            if request.detected_media_type.as_str() != MEDIA_TYPE {
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
            let mode = if request.view.as_str() == TEXT_VIEW {
                ParseMode::CollectText
            } else {
                ParseMode::MetadataOnly
            };
            let parsed = match parse_svg(&bytes, mode) {
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
enum ParseMode {
    MetadataOnly,
    CollectText,
}

impl ParseMode {
    const fn collects_text(self) -> bool {
        matches!(self, Self::CollectText)
    }
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
    let mut reader = NsReader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut declaration_is_utf8 = true;
    loop {
        match reader.read_resolved_event_into(&mut buffer) {
            Ok((namespace, Event::Start(start) | Event::Empty(start))) => {
                if start.local_name().as_ref() != b"svg" {
                    return ProbeRoot::Other;
                }
                return if declaration_is_utf8 && is_svg_namespace(&namespace) {
                    ProbeRoot::Svg
                } else {
                    ProbeRoot::MalformedSvg
                };
            }
            Ok((_, Event::Decl(declaration))) => {
                declaration_is_utf8 = declaration_uses_utf8(&declaration).unwrap_or(false);
            }
            Ok((_, Event::DocType(_) | Event::PI(_) | Event::GeneralRef(_) | Event::CData(_))) => {
                return ProbeRoot::Other;
            }
            Ok((_, Event::Text(text))) => match text.xml10_content() {
                Ok(text) if text.trim().is_empty() => {}
                _ => return ProbeRoot::Other,
            },
            Ok((_, Event::Eof)) | Err(_) => return ProbeRoot::Other,
            _ => {}
        }
        buffer.clear();
    }
}

fn parse_svg(bytes: &[u8], mode: ParseMode) -> Result<ParsedSvg, ParseIssue> {
    std::str::from_utf8(bytes).map_err(|_| ParseIssue::Malformed)?;
    let mut reader = NsReader::from_reader(bytes);
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
    let mut declaration_seen = false;
    let mut prolog_event_seen = false;
    loop {
        match reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| ParseIssue::Malformed)?
        {
            (namespace, Event::Start(start)) => {
                prolog_event_seen = true;
                inspect_element(
                    &start,
                    &namespace,
                    depth,
                    &mut attributes,
                    &mut parsed,
                    root_seen,
                )?;
                root_seen = true;
                depth = depth.checked_add(1).ok_or(ParseIssue::StructureLimit)?;
                if depth > MAX_DEPTH {
                    return Err(ParseIssue::StructureLimit);
                }
                if is_svg_element(&namespace, start.local_name().as_ref(), b"text") {
                    text_depth = text_depth
                        .checked_add(1)
                        .ok_or(ParseIssue::StructureLimit)?;
                }
            }
            (namespace, Event::Empty(empty)) => {
                prolog_event_seen = true;
                inspect_element(
                    &empty,
                    &namespace,
                    depth,
                    &mut attributes,
                    &mut parsed,
                    root_seen,
                )?;
                if depth == 0 {
                    root_seen = true;
                    root_closed = true;
                }
            }
            (namespace, Event::End(end)) => {
                if depth == 0 {
                    return Err(ParseIssue::Malformed);
                }
                if is_svg_element(&namespace, end.local_name().as_ref(), b"text") {
                    text_depth = text_depth.checked_sub(1).ok_or(ParseIssue::Malformed)?;
                    if mode.collects_text() && !parsed.text.ends_with('\n') {
                        append_text(&mut parsed.text, "\n")?;
                    }
                }
                depth -= 1;
                if depth == 0 {
                    root_closed = true;
                }
            }
            (_, Event::Text(text)) => {
                let decoded = text.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if depth == 0 && !decoded.trim().is_empty() {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0 {
                    prolog_event_seen = true;
                }
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &decoded)?;
                }
            }
            (_, Event::CData(cdata)) => {
                if depth == 0 {
                    return Err(ParseIssue::Malformed);
                }
                let decoded = cdata.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &decoded)?;
                }
            }
            (_, Event::GeneralRef(reference)) => {
                if depth == 0 {
                    return Err(ParseIssue::Malformed);
                }
                let decoded = reference.decode().map_err(|_| ParseIssue::Malformed)?;
                let value = decode_reference(decoded.as_ref())?;
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &value)?;
                }
            }
            (_, Event::Decl(declaration))
                if !root_seen && !declaration_seen && !prolog_event_seen =>
            {
                declaration_seen = true;
                validate_declaration(&declaration)?;
            }
            (_, Event::Comment(comment)) => {
                if comment.as_ref().windows(2).any(|window| window == b"--") {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0 {
                    prolog_event_seen = true;
                }
            }
            (_, Event::Decl(_)) => return Err(ParseIssue::Malformed),
            (_, Event::DocType(_)) => return Err(ParseIssue::Malformed),
            (_, Event::PI(_)) => return Err(ParseIssue::ActiveContent),
            (_, Event::Eof) if root_seen && root_closed && depth == 0 => break,
            (_, Event::Eof) => return Err(ParseIssue::Malformed),
        }
        buffer.clear();
    }
    Ok(parsed)
}

fn inspect_element(
    element: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    depth: usize,
    attribute_count: &mut usize,
    parsed: &mut ParsedSvg,
    root_seen: bool,
) -> Result<(), ParseIssue> {
    if depth == 0 && root_seen {
        return Err(ParseIssue::Malformed);
    }
    if matches!(namespace, ResolveResult::Unbound) {
        return Err(ParseIssue::Malformed);
    }
    let binding = element.local_name();
    let name = binding.as_ref();
    if depth == 0 {
        if !is_svg_element(namespace, name, b"svg") {
            return Err(ParseIssue::Malformed);
        }
    } else if is_svg_element(namespace, name, b"svg") {
        return Err(ParseIssue::NestedSvg);
    }
    if is_svg_namespace(namespace) && active_element(name) {
        return Err(ParseIssue::ActiveContent);
    }
    if is_svg_namespace(namespace) && resource_element(name) {
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
        let key_binding = attribute.key.local_name();
        let key = key_binding.as_ref();
        let raw_value =
            std::str::from_utf8(attribute.value.as_ref()).map_err(|_| ParseIssue::Malformed)?;
        if raw_value.contains('<') || !has_only_xml10_characters(raw_value) {
            return Err(ParseIssue::Malformed);
        }
        let value = unescape(raw_value).map_err(|_| ParseIssue::Malformed)?;
        let value = value.as_ref();
        if !has_only_xml10_characters(value) {
            return Err(ParseIssue::Malformed);
        }
        if event_handler_attribute(key) || key == b"style" {
            return Err(ParseIssue::ActiveContent);
        }
        if key == b"href"
            || contains_ascii_case_insensitive(value.as_bytes(), b"url(")
            || (resource_capable_attribute(key) && value.contains('\\'))
        {
            return Err(ParseIssue::ExternalReference);
        }
        if depth == 0 {
            match key {
                b"width" => parsed.width = parse_dimension(value)?,
                b"height" => parsed.height = parse_dimension(value)?,
                b"viewBox" => parsed.view_box = Some(parse_view_box(value)?),
                _ => {}
            }
        }
    }
    Ok(())
}

fn is_svg_namespace(namespace: &ResolveResult<'_>) -> bool {
    matches!(
        namespace,
        ResolveResult::Bound(namespace) if namespace.as_ref() == SVG_NAMESPACE
    )
}

fn is_svg_element(namespace: &ResolveResult<'_>, local_name: &[u8], expected: &[u8]) -> bool {
    is_svg_namespace(namespace) && local_name == expected
}

fn active_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"script"
            | b"style"
            | b"foreignObject"
            | b"iframe"
            | b"object"
            | b"embed"
            | b"animate"
            | b"animateMotion"
            | b"animateTransform"
            | b"set"
            | b"discard"
    )
}

fn resource_element(name: &[u8]) -> bool {
    matches!(name, b"image" | b"audio" | b"video")
}

fn resource_capable_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"fill"
            | b"stroke"
            | b"filter"
            | b"clip-path"
            | b"mask"
            | b"cursor"
            | b"marker"
            | b"marker-start"
            | b"marker-mid"
            | b"marker-end"
    )
}

fn event_handler_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"onabort"
            | b"onactivate"
            | b"onbegin"
            | b"oncancel"
            | b"oncanplay"
            | b"oncanplaythrough"
            | b"onchange"
            | b"onclick"
            | b"onclose"
            | b"oncopy"
            | b"oncuechange"
            | b"oncut"
            | b"ondblclick"
            | b"ondrag"
            | b"ondragend"
            | b"ondragenter"
            | b"ondragexit"
            | b"ondragleave"
            | b"ondragover"
            | b"ondragstart"
            | b"ondrop"
            | b"ondurationchange"
            | b"onemptied"
            | b"onend"
            | b"onended"
            | b"onerror"
            | b"onfocus"
            | b"onfocusin"
            | b"onfocusout"
            | b"oninput"
            | b"oninvalid"
            | b"onkeydown"
            | b"onkeypress"
            | b"onkeyup"
            | b"onload"
            | b"onloadeddata"
            | b"onloadedmetadata"
            | b"onloadstart"
            | b"onmousedown"
            | b"onmouseenter"
            | b"onmouseleave"
            | b"onmousemove"
            | b"onmouseout"
            | b"onmouseover"
            | b"onmouseup"
            | b"onmousewheel"
            | b"onpause"
            | b"onplay"
            | b"onplaying"
            | b"onpointercancel"
            | b"onpointerdown"
            | b"onpointerenter"
            | b"onpointerleave"
            | b"onpointermove"
            | b"onpointerout"
            | b"onpointerover"
            | b"onpointerup"
            | b"onprogress"
            | b"onratechange"
            | b"onrepeat"
            | b"onreset"
            | b"onresize"
            | b"onscroll"
            | b"onseeked"
            | b"onseeking"
            | b"onselect"
            | b"onshow"
            | b"onstalled"
            | b"onsubmit"
            | b"onsuspend"
            | b"ontimeupdate"
            | b"ontoggle"
            | b"ontouchcancel"
            | b"ontouchend"
            | b"ontouchmove"
            | b"ontouchstart"
            | b"onunload"
            | b"onvolumechange"
            | b"onwaiting"
            | b"onwheel"
            | b"onzoom"
    )
}

fn parse_dimension(value: &str) -> Result<Option<f64>, ParseIssue> {
    if let Ok(parsed) = parse_nonnegative_finite(value) {
        return Ok(Some(parsed));
    }
    if let Some(number) = value.strip_suffix("px") {
        return parse_nonnegative_finite(number).map(Some);
    }
    for unit in ["%", "em", "ex", "in", "cm", "mm", "pt", "pc"] {
        if let Some(number) = value.strip_suffix(unit) {
            parse_nonnegative_finite(number)?;
            return Ok(None);
        }
    }
    Err(ParseIssue::Malformed)
}

fn parse_nonnegative_finite(value: &str) -> Result<f64, ParseIssue> {
    let parsed = value.parse::<f64>().map_err(|_| ParseIssue::Malformed)?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err(ParseIssue::Malformed)
    }
}

fn parse_view_box(value: &str) -> Result<[f64; 4], ParseIssue> {
    let comma_groups: Vec<&str> = value.split(',').collect();
    if comma_groups.iter().any(|group| group.trim().is_empty()) {
        return Err(ParseIssue::Malformed);
    }
    let mut values = comma_groups
        .into_iter()
        .flat_map(str::split_ascii_whitespace)
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

fn declaration_uses_utf8(declaration: &BytesDecl<'_>) -> Result<bool, ()> {
    match declaration.encoding() {
        None => Ok(true),
        Some(Ok(encoding)) => Ok(encoding.as_ref().eq_ignore_ascii_case(b"utf-8")
            || encoding.as_ref().eq_ignore_ascii_case(b"utf8")),
        Some(Err(_)) => Err(()),
    }
}

fn validate_declaration(declaration: &BytesDecl<'_>) -> Result<(), ParseIssue> {
    let mut input = declaration.as_ref();
    input = input.strip_prefix(b"xml").ok_or(ParseIssue::Malformed)?;
    if !input.first().is_some_and(u8::is_ascii_whitespace) {
        return Err(ParseIssue::Malformed);
    }

    let mut attributes = Vec::new();
    while !input.is_empty() {
        input = trim_ascii_start(input);
        if input.is_empty() {
            break;
        }
        let name_end = input
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || *byte == b'=')
            .ok_or(ParseIssue::Malformed)?;
        let name = &input[..name_end];
        input = trim_ascii_start(&input[name_end..]);
        input = input.strip_prefix(b"=").ok_or(ParseIssue::Malformed)?;
        input = trim_ascii_start(input);
        let quote = *input.first().ok_or(ParseIssue::Malformed)?;
        if quote != b'\'' && quote != b'"' {
            return Err(ParseIssue::Malformed);
        }
        input = &input[1..];
        let value_end = input
            .iter()
            .position(|byte| *byte == quote)
            .ok_or(ParseIssue::Malformed)?;
        attributes.push((name, &input[..value_end]));
        input = &input[value_end + 1..];
        if !input.is_empty() && !input.first().is_some_and(u8::is_ascii_whitespace) {
            return Err(ParseIssue::Malformed);
        }
    }

    let Some((version_name, version)) = attributes.first() else {
        return Err(ParseIssue::Malformed);
    };
    if *version_name != b"version" || *version != b"1.0" {
        return Err(ParseIssue::Malformed);
    }
    let mut index = 1;
    if let Some((name, encoding)) = attributes.get(index)
        && *name == b"encoding"
    {
        if !encoding.eq_ignore_ascii_case(b"utf-8") && !encoding.eq_ignore_ascii_case(b"utf8") {
            return Err(ParseIssue::Malformed);
        }
        index += 1;
    }
    if let Some((name, standalone)) = attributes.get(index) {
        if *name != b"standalone" || !matches!(*standalone, b"yes" | b"no") {
            return Err(ParseIssue::Malformed);
        }
        index += 1;
    }
    if index != attributes.len() {
        return Err(ParseIssue::Malformed);
    }
    Ok(())
}

fn trim_ascii_start(mut input: &[u8]) -> &[u8] {
    while input.first().is_some_and(u8::is_ascii_whitespace) {
        input = &input[1..];
    }
    input
}

fn decode_reference(reference: &str) -> Result<Cow<'_, str>, ParseIssue> {
    let builtin = match reference {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "apos" => Some("'"),
        "quot" => Some("\""),
        _ => None,
    };
    if let Some(value) = builtin {
        return Ok(Cow::Borrowed(value));
    }
    let codepoint = if let Some(hexadecimal) = reference.strip_prefix("#x") {
        u32::from_str_radix(hexadecimal, 16).map_err(|_| ParseIssue::Malformed)?
    } else if let Some(decimal) = reference.strip_prefix('#') {
        decimal.parse::<u32>().map_err(|_| ParseIssue::Malformed)?
    } else {
        return Err(ParseIssue::Malformed);
    };
    let character = char::from_u32(codepoint).ok_or(ParseIssue::Malformed)?;
    let mut encoded = [0; 4];
    if !has_only_xml10_characters(character.encode_utf8(&mut encoded)) {
        return Err(ParseIssue::Malformed);
    }
    Ok(Cow::Owned(character.to_string()))
}

fn has_only_xml10_characters(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
            || ('\u{20}'..='\u{d7ff}').contains(&character)
            || ('\u{e000}'..='\u{fffd}').contains(&character)
            || ('\u{10000}'..='\u{10ffff}').contains(&character)
    })
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
