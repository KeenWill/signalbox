//! Stable agent tools for registry-backed file inspection and typed reads.
//!
//! The service port owns rendered-frontier authorization and verified-source
//! construction. These tools own only exact argument shapes, checked neutral
//! requests, and bounded result projection.

use std::{collections::BTreeMap, future::Future, pin::Pin, str::FromStr};

use serde::Deserialize;
use serde_json::{Value, json};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
    ToolResultText,
};
use signalbox_file_media_runtime::{
    AttachmentKind, CanonicalMediaType, FileDigest, FileInspection, FileMediaFailure,
    FileReadInput, FileReadResult, MAX_PROCESSOR_FRAME_BYTES, ReadContinuationCursor,
    ReadOutputKind, ReadViewName, VisiblePartSelector,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

pub use signalbox_file_media_runtime::{FILE_INSPECT_NAME, FILE_READ_NAME};

const INVALID_INSPECT_ARGUMENTS: &str =
    "expected exactly a canonical digest and optional visible-part selector";
const INVALID_READ_ARGUMENTS: &str = "expected a canonical digest, view, optional selector, and exactly one of object options or continuation";
const RESULT_TOO_LARGE_DETAIL: &str = r#"{"status":"result_too_large"}"#;
// The remaining processor-frame space carries validation evidence and framing.
const MAX_INITIAL_OPTIONS_BYTES: usize = MAX_PROCESSOR_FRAME_BYTES / 4;
const MAX_FILE_READ_ARGUMENT_DEPTH: usize = 256;

/// Checked service request for `file_inspect`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileInspectServiceRequest {
    digest: FileDigest,
    visible_part: Option<VisiblePartSelector>,
}

impl FileInspectServiceRequest {
    /// Constructs one checked service request from decoded tool arguments.
    pub const fn from_parts(digest: FileDigest, visible_part: Option<VisiblePartSelector>) -> Self {
        Self {
            digest,
            visible_part,
        }
    }

    /// Returns the requested immutable digest.
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Borrows the optional repeated-use selector.
    pub const fn visible_part(&self) -> Option<&VisiblePartSelector> {
        self.visible_part.as_ref()
    }
}

/// Checked service request for `file_read`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileReadServiceRequest {
    target: FileInspectServiceRequest,
    view: ReadViewName,
    input: FileReadServiceInput,
}

/// Closed checked input mode for the `file_read` service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileReadServiceInput {
    /// Initial request carrying object options.
    Initial {
        /// Provider-owned view options.
        options: BTreeMap<String, Value>,
    },
    /// Continuation request carrying a prior-page cursor.
    Continuation {
        /// Checked opaque cursor.
        cursor: ReadContinuationCursor,
    },
}

impl FileReadServiceRequest {
    /// Composes checked identities and one input mode; the registry validates view options.
    pub const fn from_parts(
        target: FileInspectServiceRequest,
        view: ReadViewName,
        input: FileReadServiceInput,
    ) -> Self {
        Self {
            target,
            view,
            input,
        }
    }

    /// Borrows the visibility target.
    pub const fn target(&self) -> &FileInspectServiceRequest {
        &self.target
    }

    /// Borrows the provider-owned view name.
    pub const fn view(&self) -> &ReadViewName {
        &self.view
    }

    /// Borrows structured model-supplied options on an initial request.
    pub const fn options(&self) -> Option<&BTreeMap<String, Value>> {
        match &self.input {
            FileReadServiceInput::Initial { options } => Some(options),
            FileReadServiceInput::Continuation { .. } => None,
        }
    }

    /// Borrows the checked prior-page cursor on a continuation request.
    pub const fn continuation(&self) -> Option<&ReadContinuationCursor> {
        match &self.input {
            FileReadServiceInput::Initial { .. } => None,
            FileReadServiceInput::Continuation { cursor } => Some(cursor),
        }
    }

    /// Converts the checked service input into the neutral runtime input.
    pub fn into_runtime_input(self) -> FileReadInput {
        match self.input {
            FileReadServiceInput::Initial { options } => FileReadInput::Initial {
                options: Value::Object(options.into_iter().collect()),
            },
            FileReadServiceInput::Continuation { cursor } => FileReadInput::Continuation { cursor },
        }
    }
}

