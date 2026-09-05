//! Data-only SVG interpretation inside the supervised file-media worker.
//! Governed by `docs/spec/file-and-media.md`.

use std::{borrow::Cow, collections::HashSet, error::Error, num::NonZeroU64, str::FromStr};

use cssparser::{Parser, ParserInput, Token};
use iri_string::types::IriReferenceStr;
use quick_xml::{
    NsReader, Reader,
    escape::unescape,
    events::{BytesDecl, BytesStart, Event},
    name::{NamespaceResolver, ResolveResult},
};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration, ProbeStrength,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration, ReaderDeclarationInput,
    ReaderIdentity, ReasonCode, StreamingTextFallback, ValidationDeclaration, ValidationEvidence,
    VerifiedBlobSource,
};

const MEDIA_TYPE: &str = "image/svg+xml";
const PROVIDER_NAME: &str = "svg";
const READER_NAME: &str = "quick-xml";
const READER_REVISION: &str = "quick-xml-svgtypes-data-only-v1";
const SVG_NAMESPACE: &[u8] = b"http://www.w3.org/2000/svg";
const XLINK_NAMESPACE: &[u8] = b"http://www.w3.org/1999/xlink";
const XML_EVENTS_NAMESPACE: &[u8] = b"http://www.w3.org/2001/xml-events";
const XML_NAMESPACE: &[u8] = b"http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &[u8] = b"http://www.w3.org/2000/xmlns/";
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
/// Hard safety ceiling preventing untrusted metadata from exceeding its declared nesting shape.
const METADATA_OUTPUT_DEPTH: u32 = 4;
/// Hard safety ceiling bounding the number of untrusted structured-output values.
const METADATA_OUTPUT_NODES: u64 = 64;
/// Hard safety ceiling bounding aggregate string storage in untrusted structured output.
const METADATA_OUTPUT_STRING_BYTES: usize = 1_024;
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
            let bytes = read_range(source, SourceRange { offset: 0, length }).await?;
            require_active(cancellation)?;
            Ok(match probe_root(&bytes) {
                ProbeRoot::Svg => ProcessorProbeOutput::Candidate {
                    media_type: String::from(MEDIA_TYPE),
                    strength: ProbeStrength::StructuralCandidate,
                    evidence_bytes: u64::try_from(bytes.len())
                        .map_err(|_| FileMediaProviderFailure::Failed)?,
                },
                ProbeRoot::MalformedSvg => malformed_probe(MALFORMED_REASON),
                ProbeRoot::ActiveSvg => malformed_probe(ACTIVE_CONTENT_REASON),
                ProbeRoot::Other | ProbeRoot::Indeterminate => ProcessorProbeOutput::NoMatch,
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
                return Err(FileMediaProviderFailure::Failed);
            }
            let maximum_source_bytes = SOURCE_BYTES.min(request.maximum_source_bytes);
            if source.byte_length().get() > maximum_source_bytes {
                let probe_length = source
                    .byte_length()
                    .get()
                    .min(PROBE_BYTES)
                    .min(maximum_source_bytes);
                if probe_length > 0 {
                    let prefix = read_range(
                        source,
                        SourceRange {
                            offset: 0,
                            length: probe_length,
                        },
                    )
                    .await?;
                    // `probe_length` is always a strict prefix here, so an
                    // `Indeterminate` result only proves the read stopped
                    // before reaching a root element; it never proves the
                    // source is not SVG. Only a fully classified non-SVG
                    // root (`ProbeRoot::Other`) is trustworthy enough to
                    // report `NoMatch`, so this still runs bounded
                    // classification whenever the effective ceiling (not
                    // just the adapter's hard ceiling) is exceeded, without
                    // resurrecting the truncation false-negative that would
                    // turn a legitimate oversized SVG into a processor
                    // failure.
                    if matches!(probe_root(&prefix), ProbeRoot::Other) {
                        return Ok(ProcessorValidationOutput::NoMatch);
                    }
                }
                return Ok(malformed_validation(SOURCE_SIZE_REASON));
            }
            let bytes = read_all(source).await?;
            require_active(cancellation)?;
            match parse_svg(&bytes, ParseMode::MetadataOnly) {
                Ok(parsed) => validated_output(request.evidence, &parsed),
                Err(ParseIssue::NoMatch) => Ok(ProcessorValidationOutput::NoMatch),
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
                return Err(FileMediaProviderFailure::Failed);
            }
            if !matches!(
                &request.input,
                FileReadInput::Initial { options } if empty_options(options)
            ) {
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
                Err(_) => return Err(FileMediaProviderFailure::Failed),
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
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Text {
            source_bytes: SOURCE_BYTES,
            output_bytes: TEXT_OUTPUT_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded SVG dimensions, view box, and element count."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Structured {
            source_bytes: SOURCE_BYTES,
            output_bytes: METADATA_OUTPUT_BYTES,
            depth: METADATA_OUTPUT_DEPTH,
            nodes: METADATA_OUTPUT_NODES,
            string_bytes: METADATA_OUTPUT_STRING_BYTES,
        },
    )?;
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(READER_NAME)?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(MEDIA_TYPE)?],
        probe: ProbeDeclaration::prefix_only(PROBE_BYTES),
        // Validation reads at most one range: either the bounded prefix used
        // for oversized-source classification or the single whole-source read,
        // never both.
        validation: ValidationDeclaration::new(SOURCE_BYTES, 1),
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
    NoMatch,
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
            Self::NoMatch => MALFORMED_REASON,
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
    ActiveSvg,
    /// A root element was fully parsed and it is conclusively not `<svg>`.
    Other,
    /// Parsing ended (decode failure, parser error, or EOF) before a root
    /// start/empty element was fully read, so no conclusion could be drawn.
    /// This is expected when probing a length-capped prefix that stops
    /// short of the actual root tag, and must not be treated the same as
    /// `Other`'s definite non-match by callers that reason about truncation.
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XmlEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf16LeBom,
    Utf16BeBom,
}

