//! Tool-attributed replacement of one session-metadata snapshot.

use std::{error::Error, fmt, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest,
    ReplaceSessionMetadataService, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    Actor, DurableCommandId, NormalizedToolArguments, ReplaceSessionMetadataRejectedResult,
    ReplaceSessionMetadataResult, SessionId, SessionMetadataContent, SessionMetadataSnapshot,
    ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault, ToolRequestId,
    ToolResultText,
};
use signalbox_persistence::session_metadata::{
    SessionMetadataRepository, SessionMetadataRepositoryError,
};
use sqlx::PgPool;

use crate::tool_contract::{ToolContract, ToolContractCompileError, compile_contract_definition};

pub(crate) const SESSION_STATUS_UPDATE_NAME: &str = "session_status_update";
const INVALID_ARGUMENTS_DETAIL: &str =
    "expected one complete admitted title, tags, attributes, and archived snapshot";
const SESSION_NOT_FOUND_DETAIL: &str = "session metadata target does not exist";

/// A static `session_status_update` declaration could not be compiled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatusToolConstructionError {
    /// The static name was rejected.
    Name,
    /// The static schema was rejected.
    Schema,
    /// One static sanitized error detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
}

impl fmt::Display for SessionStatusToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "session_status_update static name is invalid",
            Self::Schema => "session_status_update static schema is invalid",
            Self::ErrorDetail => "session_status_update static error detail is invalid",
            Self::Duplicate => "session_status_update catalog is duplicated",
        })
    }
}

impl Error for SessionStatusToolConstructionError {}

/// Compiled catalog entry and matching executor for session status replacement.
///
/// Effect posture: `ExternalEffect`. Execution durably replaces metadata and
/// carries the exact tool request as last-writer agency.
#[derive(Clone, Debug)]
pub struct SessionStatusTool<Writer> {
    catalog: CompiledToolCatalog,
    executor: SessionStatusExecutor<Writer>,
}

impl<Writer> ToolContract for SessionStatusTool<Writer> {
    type Arguments = SessionStatusUpdateArguments;
    const NAME: &'static str = SESSION_STATUS_UPDATE_NAME;
    const DESCRIPTION: &'static str =
        "Replaces the current session's complete title, tags, attributes, and archive status.";
}

/// Typed `session_status_update` argument shape; decoder and rendered schema
/// share it. Field order is the declared `required` order.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionStatusUpdateArguments {
    /// Optional exact session title.
    title: SessionStatusTitle,
    /// Complete exact session tag set.
    tags: Vec<String>,
    /// Complete exact machine-facing attribute map.
    attributes: std::collections::BTreeMap<String, String>,
    /// Whether the session is hidden from the default view.
    archived: bool,
}

/// One nullable-but-required title member.
///
/// The replacement seam distinguishes "no title" from an absent member: the
/// member must be present, and `null` — not omission — states titlelessness,
/// so this newtype keeps the member required while admitting `null`.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(transparent)]
struct SessionStatusTitle(Option<String>);

impl schemars::JsonSchema for SessionStatusTitle {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("SessionStatusTitle")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": ["string", "null"],
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

impl SessionStatusTool<PostgresSessionStatusWriter> {
    /// Composes the production writer around the existing metadata application
    /// service and PostgreSQL transaction.
    pub fn try_new_postgres(pool: PgPool) -> Result<Self, SessionStatusToolConstructionError> {
        Self::try_new(PostgresSessionStatusWriter::new(pool))
    }
}

impl<Writer> SessionStatusTool<Writer> {
    /// Compiles immutable metadata around one injected writer.
    pub fn try_new(writer: Writer) -> Result<Self, SessionStatusToolConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| SessionStatusToolConstructionError::ErrorDetail)?;
        let session_not_found_detail =
            ToolExecutionErrorDetail::try_new(String::from(SESSION_NOT_FOUND_DETAIL))
                .map_err(|_| SessionStatusToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Confirm,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => SessionStatusToolConstructionError::Name,
            ToolContractCompileError::Schema => SessionStatusToolConstructionError::Schema,
        })?;
        let compiled = CompiledTool::new(
            definition,
            SessionStatusArgumentValidator {
                detail: invalid_arguments_detail,
            },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| SessionStatusToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: SessionStatusExecutor {
                writer,
                session_not_found_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, SessionStatusExecutor<Writer>) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct SessionStatusArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for SessionStatusArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// One complete, exactly attributed session-metadata replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStatusWrite {
    command: DurableCommandId,
    session: SessionId,
    request: ToolRequestId,
    replacement: SessionMetadataContent,
}