/// Boxed future returned by the agent-facing file/media service.
pub type FileMediaAgentServiceFuture<'a, Output> =
    Pin<Box<dyn Future<Output = Result<Output, FileMediaServiceFailure>> + Send + 'a>>;

/// Separates ordinary file failures from failures of the tool's authority infrastructure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileMediaServiceFailure {
    /// Content-silent failure that may be committed as an ordinary tool result.
    File(FileMediaFailure),
    /// Operator failure that must retain its infrastructure or fail-closed class.
    Operator(FileMediaExecutorError),
}

impl From<FileMediaFailure> for FileMediaServiceFailure {
    fn from(failure: FileMediaFailure) -> Self {
        Self::File(failure)
    }
}

/// Visibility-authorized registry service consumed by both stable tools.
pub trait FileMediaAgentService: Send {
    /// Resolves one visible use, verifies its source, and inspects it.
    fn inspect(
        &mut self,
        request: FileInspectServiceRequest,
    ) -> FileMediaAgentServiceFuture<'_, FileInspection>;

    /// Repeats inspection and returns one bounded typed view.
    fn read(
        &mut self,
        request: FileReadServiceRequest,
    ) -> FileMediaAgentServiceFuture<'_, FileReadResult>;
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FileInspectArguments {
    /// Canonical sha256: digest of a visible attachment.
    digest: String,
    /// Visible-part selector carried by the rendered attachment stub.
    visible_part: Option<String>,
}

struct FileInspectContract;

impl ToolContract for FileInspectContract {
    type Arguments = FileInspectArguments;
    const NAME: &'static str = FILE_INSPECT_NAME;
    const DESCRIPTION: &'static str = "Inspects one visible immutable file and returns validated type facts and available bounded views.";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FileReadArguments {
    /// Canonical sha256: digest of a visible attachment.
    digest: String,
    /// Exact provider-owned view returned by file_inspect.
    view: String,
    /// Object options validated by the selected view on an initial request.
    options: Option<BTreeMap<String, Value>>,
    /// Opaque cursor returned by the preceding file_read result.
    continuation: Option<String>,
    /// Visible-part selector carried by the rendered attachment stub.
    visible_part: Option<String>,
}

struct FileReadContract;

impl ToolContract for FileReadContract {
    type Arguments = FileReadArguments;
    const NAME: &'static str = FILE_READ_NAME;
    const DESCRIPTION: &'static str = "Repeats safe inspection and reads one declared bounded file view without trusting model-supplied type or reader identity.";
}

/// Compiled stable declarations and matching generic executor.
#[derive(Clone, Debug)]
pub struct FileMediaTools<Service> {
    catalog: CompiledToolCatalog,
    executor: FileMediaExecutor<Service>,
}

impl<Service> FileMediaTools<Service> {
    /// Compiles both stable tools around one authorization and registry service.
    pub fn try_new(service: Service) -> Result<Self, FileMediaToolConstructionError> {
        let inspect_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_INSPECT_ARGUMENTS))
                .map_err(|_| FileMediaToolConstructionError::ErrorDetail)?;
        let read_detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_READ_ARGUMENTS))
            .map_err(|_| FileMediaToolConstructionError::ErrorDetail)?;
        let inspect = compile_contract_definition::<FileInspectContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(map_contract_error)?;
        let read = compile_contract_definition::<FileReadContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(map_contract_error)?;
        let catalog = CompiledToolCatalog::try_new([
            CompiledTool::new(
                inspect,
                InspectArgumentValidator {
                    detail: inspect_detail,
                },
            ),
            CompiledTool::new(
                read,
                ReadArgumentValidator {
                    detail: read_detail,
                },
            ),
        ])
        .map_err(|_| FileMediaToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: FileMediaExecutor { service },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, FileMediaExecutor<Service>) {
        (self.catalog, self.executor)
    }
}

fn map_contract_error(error: ToolContractCompileError) -> FileMediaToolConstructionError {
    match error {
        ToolContractCompileError::Name => FileMediaToolConstructionError::Name,
        ToolContractCompileError::Schema => FileMediaToolConstructionError::Schema,
    }
}