fn probe_root(bytes: &[u8]) -> ProbeRoot {
    let Ok((document, source_encoding)) = decode_xml(bytes, true) else {
        return ProbeRoot::Indeterminate;
    };
    let mut reader = NsReader::from_reader(document.as_bytes());
    let mut buffer = Vec::new();
    let mut declaration_matches_source = true;
    let mut forbidden_prolog_event = false;
    let mut active_prolog_event = false;
    loop {
        match reader.read_resolved_event_into(&mut buffer) {
            Ok((namespace, Event::Start(start) | Event::Empty(start))) => {
                if start.local_name().as_ref() != b"svg" || !is_svg_namespace(&namespace) {
                    return ProbeRoot::Other;
                }
                return if active_prolog_event {
                    ProbeRoot::ActiveSvg
                } else if declaration_matches_source && !forbidden_prolog_event {
                    ProbeRoot::Svg
                } else {
                    ProbeRoot::MalformedSvg
                };
            }
            Ok((_, Event::Decl(declaration))) => {
                declaration_matches_source =
                    declaration_matches_encoding(&declaration, source_encoding).unwrap_or(false);
            }
            Ok((_, Event::PI(instruction))) => {
                if instruction
                    .as_ref()
                    .get(..3)
                    .is_some_and(|target| target.eq_ignore_ascii_case(b"xml"))
                    && instruction
                        .as_ref()
                        .get(3)
                        .is_none_or(|byte| !is_name_character(char::from(*byte)))
                {
                    forbidden_prolog_event = true;
                } else {
                    active_prolog_event = true;
                }
            }
            Ok((_, Event::DocType(_) | Event::GeneralRef(_) | Event::CData(_))) => {
                forbidden_prolog_event = true;
            }
            Ok((_, Event::Text(text))) => match text.xml10_content() {
                Ok(text) if is_xml_whitespace(&text) => {}
                Ok(_) => forbidden_prolog_event = true,
                Err(_) => return ProbeRoot::Indeterminate,
            },
            Ok((_, Event::Eof)) | Err(_) => return ProbeRoot::Indeterminate,
            _ => {}
        }
        buffer.clear();
    }
}

