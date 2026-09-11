//! Session workflow authority and adapters to the shared program commands.

use super::WorkspaceInstructionRootResolver;
use crate::{
    process_runtime::{ProtocolError, program},
    workflows::WorkflowService,
};
use serde_json::{Value, json};
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass, ToolExecutorEvidence};
use signalbox_domain::{
    ProgramRunId, SessionId, ToolExecutionErrorDetail, ToolRequest, ToolResultText,
};
use signalbox_persistence::program_registration::ProgramRegistrationRepository;
use signalbox_process_protocol::{
    CanonicalUuid, ErrorCode, ProgramExecutableInput, ProgramRegistrationInput, ProtocolVersion,
    RequestId,
};
use signalbox_tools_workflows::{Operation, WorkflowPolicy, WorkflowPort, WorkflowRequest};
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceFileSystem};
use sqlx::{PgPool, Row};
use std::{io::Read, path::Path};
use uuid::Uuid;

/// Retained template policy lookup for model composition and workflow execution.
#[derive(Clone, Debug)]
pub struct WorkflowToolPolicy {
    pool: PgPool,
}

impl WorkflowToolPolicy {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn for_session(
        &self,
        session: SessionId,
    ) -> Result<WorkflowPolicy, WorkflowToolError> {
        let policy: Option<sqlx::types::Json<WorkflowPolicy>> = sqlx::query_scalar("SELECT snapshot.workflow_tools FROM session LEFT JOIN session_workflow_template_snapshot AS snapshot USING (template_name, template_content_digest) WHERE session_id = $1")
            .bind(session.into_uuid()).fetch_one(&self.pool).await?;
        Ok(policy.map(|policy| policy.0).unwrap_or_default())
    }
}

/// Workflow adapter bound to the daemon runner and existing workspace authority.
#[derive(Clone, Debug)]
pub struct DaemonWorkflowPort {
    policy: WorkflowToolPolicy,
    workflows: WorkflowService,
    roots: Option<WorkspaceInstructionRootResolver>,
}

impl DaemonWorkflowPort {
    pub fn new(
        policy: WorkflowToolPolicy,
        workflows: WorkflowService,
        roots: Option<WorkspaceInstructionRootResolver>,
    ) -> Self {
        Self {
            policy,
            workflows,
            roots,
        }
    }