#[derive(signalbox_derive::OperatorError)]
/// Static construction failure for the two-entry file/media family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMediaToolConstructionError {
    #[error("file media static tool name is invalid")]
    /// A stable name was rejected.
    Name,
    #[error("file media static tool schema is invalid")]
    /// A stable schema was rejected.
    Schema,
    #[error("file media static error detail is invalid")]
    /// Static sanitized failure detail was rejected.
    ErrorDetail,
    #[error("file media tool catalog is duplicated")]
    /// The two-entry catalog unexpectedly found a duplicate.
    Duplicate,
}

#[derive(Clone, Debug)]
struct InspectArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for InspectArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_inspect(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Debug)]
struct ReadArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for ReadArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_read(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidFileMediaArguments;

fn decode_inspect(
    arguments: &NormalizedToolArguments,
) -> Result<FileInspectServiceRequest, InvalidFileMediaArguments> {
    let decoded: FileInspectArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidFileMediaArguments)?;
    Ok(FileInspectServiceRequest {
        digest: FileDigest::from_str(&decoded.digest).map_err(|_| InvalidFileMediaArguments)?,
        visible_part: decoded
            .visible_part
            .map(VisiblePartSelector::try_new)
            .transpose()
            .map_err(|_| InvalidFileMediaArguments)?,
    })
}

fn decode_read(
    arguments: &NormalizedToolArguments,
) -> Result<FileReadServiceRequest, InvalidFileMediaArguments> {
    if !json_container_depth_fits(arguments.as_str(), MAX_FILE_READ_ARGUMENT_DEPTH) {
        return Err(InvalidFileMediaArguments);
    }
    let decoded: FileReadArguments = decode_without_recursion_limit(arguments.as_str())?;
    let continuation = decoded
        .continuation
        .map(ReadContinuationCursor::try_new)
        .transpose()
        .map_err(|_| InvalidFileMediaArguments)?;
    let input = match (decoded.options, continuation) {
        (Some(options), None) if initial_options_fit(&options) => {
            FileReadServiceInput::Initial { options }
        }
        (Some(_), None) => return Err(InvalidFileMediaArguments),
        (None, Some(cursor)) => FileReadServiceInput::Continuation { cursor },
        (Some(_), Some(_)) | (None, None) => return Err(InvalidFileMediaArguments),
    };
    Ok(FileReadServiceRequest {
        target: FileInspectServiceRequest {
            digest: FileDigest::from_str(&decoded.digest).map_err(|_| InvalidFileMediaArguments)?,
            visible_part: decoded
                .visible_part
                .map(VisiblePartSelector::try_new)
                .transpose()
                .map_err(|_| InvalidFileMediaArguments)?,
        },
        view: ReadViewName::try_new(decoded.view).map_err(|_| InvalidFileMediaArguments)?,
        input,
    })
}

fn decode_without_recursion_limit<T: for<'de> Deserialize<'de>>(
    encoded: &str,
) -> Result<T, InvalidFileMediaArguments> {
    let mut deserializer = serde_json::Deserializer::from_str(encoded);
    deserializer.disable_recursion_limit();
    let stacked = serde_stacker::Deserializer::new(&mut deserializer);
    let decoded = T::deserialize(stacked).map_err(|_| InvalidFileMediaArguments)?;
    deserializer.end().map_err(|_| InvalidFileMediaArguments)?;
    Ok(decoded)
}

fn initial_options_fit(options: &BTreeMap<String, Value>) -> bool {
    serde_json::to_vec(options).is_ok_and(|encoded| encoded.len() <= MAX_INITIAL_OPTIONS_BYTES)
}

fn json_container_depth_fits(encoded: &str, maximum_depth: usize) -> bool {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in encoded.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = match depth.checked_add(1) {
                    Some(depth) if depth <= maximum_depth => depth,
                    _ => return false,
                };
            }
            b'}' | b']' => {
                depth = match depth.checked_sub(1) {
                    Some(depth) => depth,
                    None => return false,
                };
            }
            _ => {}
        }
    }
    depth == 0 && !in_string && !escaped
}