fn classify_svg_root(bytes: &[u8]) -> Result<bool, ParseIssue> {
    let (document, _) = decode_xml(bytes, true)?;
    let mut reader = Reader::from_reader(document.as_bytes());
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start) | Event::Empty(start)) => {
                if start.local_name().as_ref() != b"svg" {
                    return Ok(false);
                }
                let qualified_name = start.name();
                let qualified_name = qualified_name.as_ref();
                let prefix = qualified_name
                    .iter()
                    .position(|byte| *byte == b':')
                    .map(|index| &qualified_name[..index]);
                let prefixed_namespace =
                    prefix.map(|prefix| [b"xmlns:".as_slice(), prefix].concat());
                let mut default_svg_namespace = false;
                let mut prefixed_svg_namespace = false;
                for attribute in start.attributes() {
                    let Ok(attribute) = attribute else {
                        return Ok(true);
                    };
                    let Some(value) = std::str::from_utf8(attribute.value.as_ref())
                        .ok()
                        .and_then(|value| unescape(value).ok())
                    else {
                        return Ok(true);
                    };
                    if value.as_bytes() != SVG_NAMESPACE {
                        continue;
                    }
                    if attribute.key.as_ref() == b"xmlns" {
                        default_svg_namespace = true;
                    }
                    if prefixed_namespace
                        .as_ref()
                        .is_some_and(|key| attribute.key.as_ref() == key.as_slice())
                    {
                        prefixed_svg_namespace = true;
                    }
                }
                return Ok(if prefix.is_some() {
                    prefixed_svg_namespace || default_svg_namespace
                } else {
                    default_svg_namespace
                });
            }
            Ok(Event::Eof) | Err(_) => return Ok(false),
            _ => {}
        }
        buffer.clear();
    }
}

fn parse_svg(bytes: &[u8], mode: ParseMode) -> Result<ParsedSvg, ParseIssue> {
    if !classify_svg_root(bytes)? {
        return Err(ParseIssue::NoMatch);
    }
    let (document, source_encoding) = decode_xml(bytes, false)?;
    let mut reader = NsReader::from_reader(document.as_bytes());
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
    let mut invalid_document_text_before_root = false;
    let mut pending_prolog_issue = None;
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| ParseIssue::Malformed)?
        {
            Event::Start(start) => {
                validate_qname(start.name().as_ref())?;
                let (namespace, _) = reader.resolver().resolve_element(start.name());
                prolog_event_seen = true;
                inspect_element(
                    &start,
                    &namespace,
                    reader.resolver(),
                    depth,
                    &mut attributes,
                    &mut parsed,
                    root_seen,
                )?;
                if depth == 0 && invalid_document_text_before_root {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0
                    && let Some(issue) = pending_prolog_issue
                {
                    return Err(issue);
                }
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
            Event::Empty(empty) => {
                validate_qname(empty.name().as_ref())?;
                let (namespace, _) = reader.resolver().resolve_element(empty.name());
                prolog_event_seen = true;
                let prospective_depth = depth.checked_add(1).ok_or(ParseIssue::StructureLimit)?;
                if prospective_depth > MAX_DEPTH {
                    return Err(ParseIssue::StructureLimit);
                }
                inspect_element(
                    &empty,
                    &namespace,
                    reader.resolver(),
                    depth,
                    &mut attributes,
                    &mut parsed,
                    root_seen,
                )?;
                if depth == 0 && invalid_document_text_before_root {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0
                    && let Some(issue) = pending_prolog_issue
                {
                    return Err(issue);
                }
                if mode.collects_text()
                    && is_svg_element(&namespace, empty.local_name().as_ref(), b"text")
                    && !parsed.text.ends_with('\n')
                {
                    append_text(&mut parsed.text, "\n")?;
                }
                if depth == 0 {
                    root_seen = true;
                    root_closed = true;
                }
            }
            Event::End(end) => {
                validate_qname(end.name().as_ref())?;
                let (namespace, _) = reader.resolver().resolve_element(end.name());
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
            Event::Text(text) => {
                if text.as_ref().windows(3).any(|window| window == b"]]>") {
                    return Err(ParseIssue::Malformed);
                }
                let decoded = text.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if !has_only_xml10_characters(&decoded) {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0 && !is_xml_whitespace(&decoded) {
                    if root_seen {
                        return Err(ParseIssue::Malformed);
                    }
                    invalid_document_text_before_root = true;
                }
                if depth == 0 {
                    prolog_event_seen = true;
                }
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &decoded)?;
                }
            }
            Event::CData(cdata) => {
                if depth == 0 {
                    if root_seen {
                        return Err(ParseIssue::Malformed);
                    }
                    prolog_event_seen = true;
                    pending_prolog_issue = Some(ParseIssue::Malformed);
                    buffer.clear();
                    continue;
                }
                let decoded = cdata.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if !has_only_xml10_characters(&decoded) {
                    return Err(ParseIssue::Malformed);
                }
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &decoded)?;
                }
            }
            Event::GeneralRef(reference) => {
                if depth == 0 {
                    return Err(ParseIssue::Malformed);
                }
                let decoded = reference.decode().map_err(|_| ParseIssue::Malformed)?;
                let value = decode_reference(decoded.as_ref())?;
                if mode.collects_text() && text_depth > 0 {
                    append_text(&mut parsed.text, &value)?;
                }
            }
            Event::Decl(declaration) if !root_seen && !declaration_seen && !prolog_event_seen => {
                declaration_seen = true;
                if validate_declaration(&declaration, source_encoding).is_err() {
                    pending_prolog_issue = Some(ParseIssue::Malformed);
                }
            }
            Event::Comment(comment) => {
                let decoded = comment.xml10_content().map_err(|_| ParseIssue::Malformed)?;
                if !has_only_xml10_characters(&decoded) {
                    return Err(ParseIssue::Malformed);
                }
                if comment.as_ref().windows(2).any(|window| window == b"--")
                    || comment.as_ref().ends_with(b"-")
                {
                    return Err(ParseIssue::Malformed);
                }
                if depth == 0 {
                    prolog_event_seen = true;
                }
            }
            Event::Decl(_) => return Err(ParseIssue::Malformed),
            Event::DocType(_) if depth == 0 && !root_seen => {
                prolog_event_seen = true;
                pending_prolog_issue = Some(ParseIssue::Malformed);
            }
            Event::PI(_) if depth == 0 && !root_seen => {
                prolog_event_seen = true;
                pending_prolog_issue = Some(ParseIssue::ActiveContent);
            }
            Event::DocType(_) => return Err(ParseIssue::Malformed),
            Event::PI(_) => return Err(ParseIssue::ActiveContent),
            Event::Eof if root_seen && root_closed && depth == 0 => break,
            Event::Eof if !root_seen => return Err(ParseIssue::NoMatch),
            Event::Eof => return Err(ParseIssue::Malformed),
        }
        buffer.clear();
    }
    Ok(parsed)
}