impl SessionStatusWrite {
    /// Returns the durable command identity derived from the tool attempt.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the invocation's current session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact tool request attributed as last writer.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Borrows the complete replacement admitted before dispatch.
    pub const fn replacement(&self) -> &SessionMetadataContent {
        &self.replacement
    }
}

/// Authoritative result of one status write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionStatusWriteOutcome {
    /// The replacement committed or an equal replay returned its receipt.
    Applied(SessionMetadataSnapshot),
    /// The invocation's current session did not exist.
    SessionNotFound,
    /// Commit acknowledgement did not establish whether the write committed.
    Ambiguous,
}

/// Injectable application-facing metadata writer.
pub trait SessionStatusWriter: Send {
    /// Sanitized adapter failure carrying no metadata content.
    type Error: ClassifyOperatorFailure;

    /// Applies exactly one complete replacement through the durable command
    /// seam.
    fn write(
        &mut self,
        update: SessionStatusWrite,
    ) -> impl Future<Output = Result<SessionStatusWriteOutcome, Self::Error>> + Send;
}

/// PostgreSQL-backed writer using `ReplaceSessionMetadataService`.
#[derive(Clone, Debug)]
pub struct PostgresSessionStatusWriter {
    pool: PgPool,
}

impl PostgresSessionStatusWriter {
    /// Uses the supplied pool for one command transaction per invocation.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Sanitized PostgreSQL metadata-writer failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostgresSessionStatusWriterError {
    /// Request correlation did not form a valid durable command identity.
    InvalidCommandIdentity,
    /// PostgreSQL failed outside the final ambiguous commit acknowledgement.
    Database,
    /// A freshly derived command identity collided with durable identity.
    IdentityCollision,
    /// Stored metadata command facts were inconsistent.
    Corruption,
}

impl fmt::Display for PostgresSessionStatusWriterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCommandIdentity => "session status command identity is invalid",
            Self::Database => "session status database operation failed",
            Self::IdentityCollision => "session status command identity collided",
            Self::Corruption => "session status durable facts are inconsistent",
        })
    }
}

impl Error for PostgresSessionStatusWriterError {}

impl ClassifyOperatorFailure for PostgresSessionStatusWriterError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::InvalidCommandIdentity => OperatorFailureClass::CallerOrHubBug,
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Database => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Corruption => OperatorFailureClass::FailClosedCorruption,
        }
    }
}

impl SessionStatusWriter for PostgresSessionStatusWriter {
    type Error = PostgresSessionStatusWriterError;

    async fn write(
        &mut self,
        update: SessionStatusWrite,
    ) -> Result<SessionStatusWriteOutcome, Self::Error> {
        let request = ReplaceSessionMetadataRequest::try_new_for_tool(
            update.command,
            update.session,
            update.request,
            update.replacement,
        )
        .map_err(|_| PostgresSessionStatusWriterError::InvalidCommandIdentity)?;
        let mut service =
            ReplaceSessionMetadataService::new(SessionMetadataRepository::new(self.pool.clone()));
        match service.execute(request).await {
            Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
                applied,
            ))) => Ok(SessionStatusWriteOutcome::Applied(
                applied.snapshot().clone(),
            )),
            Ok(ReplaceSessionMetadataOutcome::Recorded(
                ReplaceSessionMetadataResult::Rejected(
                    ReplaceSessionMetadataRejectedResult::SessionNotFound(_),
                ),
            )) => Ok(SessionStatusWriteOutcome::SessionNotFound),
            Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => {
                Err(PostgresSessionStatusWriterError::IdentityCollision)
            }
            Err(SessionMetadataRepositoryError::CommitAmbiguous(_)) => {
                Ok(SessionStatusWriteOutcome::Ambiguous)
            }
            Err(SessionMetadataRepositoryError::Database(_)) => {
                Err(PostgresSessionStatusWriterError::Database)
            }
            Err(
                SessionMetadataRepositoryError::DifferentCommandKind { .. }
                | SessionMetadataRepositoryError::Corruption(_),
            ) => Err(PostgresSessionStatusWriterError::Corruption),
        }
    }
}