/// Generic executor for both file/media tools.
#[derive(Clone, Debug)]
pub struct FileMediaExecutor<Service> {
    service: Service,
}

#[derive(signalbox_derive::OperatorError)]
#[error("file media tool infrastructure failed")]
/// Sanitized authority or executor failure preserving its operator classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMediaExecutorError {
    class: OperatorFailureClass,
}

impl FileMediaExecutorError {
    /// Carries an already classified, content-silent operator failure.
    pub const fn from_class(class: OperatorFailureClass) -> Self {
        Self { class }
    }
    /// Preserves the source failure's classification without retaining its content.
    pub fn from_error(error: &impl ClassifyOperatorFailure) -> Self {
        Self {
            class: error.operator_failure_class(),
        }
    }

    fn invalid_arguments() -> Self {
        Self {
            class: OperatorFailureClass::CallerOrHubBug,
        }
    }
}

impl ClassifyOperatorFailure for FileMediaExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<Service> ToolExecutor for FileMediaExecutor<Service>
where
    Service: FileMediaAgentService,
{
    type Error = FileMediaExecutorError;

    fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<CorrelatedToolExecutorEvidence, Self::Error>> + Send {
        let name = invocation.request().name().as_str();
        let arguments = invocation.request().arguments();
        let operation = if name == FILE_INSPECT_NAME {
            decode_inspect(arguments)
                .map(FileMediaOperation::Inspect)
                .map_err(|_| FileMediaExecutorError::invalid_arguments())
        } else if name == FILE_READ_NAME {
            decode_read(arguments)
                .map(FileMediaOperation::Read)
                .map_err(|_| FileMediaExecutorError::invalid_arguments())
        } else {
            Err(FileMediaExecutorError::invalid_arguments())
        };
        async move {
            let evidence = match operation? {
                FileMediaOperation::Inspect(request) => {
                    service_evidence(self.service.inspect(request).await, |value| {
                        Ok(inspection_evidence(value))
                    })?
                }
                FileMediaOperation::Read(request) => {
                    service_evidence(self.service.read(request).await, read_evidence)?
                }
            };
            Ok(invocation.bind(evidence))
        }
    }
}

fn service_evidence<T>(
    result: Result<T, FileMediaServiceFailure>,
    completed: impl FnOnce(T) -> Result<ToolExecutorEvidence, FileMediaExecutorError>,
) -> Result<ToolExecutorEvidence, FileMediaExecutorError> {
    match result {
        Ok(value) => completed(value),
        Err(FileMediaServiceFailure::File(failure)) => failure_evidence(failure),
        Err(FileMediaServiceFailure::Operator(error)) => Err(error),
    }
}

enum FileMediaOperation {
    Inspect(FileInspectServiceRequest),
    Read(FileReadServiceRequest),
}

fn inspection_evidence(inspection: FileInspection) -> ToolExecutorEvidence {
    match inspection {
        FileInspection::Validated(validated) => {
            let views = validated
                .views()
                .iter()
                .map(|view| {
                    json!({
                        "name": view.name().as_str(),
                        "description": view.description(),
                        "arguments_schema": view.arguments_schema().value(),
                        "output": output_kind_name(view.output_kind()),
                    })
                })
                .collect::<Vec<_>>();
            completed_json(json!({
                "status": "validated",
                "digest": validated.source().digest().to_string(),
                "byte_length": validated.source().byte_length().get().to_string(),
                "attachment_kind": attachment_kind_name(validated.source().attachment_kind()),
                "declared_media_type": validated.source().declared_media_type().as_str(),
                "display_filename": validated.source().display_filename().map(|name| name.as_str()),
                "detected_media_type": validated.detected_media_type().as_str(),
                "reader": {
                    "provider": validated.reader().provider().as_str(),
                    "reader": validated.reader().reader().as_str(),
                    "revision": validated.reader().revision().as_str(),
                },
                "metadata": validated.metadata().value(),
                "views": views,
            }))
        }
        FileInspection::Unknown { source } => completed_json(json!({
            "status": "unknown",
            "digest": source.digest().to_string(),
            "byte_length": source.byte_length().get().to_string(),
            "attachment_kind": attachment_kind_name(source.attachment_kind()),
            "declared_media_type": source.declared_media_type().as_str(),
            "display_filename": source.display_filename().map(|name| name.as_str()),
            "views": [],
        })),
        FileInspection::Malformed {
            media_type,
            reason_code,
            ..
        } => known_failure(json!({
            "status": "malformed",
            "media_type": media_type.as_str(),
            "reason_code": reason_code.as_str(),
        })),
        FileInspection::Ambiguous { media_types, .. } => ambiguous_failure(&media_types),
        FileInspection::DeclaredMismatch {
            declared, detected, ..
        } => known_failure(json!({
            "status": "declared_mismatch",
            "declared": declared.as_str(),
            "detected": detected.as_str(),
        })),
        FileInspection::EncryptedOrLocked { media_type, .. } => known_failure(json!({
            "status": "encrypted_or_locked",
            "media_type": media_type.as_str(),
        })),
    }
}