fn inspect_element(
    element: &BytesStart<'_>,
    namespace: &ResolveResult<'_>,
    resolver: &NamespaceResolver,
    depth: usize,
    attribute_count: &mut usize,
    parsed: &mut ParsedSvg,
    root_seen: bool,
) -> Result<(), ParseIssue> {
    if depth == 0 && root_seen {
        return Err(ParseIssue::Malformed);
    }
    let binding = element.local_name();
    let name = binding.as_ref();
    if depth == 0 {
        if matches!(namespace, ResolveResult::Unbound)
            && !element.name().as_ref().contains(&b':')
            && name != b"svg"
        {
            return Err(ParseIssue::NoMatch);
        }
        if !is_svg_element(namespace, name, b"svg") {
            let declares_default_svg_namespace = element.attributes().any(|attribute| {
                attribute.is_ok_and(|attribute| {
                    attribute.key.as_ref() == b"xmlns" && attribute.value.as_ref() == SVG_NAMESPACE
                })
            });
            return Err(if name == b"svg" && declares_default_svg_namespace {
                ParseIssue::Malformed
            } else {
                ParseIssue::NoMatch
            });
        }
    } else if is_svg_element(namespace, name, b"svg") {
        return Err(ParseIssue::NestedSvg);
    }
    if matches!(
        namespace,
        ResolveResult::Unbound | ResolveResult::Unknown(_)
    ) {
        return Err(ParseIssue::Malformed);
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
    let mut expanded_attributes = HashSet::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ParseIssue::Malformed)?;
        *attribute_count = attribute_count
            .checked_add(1)
            .ok_or(ParseIssue::StructureLimit)?;
        if *attribute_count > MAX_ATTRIBUTES || attribute.value.len() > MAX_ATTRIBUTE_BYTES {
            return Err(ParseIssue::StructureLimit);
        }
        validate_qname(attribute.key.as_ref())?;
        let qualified_key = attribute.key.as_ref();
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
        if qualified_key == b"xmlns" || qualified_key.starts_with(b"xmlns:") {
            validate_namespace_declaration(qualified_key, value.as_bytes())?;
            continue;
        }
        let attribute_namespace = if qualified_key.contains(&b':') {
            let (namespace, _) = resolver.resolve_attribute(attribute.key);
            if matches!(namespace, ResolveResult::Unbound) {
                return Err(ParseIssue::Malformed);
            }
            Some(namespace)
        } else {
            None
        };
        let expanded_namespace = match &attribute_namespace {
            Some(ResolveResult::Bound(namespace)) => {
                let namespace =
                    std::str::from_utf8(namespace.as_ref()).map_err(|_| ParseIssue::Malformed)?;
                unescape(namespace)
                    .map_err(|_| ParseIssue::Malformed)?
                    .into_owned()
                    .into_bytes()
            }
            Some(ResolveResult::Unbound | ResolveResult::Unknown(_)) => {
                return Err(ParseIssue::Malformed);
            }
            None => Vec::new(),
        };
        if !expanded_attributes.insert((expanded_namespace, key.to_vec())) {
            return Err(ParseIssue::Malformed);
        }
        let is_svg_attribute = attribute_namespace.is_none();
        let is_xlink_href = attribute_namespace
            .as_ref()
            .is_some_and(|namespace| namespace_matches(namespace, XLINK_NAMESPACE))
            && key == b"href";
        let is_xml_events_attribute = attribute_namespace
            .as_ref()
            .is_some_and(|namespace| namespace_matches(namespace, XML_EVENTS_NAMESPACE));
        if is_xml_events_attribute {
            return Err(ParseIssue::ActiveContent);
        }
        if is_svg_attribute
            && (event_handler_attribute(key)
                || (depth == 0 && root_window_event_handler_attribute(key))
                || key == b"style")
        {
            return Err(ParseIssue::ActiveContent);
        }
        if (is_svg_attribute && key == b"href")
            || is_xlink_href
            || (!is_svg_namespace(namespace) && is_svg_attribute && foreign_resource_attribute(key))
            || (is_svg_attribute
                && resource_capable_attribute(key)
                && (contains_ascii_case_insensitive(value.as_bytes(), b"url(")
                    || value.contains('\\')))
        {
            return Err(ParseIssue::ExternalReference);
        }
        if depth == 0 && is_svg_attribute {
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
    namespace_matches(namespace, SVG_NAMESPACE)
}

fn namespace_matches(namespace: &ResolveResult<'_>, expected: &[u8]) -> bool {
    let ResolveResult::Bound(namespace) = namespace else {
        return false;
    };
    std::str::from_utf8(namespace.as_ref())
        .ok()
        .and_then(|namespace| unescape(namespace).ok())
        .is_some_and(|namespace| namespace.as_bytes() == expected)
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
            | b"animateColor"
            | b"animateMotion"
            | b"animateTransform"
            | b"set"
            | b"discard"
            | b"handler"
            | b"listener"
    )
}