/// Daemon-local session-status executor.
///
/// Effect posture: `ExternalEffect`. The executor replaces durable metadata and
/// carries the exact tool request as last-writer agency; crash or commit
/// ambiguity therefore cannot be described as effect-free.
#[derive(Clone, Debug)]
pub struct SessionStatusExecutor<Writer> {
    writer: Writer,
    session_not_found_detail: ToolExecutionErrorDetail,
}

/// Failure inside the session-status executor.
#[derive(Debug)]
pub enum SessionStatusExecutorError<WriterError> {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// The injected writer failed without trustworthy tool evidence.
    Writer(WriterError),
    /// The writer returned an applied snapshot for different admitted facts.
    WriterContract,
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
}

impl<WriterError> fmt::Display for SessionStatusExecutorError<WriterError>
where
    WriterError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("session_status_update argument validation drifted")
            }
            Self::Writer(error) => error.fmt(formatter),
            Self::WriterContract => {
                formatter.write_str("session_status_update writer contract was violated")
            }
            Self::ResultEncoding => {
                formatter.write_str("session_status_update result encoding failed")
            }
        }
    }
}

impl<WriterError> Error for SessionStatusExecutorError<WriterError>
where
    WriterError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Writer(error) => Some(error),
            Self::ArgumentValidationDrift | Self::WriterContract | Self::ResultEncoding => None,
        }
    }
}

impl<WriterError> ClassifyOperatorFailure for SessionStatusExecutorError<WriterError>
where
    WriterError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ArgumentValidationDrift | Self::WriterContract | Self::ResultEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Writer(error) => error.operator_failure_class(),
        }
    }
}

impl<Writer> ToolExecutor for SessionStatusExecutor<Writer>
where
    Writer: SessionStatusWriter,
{
    type Error = SessionStatusExecutorError<Writer::Error>;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let replacement = decode_arguments(invocation.request().arguments())
            .map_err(|_| SessionStatusExecutorError::ArgumentValidationDrift)?;
        let session = invocation.correlation().session();
        let result = session_status_result_text(session, &replacement)
            .map_err(|_| SessionStatusExecutorError::ResultEncoding)?;
        let update = SessionStatusWrite {
            command: DurableCommandId::from_uuid(invocation.correlation().attempt().into_uuid()),
            session,
            request: invocation.correlation().request(),
            replacement: replacement.clone(),
        };
        let evidence = match self
            .writer
            .write(update)
            .await
            .map_err(SessionStatusExecutorError::Writer)?
        {
            SessionStatusWriteOutcome::Applied(snapshot)
                if applied_snapshot_authenticates(
                    &snapshot,
                    session,
                    invocation.correlation().request(),
                    &replacement,
                ) =>
            {
                ToolExecutorEvidence::CompletedText(result)
            }
            SessionStatusWriteOutcome::Applied(_) => {
                return Err(SessionStatusExecutorError::WriterContract);
            }
            SessionStatusWriteOutcome::SessionNotFound => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.session_not_found_detail.clone()),
            },
            SessionStatusWriteOutcome::Ambiguous => ToolExecutorEvidence::Ambiguous,
        };
        Ok(invocation.bind(evidence))
    }
}

fn applied_snapshot_authenticates(
    snapshot: &SessionMetadataSnapshot,
    session: SessionId,
    request: ToolRequestId,
    replacement: &SessionMetadataContent,
) -> bool {
    snapshot.session() == session
        && snapshot.content() == replacement
        && snapshot
            .last_writer()
            .is_some_and(|writer| writer.actor() == Actor::Tool { request })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidSessionStatusArguments;

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<SessionMetadataContent, InvalidSessionStatusArguments> {
    let content = decode_metadata_content(arguments)?;
    session_status_result_text(SessionId::from_uuid(uuid::Uuid::nil()), &content)?;
    Ok(content)
}

fn decode_metadata_content(
    arguments: &NormalizedToolArguments,
) -> Result<SessionMetadataContent, InvalidSessionStatusArguments> {
    let decoded: SessionStatusUpdateArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidSessionStatusArguments)?;
    SessionMetadataContent::try_new(
        decoded.title.0,
        decoded.tags,
        decoded.attributes.into_iter().collect(),
        decoded.archived,
    )
    .map_err(|_| InvalidSessionStatusArguments)
}