    async fn run(
        &self,
        caller: &ToolRequest,
        request: WorkflowRequest,
    ) -> Result<Value, WorkflowToolError> {
        let pool = &self.policy.pool;
        let policy = self.policy.for_session(caller.session()).await?;
        let operation = request.operation();
        let registrations = ProgramRegistrationRepository::new(pool.clone());
        let target = match &request {
            WorkflowRequest::Start(args) => Some(args.name.clone()),
            WorkflowRequest::Register(args) => Some(args.name.clone()),
            WorkflowRequest::Replay { run } => registrations
                .for_run(ProgramRunId::from_uuid(*run))
                .await
                .map_err(|error| {
                    WorkflowToolError::Protocol(program::workflow_error(error.into()).code)
                })?
                .map(|registration| registration.content.name),
            _ => None,
        };
        if !policy.permits(operation, target.as_deref()) {
            return Err(WorkflowToolError::GrantDenied);
        }
        // The daemon minted this logical request before any physical tool attempt.
        let mutation = caller.id().into_uuid();
        let wire_mutation = CanonicalUuid::from_uuid(mutation);
        let correlation = RequestId::try_new(u64::MAX).map_err(|_| WorkflowToolError::Contract)?;
        match request {
            WorkflowRequest::List { after } => self.list(caller.session(), after).await,
            WorkflowRequest::Read { run } => {
                let view = program::read_program(
                    pool,
                    ProtocolVersion::One,
                    correlation,
                    CanonicalUuid::from_uuid(run),
                )
                .await?;
                let metadata = sqlx::query("SELECT last_position::text AS journal_length FROM program_run_journal_sequence_state WHERE run_id = $1")
                    .bind(run).fetch_one(pool).await?;
                let mut result = json!({"run_id":run.to_string(), "journal_length":metadata.try_get::<String,_>("journal_length")?, "run":view});
                bound_run_bytes(&mut result)?;
                Ok(result)
            }
            WorkflowRequest::Start(args) => {
                let registration: Option<Uuid> = sqlx::query_scalar("SELECT registration_id FROM program_registration WHERE name = $1 AND revision = $2")
                    .bind(args.name).bind(args.revision).fetch_optional(pool).await?;
                let registration =
                    registration.ok_or(WorkflowToolError::Protocol(ErrorCode::NotFound))?;
                program::start_program(
                    Some(&self.workflows),
                    wire_mutation,
                    CanonicalUuid::from_uuid(registration),
                    args.input,
                )
                .await?;
                Ok(
                    json!({"run_id":mutation.to_string(),"registration_id":registration.to_string()}),
                )
            }
            WorkflowRequest::Replay { run } => {
                let original = ProgramRunId::from_uuid(run);
                let registration = registrations
                    .for_run(original)
                    .await
                    .map_err(|error| {
                        WorkflowToolError::Protocol(program::workflow_error(error.into()).code)
                    })?
                    .ok_or(WorkflowToolError::Protocol(ErrorCode::NotFound))?;
                let input = registrations
                    .input_for_run(original)
                    .await
                    .map_err(|error| {
                        WorkflowToolError::Protocol(program::workflow_error(error.into()).code)
                    })?
                    .ok_or(WorkflowToolError::Contract)?;
                program::start_program(
                    Some(&self.workflows),
                    wire_mutation,
                    CanonicalUuid::from_uuid(registration.id.into_uuid()),
                    input.as_bytes().to_vec(),
                )
                .await?;
                Ok(
                    json!({"run_id":mutation.to_string(),"registration_id":registration.id.into_uuid().to_string()}),
                )
            }
            WorkflowRequest::Stop { run } => {
                let outcome = program::cancel_program(
                    pool,
                    Some(&self.workflows),
                    ProtocolVersion::One,
                    correlation,
                    signalbox_process_protocol::CommandId::try_from_uuid(mutation)
                        .map_err(|_| WorkflowToolError::Contract)?,
                    CanonicalUuid::from_uuid(run),
                )
                .await?;
                let mut value = json!({"command_id":mutation.to_string(),"run_id":run.to_string(),"outcome":outcome});
                bound_result(&mut value, "/outcome/result", "/outcome/result_extent")?;
                Ok(value)
            }
            WorkflowRequest::Register(args) => {
                let retained = sqlx::query("SELECT source, artifact FROM session_workflow_registration_source WHERE request_id = $1")
                    .bind(mutation).fetch_optional(pool).await?;
                let retained = match retained {
                    Some(retained) => retained,
                    None => {
                        let roots = self.roots.as_ref().ok_or(WorkflowToolError::Workspace)?;
                        let root = roots
                            .resolve(caller.session())
                            .await
                            .map_err(|_| WorkflowToolError::Workspace)?;
                        let filesystem = LocalWorkspaceFileSystem;
                        let root = filesystem
                            .open_root(&root)
                            .map_err(|_| WorkflowToolError::Workspace)?;
                        let read = |path: &str| -> Result<Vec<u8>, WorkflowToolError> {
                            let file = filesystem
                                .open_file_stream(&root, Path::new(path))
                                .map_err(|_| WorkflowToolError::Workspace)?;
                            let mut bytes = Vec::new();
                            file.take(signalbox_process_protocol::MAX_FRAME_BYTES as u64 + 1)
                                .read_to_end(&mut bytes)
                                .map_err(|_| WorkflowToolError::Workspace)?;
                            if bytes.len() > signalbox_process_protocol::MAX_FRAME_BYTES {
                                return Err(WorkflowToolError::Workspace);
                            }
                            Ok(bytes)
                        };
                        let source = read(&args.source_path)?;
                        let artifact = String::from_utf8(read(&args.artifact_path)?)
                            .map_err(|_| WorkflowToolError::Workspace)?;
                        if artifact.contains('\0') {
                            return Err(WorkflowToolError::Protocol(ErrorCode::InvalidRequest));
                        }
                        sqlx::query("INSERT INTO session_workflow_registration_source (request_id, source, artifact) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
                            .bind(mutation).bind(source).bind(artifact).execute(pool).await?;
                        sqlx::query("SELECT source, artifact FROM session_workflow_registration_source WHERE request_id = $1")
                            .bind(mutation).fetch_one(pool).await?
                    }
                };
                let source = retained.try_get("source")?;
                let artifact = retained.try_get("artifact")?;
                let grants = serde_json::from_value(
                    serde_json::to_value(args.grants).map_err(|_| WorkflowToolError::Contract)?,
                )
                .map_err(|_| WorkflowToolError::Contract)?;
                program::register_program(
                    Some(&self.workflows),
                    wire_mutation,
                    ProgramRegistrationInput {
                        name: args.name,
                        revision: args.revision,
                        executable: ProgramExecutableInput::JavaScript { source, artifact },
                        grants,
                    },
                )
                .await?;
                Ok(json!({"registration_id":mutation.to_string()}))
            }
        }
    }

    async fn list(
        &self,
        session: SessionId,
        after: Option<Uuid>,
    ) -> Result<Value, WorkflowToolError> {
        use futures_util::TryStreamExt;
        let mut rows = sqlx::query("SELECT run.run_id, registration.registration_id, registration.name, registration.revision, stream.created_at::text AS started_at, terminal.recorded_at::text AS terminal_at,
            CASE terminal.frame_kind WHEN 'answer' THEN 'succeeded' WHEN 'run_cancel' THEN 'cancelled' WHEN 'fault' THEN 'faulted' ELSE 'running' END AS state,
            EXISTS (SELECT 1 FROM tool_request AS request WHERE request.request_id = run.run_id AND request.session_id = $2 AND request.tool_name IN ('workflow_start', 'workflow_replay')) AS own_run
            FROM program_run_registration AS run JOIN program_registration AS registration USING (registration_id)
            JOIN program_run_journal_stream AS stream USING (run_id)
            LEFT JOIN LATERAL (SELECT entry.frame_kind, entry.recorded_at FROM program_run_journal_entry AS entry
                WHERE entry.run_id = run.run_id AND (entry.frame_kind IN ('run_cancel','fault') OR (entry.frame_kind = 'answer' AND EXISTS (SELECT 1 FROM program_run_journal_entry AS request WHERE request.run_id = entry.run_id AND request.frame_kind = 'terminal' AND request.request_ordinal = entry.resolves_request_ordinal))) ORDER BY entry.journal_position LIMIT 1) AS terminal ON true
            WHERE ($1::uuid IS NULL OR run.run_id > $1) ORDER BY run.run_id")
            .bind(after).bind(session.into_uuid()).fetch(&self.policy.pool);
        // Every cursor has the same encoded UUID width; reserve it before admitting rows.
        let mut output = json!({"runs":[],"next_after":Uuid::nil().to_string()});
        let mut encoded_bytes = output.to_string().len();
        let mut last = None;
        while let Some(row) = rows.try_next().await? {
            let run: Uuid = row.try_get("run_id")?;
            let item = json!({"run_id":run.to_string(),"registration_id":row.try_get::<Uuid,_>("registration_id")?.to_string(),"name":row.try_get::<String,_>("name")?,"revision":row.try_get::<String,_>("revision")?,"state":row.try_get::<String,_>("state")?,"started_at":row.try_get::<String,_>("started_at")?,"terminal_at":row.try_get::<Option<String>,_>("terminal_at")?,"own_run":row.try_get::<bool,_>("own_run")?});
            let row_bytes = item.to_string().len() + usize::from(last.is_some());
            if row_bytes > ToolResultText::MAX_UTF8_BYTES - encoded_bytes {
                output["next_after"] = json!(last.ok_or(WorkflowToolError::ResultTooLarge)?);
                return Ok(output);
            }
            encoded_bytes += row_bytes;
            output["runs"]
                .as_array_mut()
                .ok_or(WorkflowToolError::Contract)?
                .push(item);
            last = Some(run.to_string());
        }
        output["next_after"] = Value::Null;
        Ok(output)
    }
}

fn fits(value: &Value) -> bool {
    ToolResultText::try_new(value.to_string()).is_ok()
}

fn bound_run_bytes(value: &mut Value) -> Result<(), WorkflowToolError> {
    // Input takes precedence, matching the socket's frame budgeting.
    bound_result(value, "/run/outcome/result", "/run/outcome/result_extent")?;
    bound_result(value, "/run/input", "/run/input_extent")?;
    if fits(value) {
        Ok(())
    } else {
        Err(WorkflowToolError::ResultTooLarge)
    }
}

fn bound_result(value: &mut Value, path: &str, extent_path: &str) -> Result<(), WorkflowToolError> {
    if fits(value) {
        return Ok(());
    }
    let Some(bytes) = value.pointer(path).and_then(Value::as_array).cloned() else {
        return Ok(());
    };
    let total = value
        .pointer(extent_path)
        .and_then(|extent| extent.get("total_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(bytes.len() as u64);
    *value
        .pointer_mut(extent_path)
        .ok_or(WorkflowToolError::Contract)? = json!({"kind":"truncated","total_bytes":total});
    let (mut low, mut high) = (0, bytes.len());
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        *value.pointer_mut(path).ok_or(WorkflowToolError::Contract)? =
            Value::Array(bytes[..middle].to_vec());
        if fits(value) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    *value.pointer_mut(path).ok_or(WorkflowToolError::Contract)? =
        Value::Array(bytes[..low].to_vec());
    Ok(())
}

/// Sanitized workflow adapter failure retaining infrastructure ambiguity.
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum WorkflowToolError {
    #[error("workflow database access failed")]
    Database(#[source] sqlx::Error),
    #[error("workflow program command failed")]
    Protocol(ErrorCode),
    #[error("workflow_grant_denied")]
    GrantDenied,
    #[error("workflow_workspace_source_unavailable")]
    Workspace,
    #[error("workflow result exceeds the tool bound")]
    ResultTooLarge,
    #[error("workflow adapter contract failed")]
    Contract,
}
impl From<sqlx::Error> for WorkflowToolError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
impl From<ProtocolError> for WorkflowToolError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error.code)
    }
}
impl ClassifyOperatorFailure for WorkflowToolError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) | Self::Protocol(ErrorCode::Unavailable) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Protocol(ErrorCode::CommitAmbiguous) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::Protocol(ErrorCode::Internal) | Self::Contract => {
                OperatorFailureClass::FailClosedCorruption
            }
            _ => OperatorFailureClass::CallerOrHubBug,
        }
    }
}
impl WorkflowPort for DaemonWorkflowPort {
    type Error = WorkflowToolError;
    async fn execute(
        &mut self,
        caller: &ToolRequest,
        request: WorkflowRequest,
    ) -> Result<ToolExecutorEvidence, Self::Error> {
        match self.run(caller, request).await {
            Ok(value) => Ok(ToolExecutorEvidence::CompletedText(value.to_string())),
            Err(
                error @ (WorkflowToolError::Database(_)
                | WorkflowToolError::Contract
                | WorkflowToolError::Protocol(
                    ErrorCode::Unavailable | ErrorCode::CommitAmbiguous | ErrorCode::Internal,
                )),
            ) => Err(error),
            Err(error) => {
                let detail = match error {
                    WorkflowToolError::Protocol(code) => {
                        serde_json::to_string(&code).map_err(|_| WorkflowToolError::Contract)?
                    }
                    error => error.to_string(),
                };
                Ok(ToolExecutorEvidence::KnownFailed {
                    detail: Some(
                        ToolExecutionErrorDetail::try_new(detail)
                            .map_err(|_| WorkflowToolError::Contract)?,
                    ),
                })
            }
        }
    }
}