fn resource_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"image" | b"img" | b"audio" | b"video" | b"source" | b"track"
    )
}

fn foreign_resource_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"src" | b"srcset" | b"poster" | b"data" | b"action" | b"formaction" | b"background"
    )
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
            | b"offset-path"
            | b"shape-inside"
            | b"shape-outside"
            | b"shape-subtract"
            | b"color-profile"
    )
}

fn event_handler_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"onabort"
            | b"onactivate"
            | b"onanimationcancel"
            | b"onanimationend"
            | b"onanimationiteration"
            | b"onanimationstart"
            | b"onauxclick"
            | b"onbeforeinput"
            | b"onbeforematch"
            | b"onbeforetoggle"
            | b"onbegin"
            | b"onblur"
            | b"oncancel"
            | b"oncanplay"
            | b"oncanplaythrough"
            | b"onchange"
            | b"onclick"
            | b"onclose"
            | b"oncompositionend"
            | b"oncompositionstart"
            | b"oncompositionupdate"
            | b"oncontextlost"
            | b"oncontextmenu"
            | b"oncontextrestored"
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
            | b"onformdata"
            | b"onfullscreenchange"
            | b"onfullscreenerror"
            | b"ongotpointercapture"
            | b"oninput"
            | b"oninvalid"
            | b"onkeydown"
            | b"onkeypress"
            | b"onkeyup"
            | b"onload"
            | b"onloadeddata"
            | b"onloadedmetadata"
            | b"onloadstart"
            | b"onlostpointercapture"
            | b"onmousedown"
            | b"onmouseenter"
            | b"onmouseleave"
            | b"onmousemove"
            | b"onmouseout"
            | b"onmouseover"
            | b"onmouseup"
            | b"onmousewheel"
            | b"onpause"
            | b"onpaste"
            | b"onplay"
            | b"onplaying"
            | b"onpointercancel"
            | b"onpointerdown"
            | b"onpointerenter"
            | b"onpointerleave"
            | b"onpointermove"
            | b"onpointerout"
            | b"onpointerover"
            | b"onpointerrawupdate"
            | b"onpointerup"
            | b"onprogress"
            | b"onratechange"
            | b"onrepeat"
            | b"onreset"
            | b"onresize"
            | b"onscroll"
            | b"onscrollend"
            | b"onsecuritypolicyviolation"
            | b"onseeked"
            | b"onseeking"
            | b"onselect"
            | b"onselectionchange"
            | b"onselectstart"
            | b"onshow"
            | b"onslotchange"
            | b"onstalled"
            | b"onsubmit"
            | b"onsuspend"
            | b"ontimeupdate"
            | b"ontoggle"
            | b"ontouchcancel"
            | b"ontouchend"
            | b"ontouchmove"
            | b"ontouchstart"
            | b"ontransitioncancel"
            | b"ontransitionend"
            | b"ontransitionrun"
            | b"ontransitionstart"
            | b"onunload"
            | b"onvolumechange"
            | b"onwaiting"
            | b"onwheel"
            | b"onzoom"
    )
}