fn ambiguous_failure(media_types: &[CanonicalMediaType]) -> ToolExecutorEvidence {
    let mut projected = Vec::new();
    for media_type in media_types {
        projected.push(media_type.as_str());
        let candidate = json!({
            "status": "ambiguous",
            "media_types": &projected,
            "truncated": true,
        });
        if ToolExecutionErrorDetail::try_new(candidate.to_string()).is_err() {
            projected.pop();
            return known_failure(json!({
                "status": "ambiguous",
                "media_types": projected,
                "truncated": true,
            }));
        }
    }
    known_failure(json!({
        "status": "ambiguous",
        "media_types": projected,
    }))
}

fn read_evidence(result: FileReadResult) -> Result<ToolExecutorEvidence, FileMediaExecutorError> {
    Ok(match result {
        FileReadResult::Reference(reference) => return reference_evidence(reference),
        FileReadResult::Text { body, continuation } => completed_json(json!({
            "status": "text",
            "body": body,
            "truncated": matches!(&continuation, signalbox_file_media_runtime::ReadContinuation::More { .. }),
            "cursor": continuation_cursor(continuation),
        })),
        FileReadResult::Structured { body, continuation } => completed_json(json!({
            "status": "structured",
            "body": body,
            "truncated": matches!(&continuation, signalbox_file_media_runtime::ReadContinuation::More { .. }),
            "cursor": continuation_cursor(continuation),
        })),
    })
}

fn reference_evidence(
    reference: signalbox_file_media_runtime::FileMediaReference,
) -> Result<ToolExecutorEvidence, FileMediaExecutorError> {
    fn domain_identity(
        identity: &signalbox_file_media_runtime::MediaValidationIdentity,
    ) -> Option<signalbox_domain::MediaValidationIdentity> {
        let evidence = match identity.evidence() {
            signalbox_file_media_runtime::ValidationEvidence::StrongSignature => {
                signalbox_domain::MediaValidationEvidence::StrongSignature
            }
            signalbox_file_media_runtime::ValidationEvidence::StructuralValidation => {
                signalbox_domain::MediaValidationEvidence::StructuralValidation
            }
            _ => return None,
        };
        signalbox_domain::MediaValidationIdentity::try_new(
            signalbox_domain::BlobDigest::from_bytes(*identity.digest().as_bytes()),
            identity.media_type().as_str().to_owned(),
            identity.reader().provider().as_str().to_owned(),
            identity.reader().reader().as_str().to_owned(),
            identity.reader().revision().as_str().to_owned(),
            evidence,
        )
    }
    let (Some(identity), Some(source)) = (
        domain_identity(reference.presented()),
        domain_identity(reference.source()),
    ) else {
        return Err(FileMediaExecutorError::from_class(
            OperatorFailureClass::FailClosedCorruption,
        ));
    };
    let text = json!({"status":"read", "output":"image", "digest":identity.digest().to_string(), "media_type":identity.media_type(), "byte_length":reference.byte_length().get().to_string()});
    let Some(reference) =
        signalbox_domain::ToolMediaReference::image(identity, source, reference.byte_length())
    else {
        return failure_evidence(FileMediaFailure::OutputUnitTooLarge);
    };
    match ToolResultText::try_new(text.to_string()) {
        Ok(text) => Ok(ToolExecutorEvidence::CompletedMedia { text, reference }),
        Err(_) => failure_evidence(FileMediaFailure::OutputUnitTooLarge),
    }
}