/// Session-selected workflow postures over the unchanged family catalog.
#[derive(Clone)]
pub(crate) struct SessionWorkflowCatalog<Catalog> {
    pub catalog: Catalog,
    pub policy: WorkflowPolicy,
}
impl<Catalog: signalbox_application::ToolCatalog> signalbox_application::ToolCatalog
    for SessionWorkflowCatalog<Catalog>
{
    fn definitions(&self) -> Box<[signalbox_application::ToolDefinition]> {
        self.catalog
            .definitions()
            .into_vec()
            .into_iter()
            .map(|definition| self.apply(definition))
            .collect()
    }
    fn definition(
        &self,
        name: &signalbox_domain::ToolName,
    ) -> Option<signalbox_application::ToolDefinition> {
        self.catalog
            .definition(name)
            .map(|definition| self.apply(definition))
    }
    fn validate_arguments(
        &self,
        name: &signalbox_domain::ToolName,
        arguments: &signalbox_domain::NormalizedToolArguments,
    ) -> Result<(), signalbox_application::ToolCatalogValidationFailure> {
        self.catalog.validate_arguments(name, arguments)
    }
    fn preauthorization(
        &self,
        name: &signalbox_domain::ToolName,
        arguments: &signalbox_domain::NormalizedToolArguments,
    ) -> Result<
        signalbox_application::ToolPreauthorization,
        signalbox_application::ToolCatalogValidationFailure,
    > {
        self.catalog.preauthorization(name, arguments)
    }
}
impl<Catalog> SessionWorkflowCatalog<Catalog> {
    fn apply(
        &self,
        definition: signalbox_application::ToolDefinition,
    ) -> signalbox_application::ToolDefinition {
        match Operation::from_name(definition.name().as_str()) {
            Some(operation) => definition.with_approval_posture(self.policy.posture(operation)),
            None => definition,
        }
    }
}