fn root_window_event_handler_attribute(name: &[u8]) -> bool {
    matches!(
        name,
        b"onafterprint"
            | b"onbeforeprint"
            | b"onbeforeunload"
            | b"onhashchange"
            | b"onlanguagechange"
            | b"onmessage"
            | b"onmessageerror"
            | b"onoffline"
            | b"ononline"
            | b"onpagehide"
            | b"onpagereveal"
            | b"onpageshow"
            | b"onpageswap"
            | b"onpopstate"
            | b"onrejectionhandled"
            | b"onstorage"
            | b"onunhandledrejection"
            | b"onunload"
    )
}

fn validate_qname(name: &[u8]) -> Result<(), ParseIssue> {
    let name = std::str::from_utf8(name).map_err(|_| ParseIssue::Malformed)?;
    let mut parts = name.split(':');
    let first = parts.next().ok_or(ParseIssue::Malformed)?;
    let second = parts.next();
    if parts.next().is_some()
        || !valid_ncname(first)
        || second.is_some_and(|part| !valid_ncname(part))
    {
        return Err(ParseIssue::Malformed);
    }
    Ok(())
}

fn valid_ncname(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(is_name_start_character) && characters.all(is_name_character)
}

fn is_name_start_character(character: char) -> bool {
    character == '_'
        || character.is_ascii_alphabetic()
        || ('\u{c0}'..='\u{d6}').contains(&character)
        || ('\u{d8}'..='\u{f6}').contains(&character)
        || ('\u{f8}'..='\u{2ff}').contains(&character)
        || ('\u{370}'..='\u{37d}').contains(&character)
        || ('\u{37f}'..='\u{1fff}').contains(&character)
        || ('\u{200c}'..='\u{200d}').contains(&character)
        || ('\u{2070}'..='\u{218f}').contains(&character)
        || ('\u{2c00}'..='\u{2fef}').contains(&character)
        || ('\u{3001}'..='\u{d7ff}').contains(&character)
        || ('\u{f900}'..='\u{fdcf}').contains(&character)
        || ('\u{fdf0}'..='\u{fffd}').contains(&character)
        || ('\u{10000}'..='\u{effff}').contains(&character)
}

fn is_name_character(character: char) -> bool {
    is_name_start_character(character)
        || character.is_ascii_digit()
        || matches!(character, '-' | '.' | '\u{b7}')
        || ('\u{300}'..='\u{36f}').contains(&character)
        || ('\u{203f}'..='\u{2040}').contains(&character)
}