fn session_status_result_text(
    session: SessionId,
    content: &SessionMetadataContent,
) -> Result<String, InvalidSessionStatusArguments> {
    let attributes: std::collections::BTreeMap<&str, &str> = content.attributes().collect();
    let tags: Vec<&str> = content.tags().collect();
    let result = serde_json::to_string(&serde_json::json!({
        "archived": content.archived(),
        "attributes": attributes,
        "session_id": session.into_uuid().to_string(),
        "tags": tags,
        "title": content.title(),
    }))
    .map_err(|_| InvalidSessionStatusArguments)?;
    ToolResultText::try_new(result)
        .map(ToolResultText::into_string)
        .map_err(|_| InvalidSessionStatusArguments)
}

#[cfg(test)]
mod tests {
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
    use signalbox_domain::{SessionMetadataLastWriter, SessionMetadataUpdatedAt};
    use uuid::Uuid;

    use super::*;

    // Canonical arguments with this many escaped control characters remain
    // under the 1 MiB argument bound; adding the fixed session-id receipt field
    // places the canonical result three bytes over its independent 1 MiB bound.
    const RECEIPT_OVERFLOW_CONTROL_CHARACTERS: usize = 174_744;
    const FIXTURE_COMMAND_ID: u128 = 1;
    const FIXTURE_SESSION_ID: u128 = 2;
    const FIXTURE_REQUEST_ID: u128 = 3;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    /// Durable metadata mutation is confirmed and external-effecting.
    #[test]
    fn session_status_definition_carries_exact_policy() {
        let (catalog, _executor) = SessionStatusTool::try_new(RejectingWriter)
            .expect("static session_status_update tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("session_status_update is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), SESSION_STATUS_UPDATE_NAME);
        assert_eq!(
            definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    /// The complete rendered wire schema. The pretty golden is the review
    /// surface; the byte-exact assertion pins the canonical compact form the
    /// registry stores and providers receive as its exact serialization.
    #[test]
    fn session_status_rendered_schema_is_the_exact_wire_artifact() {
        let (catalog, _executor) = SessionStatusTool::try_new(RejectingWriter)
            .expect("static session_status_update tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "archived": {
                  "description": "Whether the session is hidden from the default view.",
                  "type": "boolean"
                },
                "attributes": {
                  "additionalProperties": {
                    "type": "string"
                  },
                  "description": "Complete exact machine-facing attribute map.",
                  "type": "object"
                },
                "tags": {
                  "description": "Complete exact session tag set.",
                  "items": {
                    "type": "string"
                  },
                  "type": "array"
                },
                "title": {
                  "description": "Optional exact session title.",
                  "type": [
                    "string",
                    "null"
                  ]
                }
              },
              "required": [
                "title",
                "tags",
                "attributes",
                "archived"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), schema.to_string());
    }

    /// Typed decoding accepts exactly the existing complete metadata shape.
    #[test]
    fn session_status_typed_decode_accepts_complete_metadata() {
        let (catalog, _executor) = SessionStatusTool::try_new(RejectingWriter)
            .expect("static session_status_update tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert_eq!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(
                    r#"{"archived":false,"attributes":{"phase":"review"},"tags":["tooling"],"title":"Tool batch"}"#,
                ),
            ),
            Ok(())
        );
    }

    /// Typed decoding rejects a partial patch because the application seam
    /// replaces one complete snapshot.
    #[test]
    fn session_status_typed_decode_rejects_partial_metadata() {
        let (catalog, _executor) = SessionStatusTool::try_new(RejectingWriter)
            .expect("static session_status_update tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"attributes":{"phase":"review"}}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Argument admission rejects metadata whose canonical success receipt
    /// would exceed the independent result-text bound.
    #[test]
    fn session_status_typed_decode_rejects_oversized_receipt() {
        let supplied = serde_json::json!({
            "archived": false,
            "attributes": {
                "k": "\u{0001}".repeat(RECEIPT_OVERFLOW_CONTROL_CHARACTERS)
            },
            "tags": [],
            "title": null,
        })
        .to_string();
        let normalized = arguments(&supplied);

        assert!(decode_metadata_content(&normalized).is_ok());
        assert_eq!(
            decode_arguments(&normalized),
            Err(InvalidSessionStatusArguments)
        );
    }

    /// Injected writers can inspect every admitted write fact without
    /// depending on the PostgreSQL adapter.
    #[test]
    fn session_status_write_exposes_application_seam_facts() {
        let command = DurableCommandId::from_uuid(Uuid::from_u128(FIXTURE_COMMAND_ID));
        let session = SessionId::from_uuid(Uuid::from_u128(FIXTURE_SESSION_ID));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(FIXTURE_REQUEST_ID));
        let replacement = SessionMetadataContent::empty();
        let write = SessionStatusWrite {
            command,
            session,
            request,
            replacement: replacement.clone(),
        };

        assert_eq!(write.command(), command);
        assert_eq!(write.session(), session);
        assert_eq!(write.request(), request);
        assert_eq!(write.replacement(), &replacement);
    }

    /// Applied evidence is accepted only when the recorded writer is the exact
    /// tool request that produced the invocation.
    #[test]
    fn session_status_applied_snapshot_authenticates_exact_tool_request() {
        let session = SessionId::from_uuid(Uuid::from_u128(FIXTURE_SESSION_ID));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(FIXTURE_REQUEST_ID));
        let replacement = SessionMetadataContent::empty();
        let snapshot = SessionMetadataSnapshot::from_recorded_write(
            session,
            replacement.clone(),
            SessionMetadataLastWriter::new(
                SessionMetadataUpdatedAt::from_unix_micros(17),
                Actor::Tool { request },
            ),
        );

        assert!(applied_snapshot_authenticates(
            &snapshot,
            session,
            request,
            &replacement
        ));
    }

    /// An initial projection cannot authenticate an applied write even when
    /// its empty content matches the requested replacement.
    #[test]
    fn session_status_initial_snapshot_does_not_authenticate_applied_write() {
        let session = SessionId::from_uuid(Uuid::from_u128(FIXTURE_SESSION_ID));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(FIXTURE_REQUEST_ID));
        let replacement = SessionMetadataContent::empty();
        let snapshot = SessionMetadataSnapshot::initial(session);

        assert!(!applied_snapshot_authenticates(
            &snapshot,
            session,
            request,
            &replacement
        ));
    }

    /// Owner attribution cannot authenticate a matching tool replacement.
    #[test]
    fn session_status_owner_snapshot_does_not_authenticate_tool_write() {
        let session = SessionId::from_uuid(Uuid::from_u128(FIXTURE_SESSION_ID));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(FIXTURE_REQUEST_ID));
        let replacement = SessionMetadataContent::empty();
        let snapshot = SessionMetadataSnapshot::from_recorded_write(
            session,
            replacement.clone(),
            SessionMetadataLastWriter::new(
                SessionMetadataUpdatedAt::from_unix_micros(17),
                Actor::Owner,
            ),
        );

        assert!(!applied_snapshot_authenticates(
            &snapshot,
            session,
            request,
            &replacement
        ));
    }

    /// A durable command already using the freshly attempt-derived identity is
    /// an operational identity collision rather than a caller defect.
    #[test]
    fn session_status_command_reuse_is_identity_collision() {
        assert_eq!(
            PostgresSessionStatusWriterError::IdentityCollision.operator_failure_class(),
            OperatorFailureClass::IdentityCollision
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct RejectingWriterError;

    impl fmt::Display for RejectingWriterError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fixture writer is not invoked")
        }
    }

    impl Error for RejectingWriterError {}

    impl ClassifyOperatorFailure for RejectingWriterError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::CallerOrHubBug
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct RejectingWriter;

    impl SessionStatusWriter for RejectingWriter {
        async fn write(
            &mut self,
            _update: SessionStatusWrite,
        ) -> Result<SessionStatusWriteOutcome, Self::Error> {
            Err(RejectingWriterError)
        }

        type Error = RejectingWriterError;
    }
}