fn continuation_cursor(
    continuation: signalbox_file_media_runtime::ReadContinuation,
) -> Option<String> {
    match continuation {
        signalbox_file_media_runtime::ReadContinuation::Complete => None,
        signalbox_file_media_runtime::ReadContinuation::More { cursor } => {
            Some(cursor.into_string())
        }
    }
}

fn failure_evidence(
    failure: FileMediaFailure,
) -> Result<ToolExecutorEvidence, FileMediaExecutorError> {
    let value = match failure {
        FileMediaFailure::SourceIntegrity => {
            return Err(FileMediaExecutorError::from_class(
                OperatorFailureClass::FailClosedCorruption,
            ));
        }
        FileMediaFailure::BlobNotVisible => json!({"status": "blob_not_visible"}),
        FileMediaFailure::BlobMissing => json!({"status": "blob_missing"}),
        FileMediaFailure::BlobCorrupt => json!({"status": "blob_corrupt"}),
        FileMediaFailure::BlobUnavailable => json!({"status": "blob_unavailable"}),
        FileMediaFailure::UnknownType => json!({"status": "unknown_type"}),
        FileMediaFailure::AmbiguousType => json!({"status": "ambiguous_type"}),
        FileMediaFailure::DeclaredTypeMismatch { declared, detected } => json!({
            "status": "declared_type_mismatch",
            "declared": declared.as_str(),
            "detected": detected.as_str(),
        }),
        FileMediaFailure::Malformed {
            media_type,
            reason_code,
        } => json!({
            "status": "malformed",
            "media_type": media_type.as_str(),
            "reason_code": reason_code.as_str(),
        }),
        FileMediaFailure::EncryptedOrLocked { media_type } => json!({
            "status": "encrypted_or_locked",
            "media_type": media_type.as_str(),
        }),
        FileMediaFailure::UnsupportedView => json!({"status": "unsupported_view"}),
        FileMediaFailure::InvalidViewArguments => json!({"status": "invalid_view_arguments"}),
        FileMediaFailure::SourceTooLarge { maximum_bytes } => json!({
            "status": "source_too_large",
            "maximum_bytes": maximum_bytes.to_string(),
        }),
        FileMediaFailure::ExpansionLimitExceeded { limit_kind } => json!({
            "status": "expansion_limit_exceeded",
            "limit_kind": limit_kind.as_str(),
        }),
        FileMediaFailure::OutputUnitTooLarge => json!({"status": "output_unit_too_large"}),
        FileMediaFailure::ProcessorUnavailable => json!({"status": "processor_unavailable"}),
        FileMediaFailure::ProcessorFailed => json!({"status": "processor_failed"}),
        FileMediaFailure::ProcessorTimedOut => json!({"status": "processor_timed_out"}),
        FileMediaFailure::Cancelled => json!({"status": "cancelled"}),
    };
    Ok(known_failure(value))
}

fn completed_json(value: Value) -> ToolExecutorEvidence {
    match ToolResultText::try_new(value.to_string()) {
        Ok(text) => ToolExecutorEvidence::CompletedText(text.into_string()),
        Err(_) => known_failure(json!({"status": "result_too_large"})),
    }
}

fn known_failure(value: Value) -> ToolExecutorEvidence {
    let detail = ToolExecutionErrorDetail::try_new(value.to_string())
        .or_else(|_| ToolExecutionErrorDetail::try_new(String::from(RESULT_TOO_LARGE_DETAIL)))
        .ok();
    ToolExecutorEvidence::KnownFailed { detail }
}

const fn attachment_kind_name(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::Image => "image",
        AttachmentKind::Document => "document",
        AttachmentKind::File => "file",
    }
}