fn parse_dimension(value: &str) -> Result<Option<f64>, ParseIssue> {
    let value = value.trim_matches(|character| matches!(character, ' ' | '\t' | '\n' | '\r'));
    if [
        "auto",
        "inherit",
        "initial",
        "unset",
        "revert",
        "revert-layer",
    ]
    .iter()
    .any(|keyword| value.eq_ignore_ascii_case(keyword))
    {
        return Ok(None);
    }
    if let Ok(length) = svgtypes::Length::from_str(value) {
        if !length.number.is_finite() || length.number < 0.0 {
            return Err(ParseIssue::Malformed);
        }
        return Ok(matches!(
            length.unit,
            svgtypes::LengthUnit::None | svgtypes::LengthUnit::Px
        )
        .then_some(length.number));
    }
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    parser
        .parse_entirely(|parser| {
            let token = parser.next()?.clone();
            match token {
                Token::Dimension { value, .. }
                | Token::Percentage {
                    unit_value: value, ..
                } if value.is_finite() && value >= 0.0 => Ok(None),
                Token::Function(name)
                    if ["calc", "min", "max", "clamp"]
                        .iter()
                        .any(|expected| name.eq_ignore_ascii_case(expected)) =>
                {
                    parser.parse_nested_block(|nested| {
                        let mut saw_value = false;
                        while !nested.is_exhausted() {
                            nested
                                .next_including_whitespace_and_comments()
                                .map_err(cssparser::ParseError::from)?;
                            saw_value = true;
                        }
                        if saw_value {
                            Ok(())
                        } else {
                            Err(nested.new_custom_error(()))
                        }
                    })?;
                    Ok(None)
                }
                token => Err(parser.new_unexpected_token_error::<()>(token)),
            }
        })
        .map_err(|_| ParseIssue::Malformed)
}

fn is_xml_whitespace(value: &str) -> bool {
    value
        .chars()
        .all(|character| matches!(character, ' ' | '\t' | '\n' | '\r'))
}

fn parse_view_box(value: &str) -> Result<[f64; 4], ParseIssue> {
    let values = svgtypes::NumberListParser::from(value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ParseIssue::Malformed)?;
    let [x, y, width, height]: [f64; 4] = values.try_into().map_err(|_| ParseIssue::Malformed)?;
    if width < 0.0 || height < 0.0 {
        return Err(ParseIssue::Malformed);
    }
    Ok([x, y, width, height])
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
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    let metadata_json = metadata_json(parsed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(MEDIA_TYPE),
        evidence,
        metadata_json,
    })
}

fn metadata_output(parsed: &ParsedSvg) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    Ok(ProcessorReadOutput::Structured {
        body_json: metadata_json(parsed)?,
        truncated: false,
        cursor: None,
    })
}

