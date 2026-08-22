//! Stable agent tools for registry-backed file inspection and typed reads.
//!
//! The service port owns rendered-frontier authorization and verified-source
//! construction. These tools own only exact argument shapes, checked neutral
//! requests, and bounded result projection.

use std::{collections::BTreeMap, error::Error, fmt, future::Future, pin::Pin, str::FromStr};

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
    AttachmentKind, FileDigest, FileInspection, FileMediaFailure, FileReadInput, FileReadResult,
    ReadContinuationCursor, ReadOutputKind, ReadViewName, VisiblePartSelector,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

pub use signalbox_file_media_runtime::{FILE_INSPECT_NAME, FILE_READ_NAME};

const INVALID_INSPECT_ARGUMENTS: &str =
    "expected exactly a canonical digest and optional visible-part selector";
const INVALID_READ_ARGUMENTS: &str = "expected a canonical digest, view, optional selector, and exactly one of object options or continuation";
const RESULT_TOO_LARGE_DETAIL: &str = r#"{"status":"result_too_large"}"#;

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
    Pin<Box<dyn Future<Output = Result<Output, FileMediaFailure>> + Send + 'a>>;

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
    /// Visible-part selector returned by attachment inspection.
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
    /// Visible-part selector returned by attachment inspection.
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
            ToolEffectClass::EffectFree,
        )
        .map_err(map_contract_error)?;
        let read = compile_contract_definition::<FileReadContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
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

/// Static construction failure for the two-entry file/media family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMediaToolConstructionError {
    /// A stable name was rejected.
    Name,
    /// A stable schema was rejected.
    Schema,
    /// Static sanitized failure detail was rejected.
    ErrorDetail,
    /// The two-entry catalog unexpectedly found a duplicate.
    Duplicate,
}

impl fmt::Display for FileMediaToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "file media static tool name is invalid",
            Self::Schema => "file media static tool schema is invalid",
            Self::ErrorDetail => "file media static error detail is invalid",
            Self::Duplicate => "file media tool catalog is duplicated",
        })
    }
}

impl Error for FileMediaToolConstructionError {}

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
    let decoded: FileReadArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidFileMediaArguments)?;
    let continuation = decoded
        .continuation
        .map(ReadContinuationCursor::try_new)
        .transpose()
        .map_err(|_| InvalidFileMediaArguments)?;
    let input = match (decoded.options, continuation) {
        (Some(options), None) => FileReadServiceInput::Initial { options },
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

/// Generic executor for both file/media tools.
#[derive(Clone, Debug)]
pub struct FileMediaExecutor<Service> {
    service: Service,
}

/// A checked catalog/executor assumption drifted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMediaExecutorError;

impl fmt::Display for FileMediaExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("file media argument validation drifted")
    }
}

impl Error for FileMediaExecutorError {}

impl ClassifyOperatorFailure for FileMediaExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
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
                .map_err(|_| FileMediaExecutorError)
        } else if name == FILE_READ_NAME {
            decode_read(arguments)
                .map(FileMediaOperation::Read)
                .map_err(|_| FileMediaExecutorError)
        } else {
            Err(FileMediaExecutorError)
        };
        async move {
            let evidence = match operation? {
                FileMediaOperation::Inspect(request) => match self.service.inspect(request).await {
                    Ok(inspection) => inspection_evidence(inspection),
                    Err(failure) => failure_evidence(failure),
                },
                FileMediaOperation::Read(request) => match self.service.read(request).await {
                    Ok(result) => read_evidence(result),
                    Err(failure) => failure_evidence(failure),
                },
            };
            Ok(invocation.bind(evidence))
        }
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
        FileInspection::Ambiguous { media_types, .. } => known_failure(json!({
            "status": "ambiguous",
            "media_types": media_types
                .iter()
                .map(|media_type| media_type.as_str())
                .collect::<Vec<_>>(),
        })),
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

fn read_evidence(result: FileReadResult) -> ToolExecutorEvidence {
    match result {
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

fn failure_evidence(failure: FileMediaFailure) -> ToolExecutorEvidence {
    let value = match failure {
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
    known_failure(value)
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

    struct UnusedService;

    impl FileMediaAgentService for UnusedService {
        fn inspect(
            &mut self,
            _request: FileInspectServiceRequest,
        ) -> FileMediaAgentServiceFuture<'_, FileInspection> {
            Box::pin(async { Err(FileMediaFailure::BlobNotVisible) })
        }

        fn read(
            &mut self,
            _request: FileReadServiceRequest,
        ) -> FileMediaAgentServiceFuture<'_, FileReadResult> {
            Box::pin(async { Err(FileMediaFailure::BlobNotVisible) })
        }
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    #[test]
    fn stable_catalog_exposes_exact_inspect_and_read_names() {
        let (catalog, _executor) = FileMediaTools::try_new(UnusedService)
            .expect("static file media tools compile")
            .into_parts();

        assert_eq!(catalog.definitions()[0].name().as_str(), FILE_INSPECT_NAME);
        assert_eq!(catalog.definitions()[1].name().as_str(), FILE_READ_NAME);
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
    fn maximum_admitted_text_result_fits_tool_result_text_bound() {
        let result = FileReadResult::Text {
            body: "\u{1f}".repeat(signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES),
            continuation: signalbox_file_media_runtime::ReadContinuation::Complete,
        };

        let evidence = read_evidence(result);

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