const fn output_kind_name(kind: ReadOutputKind) -> &'static str {
    match kind {
        ReadOutputKind::Text => "text",
        ReadOutputKind::Structured => "structured",
        ReadOutputKind::Image => "image",
        ReadOutputKind::Audio => "audio",
        ReadOutputKind::File => "file",
    }
}

#[cfg(test)]
mod tests {
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;

    #[test]
    fn authority_failures_escape_without_committing_ordinary_tool_failure_evidence() {
        for class in [
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            OperatorFailureClass::FailClosedCorruption,
            OperatorFailureClass::IdentityCollision,
            OperatorFailureClass::CallerOrHubBug,
        ] {
            let failure = FileMediaExecutorError::from_class(class);
            let outcome = service_evidence(
                Err(FileMediaServiceFailure::Operator(failure)),
                read_evidence,
            );
            assert_eq!(outcome.unwrap_err().operator_failure_class(), class);
        }
        let unavailable =
            service_evidence(Err(FileMediaFailure::BlobUnavailable.into()), read_evidence).unwrap();
        assert!(matches!(
            unavailable,
            ToolExecutorEvidence::KnownFailed { .. }
        ));
    }

    #[test]
    fn source_integrity_failure_cannot_commit_an_ordinary_failed_tool_result() {
        let failure = FileMediaFailure::from(
            signalbox_file_media_runtime::ProcessorBoundaryFailure::Source(
                signalbox_file_media_runtime::SourceReadError::Integrity,
            ),
        );
        let inspected = service_evidence(Err(failure.clone().into()), |value| {
            Ok(inspection_evidence(value))
        });
        let read = service_evidence(Err(failure.into()), read_evidence);

        assert_eq!(
            inspected.unwrap_err().operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
        assert_eq!(
            read.unwrap_err().operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
    }

    struct UnusedService;

    impl FileMediaAgentService for UnusedService {
        fn inspect(
            &mut self,
            _request: FileInspectServiceRequest,
        ) -> FileMediaAgentServiceFuture<'_, FileInspection> {
            Box::pin(async { Err(FileMediaFailure::BlobNotVisible.into()) })
        }

        fn read(
            &mut self,
            _request: FileReadServiceRequest,
        ) -> FileMediaAgentServiceFuture<'_, FileReadResult> {
            Box::pin(async { Err(FileMediaFailure::BlobNotVisible.into()) })
        }
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn oversized_initial_options(digest: FileDigest) -> String {
        serde_json::json!({
            "digest": digest.to_string(),
            "view": "body_text",
            "options": {"value": "x".repeat(MAX_INITIAL_OPTIONS_BYTES)},
            "visible_part": null,
        })
        .to_string()
    }

    fn deeply_nested_initial_options(digest: FileDigest) -> String {
        let nested = format!("{}0{}", "[".repeat(160), "]".repeat(160));
        format!(
            r#"{{"digest":"{digest}","view":"body_text","options":{{"nested":{nested}}},"visible_part":null}}"#
        )
    }

    fn excessively_nested_initial_options(digest: FileDigest) -> String {
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_FILE_READ_ARGUMENT_DEPTH),
            "]".repeat(MAX_FILE_READ_ARGUMENT_DEPTH)
        );
        format!(
            r#"{{"digest":"{digest}","view":"body_text","options":{{"nested":{nested}}},"visible_part":null}}"#
        )
    }

    fn many_long_media_types() -> Vec<CanonicalMediaType> {
        (0..32)
            .map(|index| {
                format!("application/x-{index:02}-{}", "a".repeat(110))
                    .parse()
                    .expect("fixture media type is canonical")
            })
            .collect()
    }

    #[test]
    fn stable_catalog_exposes_exact_inspect_and_read_names() {
        let (catalog, _executor) = FileMediaTools::try_new(UnusedService)
            .expect("static file media tools compile")
            .into_parts();

        assert_eq!(catalog.definitions()[0].name().as_str(), FILE_INSPECT_NAME);
        assert_eq!(catalog.definitions()[1].name().as_str(), FILE_READ_NAME);
        assert!(
            catalog
                .definitions()
                .iter()
                .all(|definition| definition.effect_class() == ToolEffectClass::ExternalEffect)
        );
    }

    #[test]
    fn inspect_arguments_reject_noncanonical_digest() {
        let (catalog, _executor) = FileMediaTools::try_new(UnusedService)
            .expect("static file media tools compile")
            .into_parts();
        let inspect = &catalog.definitions()[0];

        let outcome = catalog.validate_arguments(
            inspect.name(),
            &arguments(r#"{"digest":"SHA256:00","visible_part":null}"#),
        );

        assert!(matches!(
            outcome,
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    #[test]
    fn read_arguments_require_object_options() {
        let (catalog, _executor) = FileMediaTools::try_new(UnusedService)
            .expect("static file media tools compile")
            .into_parts();
        let read = &catalog.definitions()[1];
        let digest = FileDigest::from_bytes([0x11; 32]).to_string();
        let supplied = format!(
            r#"{{"digest":"{digest}","view":"body_text","options":[],"visible_part":null}}"#
        );

        let outcome = catalog.validate_arguments(read.name(), &arguments(&supplied));

        assert!(matches!(
            outcome,
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    #[test]
    fn read_arguments_accept_a_returned_continuation_without_options() {
        let digest = FileDigest::from_bytes([0x22; 32]).to_string();
        let supplied = format!(
            r#"{{"digest":"{digest}","view":"body_text","continuation":"next-page","visible_part":null}}"#
        );

        let decoded = decode_read(&arguments(&supplied))
            .expect("a checked prior-page cursor forms a continuation request");

        assert!(decoded.options().is_none());
        assert_eq!(
            decoded
                .continuation()
                .expect("the continuation remains present")
                .as_str(),
            "next-page"
        );
    }

    #[test]
    fn read_arguments_reject_initial_options_that_cannot_fit_the_processor_frame() {
        let supplied = oversized_initial_options(FileDigest::from_bytes([0x33; 32]));

        let outcome = decode_read(&arguments(&supplied));

        assert_eq!(outcome, Err(InvalidFileMediaArguments));
    }

    #[test]
    fn read_arguments_preserve_deep_options_for_adapter_validation() {
        let supplied = deeply_nested_initial_options(FileDigest::from_bytes([0x44; 32]));

        let decoded = decode_read(&arguments(&supplied))
            .expect("deep bounded options remain available to the selected adapter");

        assert!(decoded.options().is_some());
    }

    #[test]
    fn read_arguments_reject_options_beyond_the_contract_depth_ceiling() {
        let supplied = excessively_nested_initial_options(FileDigest::from_bytes([0x55; 32]));

        let outcome = decode_read(&arguments(&supplied));

        assert_eq!(outcome, Err(InvalidFileMediaArguments));
    }

    #[test]
    fn oversized_ambiguity_inventory_preserves_ambiguous_status() {
        let media_types = many_long_media_types();

        let evidence = ambiguous_failure(&media_types);

        let ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        } = evidence
        else {
            panic!("ambiguous evidence remains a typed known failure");
        };
        let projected: Value =
            serde_json::from_str(detail.as_str()).expect("bounded ambiguity detail remains JSON");
        assert_eq!(projected["status"], "ambiguous");
        assert_eq!(projected["truncated"], true);
    }

    #[test]
    fn maximum_admitted_text_result_fits_tool_result_text_bound() {
        let result = FileReadResult::Text {
            body: "\u{1f}".repeat(signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES),
            continuation: signalbox_file_media_runtime::ReadContinuation::Complete,
        };

        let evidence = read_evidence(result).unwrap();

        let ToolExecutorEvidence::CompletedText(text) = evidence else {
            panic!("the maximum admitted worst-case text must fit the tool result");
        };
        assert!(ToolResultText::try_new(text).is_ok());
    }

    #[test]
    fn oversized_known_failure_retains_compact_typed_evidence() {
        let evidence = known_failure(json!({
            "status": "ambiguous",
            "media_types": ["x".repeat(4_096)],
        }));

        let ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        } = evidence
        else {
            panic!("an oversized known failure must retain fallback detail");
        };
        assert_eq!(detail.as_str(), RESULT_TOO_LARGE_DETAIL);
    }
}