fn metadata_json(parsed: &ParsedSvg) -> Result<String, FileMediaProviderFailure> {
    serde_json::to_string(&serde_json::json!({
        "elements": parsed.elements,
        "height": parsed.height,
        "view_box": parsed.view_box,
        "width": parsed.width,
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)
}

async fn read_all(source: &dyn VerifiedBlobSource) -> Result<Vec<u8>, FileMediaProviderFailure> {
    read_range(
        source,
        SourceRange {
            offset: 0,
            length: source.byte_length().get(),
        },
    )
    .await
}

#[derive(Clone, Copy, Debug)]
struct SourceRange {
    offset: u64,
    length: u64,
}

async fn read_range(
    source: &dyn VerifiedBlobSource,
    range: SourceRange,
) -> Result<Vec<u8>, FileMediaProviderFailure> {
    let length = NonZeroU64::new(range.length).ok_or(FileMediaProviderFailure::Failed)?;
    source
        .read_range(range.offset, length)
        .await
        .map_err(|_| FileMediaProviderFailure::Failed)
}

fn validate_namespace_declaration(name: &[u8], value: &[u8]) -> Result<(), ParseIssue> {
    let prefix = name.strip_prefix(b"xmlns:");
    if value == XMLNS_NAMESPACE
        || prefix == Some(b"xmlns")
        || (prefix == Some(b"xml") && value != XML_NAMESPACE)
        || (prefix != Some(b"xml") && value == XML_NAMESPACE)
        || prefix.is_some_and(|_| value.is_empty())
        || (!value.is_empty() && !valid_iri_reference(value))
    {
        return Err(ParseIssue::Malformed);
    }
    Ok(())
}

fn valid_iri_reference(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| IriReferenceStr::new(value).ok())
        .is_some()
}

fn require_reader(reader: &ReaderIdentity) -> Result<(), FileMediaProviderFailure> {
    if reader.provider().as_str() == PROVIDER_NAME
        && reader.reader().as_str() == READER_NAME
        && reader.revision().as_str() == READER_REVISION
    {
        Ok(())
    } else {
        Err(FileMediaProviderFailure::Failed)
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), FileMediaProviderFailure> {
    if cancellation.is_cancelled() {
        Err(FileMediaProviderFailure::Failed)
    } else {
        Ok(())
    }
}

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn declaration_matches_encoding(
    declaration: &BytesDecl<'_>,
    source_encoding: XmlEncoding,
) -> Result<bool, ()> {
    match declaration.encoding() {
        None => Ok(true),
        Some(Ok(encoding)) => Ok(encoding_matches_source(encoding.as_ref(), source_encoding)),
        Some(Err(_)) => Err(()),
    }
}

fn validate_declaration(
    declaration: &BytesDecl<'_>,
    source_encoding: XmlEncoding,
) -> Result<(), ParseIssue> {
    let mut input = declaration.as_ref();
    input = input.strip_prefix(b"xml").ok_or(ParseIssue::Malformed)?;
    if !input
        .first()
        .is_some_and(|byte| is_xml_whitespace_byte(*byte))
    {
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
            .position(|byte| is_xml_whitespace_byte(*byte) || *byte == b'=')
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
        if !input.is_empty()
            && !input
                .first()
                .is_some_and(|byte| is_xml_whitespace_byte(*byte))
        {
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
        if !encoding_matches_source(encoding, source_encoding) {
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

fn encoding_matches_source(declared: &[u8], source: XmlEncoding) -> bool {
    match source {
        XmlEncoding::Utf8 => {
            declared.eq_ignore_ascii_case(b"utf-8") || declared.eq_ignore_ascii_case(b"utf8")
        }
        XmlEncoding::Utf16Le => declared.eq_ignore_ascii_case(b"utf-16le"),
        XmlEncoding::Utf16Be => declared.eq_ignore_ascii_case(b"utf-16be"),
        XmlEncoding::Utf16LeBom => {
            declared.eq_ignore_ascii_case(b"utf-16") || declared.eq_ignore_ascii_case(b"utf-16le")
        }
        XmlEncoding::Utf16BeBom => {
            declared.eq_ignore_ascii_case(b"utf-16") || declared.eq_ignore_ascii_case(b"utf-16be")
        }
    }
}

fn decode_xml(
    bytes: &[u8],
    allow_truncated_tail: bool,
) -> Result<(Cow<'_, str>, XmlEncoding), ParseIssue> {
    let (encoding, payload) = if let Some(payload) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (XmlEncoding::Utf16LeBom, payload)
    } else if let Some(payload) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (XmlEncoding::Utf16BeBom, payload)
    } else if bytes.starts_with(&[0x3c, 0x00, 0x3f, 0x00]) {
        (XmlEncoding::Utf16Le, bytes)
    } else if bytes.starts_with(&[0x00, 0x3c, 0x00, 0x3f]) {
        (XmlEncoding::Utf16Be, bytes)
    } else {
        let payload = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let document = match std::str::from_utf8(payload) {
            Ok(document) => document,
            Err(error) if allow_truncated_tail => {
                std::str::from_utf8(&payload[..error.valid_up_to()])
                    .map_err(|_| ParseIssue::Malformed)?
            }
            Err(_) => return Err(ParseIssue::Malformed),
        };
        return Ok((Cow::Borrowed(document), XmlEncoding::Utf8));
    };
    let payload = if payload.len() % 2 != 0 && allow_truncated_tail {
        &payload[..payload.len() - 1]
    } else if payload.len() % 2 != 0 {
        return Err(ParseIssue::Malformed);
    } else {
        payload
    };
    let little_endian = match encoding {
        XmlEncoding::Utf16Le | XmlEncoding::Utf16LeBom => true,
        XmlEncoding::Utf16Be | XmlEncoding::Utf16BeBom => false,
        XmlEncoding::Utf8 => return Err(ParseIssue::Malformed),
    };
    let units = payload.chunks_exact(2).map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    let mut document = String::new();
    let decoded = char::decode_utf16(units);
    for character in decoded {
        match character {
            Ok(character) => document.push(character),
            Err(_) if allow_truncated_tail => break,
            Err(_) => return Err(ParseIssue::Malformed),
        }
    }
    Ok((Cow::Owned(document), encoding))
}

fn trim_ascii_start(mut input: &[u8]) -> &[u8] {
    while input
        .first()
        .is_some_and(|byte| is_xml_whitespace_byte(*byte))
    {
        input = &input[1..];
    }
    input
}

const fn is_xml_whitespace_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
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
        if hexadecimal.is_empty() || !hexadecimal.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ParseIssue::Malformed);
        }
        u32::from_str_radix(hexadecimal, 16).map_err(|_| ParseIssue::Malformed)?
    } else if let Some(decimal) = reference.strip_prefix('#') {
        if decimal.is_empty() || !decimal.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseIssue::Malformed);
        }
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
