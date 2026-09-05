//! PostgreSQL transactions for durable tool approval and execution.
//!
//! Every mutating method reloads the complete batch under the session's
//! scheduler lock before asking the domain aggregate for authority. Executor
//! work remains outside database transactions.

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    num::NonZeroU64,
};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, CorrelatedDurableChildWait, DecideToolRequestTransaction,
    ModelCallCredentialReference, OperatorFailureClass, OverrideDeniedToolRequestTransaction,
    PrepareToolContinuationOutcome, RetainedToolAttemptObservationStatus,
    ToolAttemptAuthorizationOutcome, ToolAttemptAuthorizationStatus, ToolContinuationIdentities,
    ToolCrashClosureIdentities, ToolExecutionTransaction, ToolPreauthorization,
};
use signalbox_domain::{
    ActiveTurnPhase, CorrelatedToolAttemptObservation, CurrentToolAttempt, CurrentToolAttemptState,
    DangerousToolAutoApproval, DecideToolRequest, DecideToolRequestRejectedResult,
    DecideToolRequestResult, DelegateApprovalRecommendation, DelegateToolApproval,
    DelegationContent, DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationProvenanceReconstitutionInput, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, EndedToolAttempt, GoalGeneration, NormalizedToolArguments,
    OverrideDeniedToolRequest, OverrideDeniedToolRequestResult, PreparedDecideToolRequest,
    PreparedOverrideDeniedToolRequest, PreparedToolBatchDecision, PreparedToolResultProjection,
    ReconstitutedToolAttempt, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntryPayload, SessionId,
    ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput, ToolArgumentsKind,
    ToolAttemptDispatchCorrelation, ToolAttemptEnd, ToolAttemptId, ToolAttemptObservation,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolBatch,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionFailure,
    ToolBatchReconstitutionInput, ToolDenialReason, ToolDispatchAuthority, ToolDispatchGeneration,
    ToolEffectClass, ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorKind,
    ToolName, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultContent,
    ToolResultText, TurnId,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    approval_judge::FailedApprovalJudgeDisposition,
    command_registry::{
        self, CommandKind, DECIDE_TOOL_REQUEST_KIND, OVERRIDE_DENIED_TOOL_REQUEST_KIND,
        RegistryCorruption, RegistryInspectionError,
    },
    commit_failure_is_ambiguous,
    mapping::{
        ApprovalJudgeStateStorageKind, ApprovalJudgeTerminalDispositionStorageKind,
        BlobReadRejectionStorageKind, ToolApprovalDecisionSourceStorageKind,
        ToolAttemptDispositionStorageKind, approval_judge_state_to_str,
        approval_judge_terminal_disposition_to_str, blob_read_rejection_from_str,
        blob_read_rejection_to_str, dangerous_tool_auto_approval_from_str,
        durable_command_id_from_uuid, durable_command_id_to_uuid, positive_u64_from_numeric,
        session_id_from_uuid, session_id_to_uuid, tool_approval_decision_source_from_str,
        tool_approval_decision_source_to_str, tool_approval_posture_from_str,
        tool_attempt_disposition_from_str, tool_attempt_disposition_to_str,
        tool_attempt_id_from_uuid, tool_attempt_id_to_uuid, tool_request_id_from_uuid,
        tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    model_execution::{
        insert_prepared_call, insert_snapshot, lock_delegated_child_endpoint_sessions,
        lock_delegated_turn_terminal_frontier,
    },
    outbox::{self, OutboxEvent, ToolBatchOutboxState},
};

/// Largest decoded byte count one `blob_read` request may charge.
///
/// The durable admission here and the daemon's argument validator are the two
/// constructors of this bound, so it is declared once here and imported at the
/// tool boundary rather than restated there.
// numeric-bound: guard - prevents one blob_read tool response from exhausting turn memory
pub const MAX_BLOB_READ_TOOL_BYTES: u64 = 524_288;
// numeric-bound: guard - prevents accumulated blob_read responses in one turn from exhausting turn memory
const MAX_BLOB_READ_TURN_BYTES: u64 = 2_097_152;
// numeric-bound: guard - prevents unbounded blob_read requests in one turn from exhausting turn budget
const MAX_BLOB_READ_REQUESTS_PER_TURN: i64 = 64;

const BLOB_NOT_VISIBLE_DETAIL: &str = "blob_not_visible";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlobReadAdmission {
    Admitted,
    NotVisible,
    TurnByteBudgetExceeded,
    TurnReadCountExceeded,
}

impl BlobReadAdmission {
    /// Durable rejection spelling this admission records on its charge row.
    ///
    /// A visibility refusal returns before any charge exists, so it is absent
    /// here as well as on the admitted path.
    const fn rejection(self) -> Option<BlobReadRejectionStorageKind> {
        match self {
            Self::Admitted | Self::NotVisible => None,
            Self::TurnByteBudgetExceeded => {
                Some(BlobReadRejectionStorageKind::TurnByteBudgetExceeded)
            }
            Self::TurnReadCountExceeded => {
                Some(BlobReadRejectionStorageKind::TurnReadCountExceeded)
            }
        }
    }

    fn detail(self) -> Option<&'static str> {
        match self {
            Self::Admitted => None,
            Self::NotVisible => Some(BLOB_NOT_VISIBLE_DETAIL),
            Self::TurnByteBudgetExceeded | Self::TurnReadCountExceeded => {
                self.rejection().map(blob_read_rejection_to_str)
            }
        }
    }

    fn into_detail(self) -> Result<Option<ToolExecutionErrorDetail>, ToolLoopRepositoryError> {
        self.detail()
            .map(|detail| {
                ToolExecutionErrorDetail::try_new(String::from(detail)).map_err(|_| {
                    ToolLoopCorruption::Inconsistent("blob read rejection detail").into()
                })
            })
            .transpose()
    }

    fn from_charge(
        admitted: bool,
        rejection_reason: Option<String>,
    ) -> Result<Self, ToolLoopRepositoryError> {
        match (admitted, rejection_reason.as_deref()) {
            (true, None) => Ok(Self::Admitted),
            (false, Some(reason)) => match blob_read_rejection_from_str(reason) {
                Some(BlobReadRejectionStorageKind::TurnByteBudgetExceeded) => {
                    Ok(Self::TurnByteBudgetExceeded)
                }
                Some(BlobReadRejectionStorageKind::TurnReadCountExceeded) => {
                    Ok(Self::TurnReadCountExceeded)
                }
                None => Err(ToolLoopCorruption::Unsupported {
                    field: "rejection_reason",
                    value: reason.to_owned(),
                }
                .into()),
            },
            (true, Some(_)) | (false, None) => {
                Err(ToolLoopCorruption::Inconsistent("blob read rejection reason").into())
            }
        }
    }
}

const STORAGE_VERSION: i16 = 1;

/// Stored tool-loop facts failed checked domain reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolLoopCorruption {
    /// A required row or value is absent.
    Missing(&'static str),
    /// Stored facts disagree about an exact relationship.
    Inconsistent(&'static str),
    /// A closed discriminator has an unknown spelling.
    Unsupported {
        /// Storage field.
        field: &'static str,
        /// Unsupported value.
        value: String,
    },
    /// Complete batch facts failed aggregate reconstruction.
    Batch(ToolBatchReconstitutionFailure),
}

impl fmt::Display for ToolLoopCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(value) => write!(formatter, "missing tool-loop {value}"),
            Self::Inconsistent(value) => write!(formatter, "inconsistent tool-loop {value}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported tool-loop {field}: {value}")
            }
            Self::Batch(failure) => {
                write!(formatter, "tool batch reconstitution failed: {failure:?}")
            }
        }
    }
}

impl Error for ToolLoopCorruption {}

/// Database, replay, corruption, or rejected transition at the tool boundary.
#[derive(Debug)]
pub enum ToolLoopRepositoryError {
    /// PostgreSQL failure.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
    /// A fresh application-owned identity collided with durable state.
    IdentityCollision,
    /// Durable facts failed closed reconstruction.
    Corruption(ToolLoopCorruption),
    /// The command identity belongs to another durable command kind.
    DifferentCommandKind,
    /// The command identity is recorded with a different decision payload.
    ConflictingCommandReuse,
    /// Caller supplied a transition the current batch does not authorize.
    InvalidTransition(&'static str),
}

impl fmt::Display for ToolLoopRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "tool-loop database failure: {source}")
            }
            Self::IdentityCollision => {
                formatter.write_str("tool-loop identity candidate already exists")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::DifferentCommandKind => {
                formatter.write_str("command identity already belongs to another kind")
            }
            Self::ConflictingCommandReuse => {
                formatter.write_str("command replay payload differs from the durable command")
            }
            Self::InvalidTransition(value) => {
                write!(formatter, "tool-loop transition rejected: {value}")
            }
        }
    }
}

impl Error for ToolLoopRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision
            | Self::DifferentCommandKind
            | Self::ConflictingCommandReuse
            | Self::InvalidTransition(_) => None,
        }
    }
}

impl From<sqlx::Error> for ToolLoopRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        if let Some(database) = error.as_database_error()
            && database.code().as_deref() == Some("23505")
        {
            return match database.constraint() {
                Some(
                    "semantic_transcript_entry_pk"
                    | "semantic_transcript_entry_id_global"
                    | "tool_attempt_pkey"
                    | "turn_attempt_pkey",
                ) => Self::IdentityCollision,
                _ => Self::Corruption(ToolLoopCorruption::Inconsistent(
                    "logical uniqueness constraint",
                )),
            };
        }
        Self::Database {
            source: error,
            commit_ambiguous: false,
        }
    }
}

impl From<ToolLoopCorruption> for ToolLoopRepositoryError {
    fn from(error: ToolLoopCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl ClassifyOperatorFailure for ToolLoopRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::DifferentCommandKind
            | Self::ConflictingCommandReuse
            | Self::InvalidTransition(_) => OperatorFailureClass::CallerOrHubBug,
        }
    }
}

/// PostgreSQL adapter for serialized tool-loop transactions.
#[derive(Clone, Debug)]
pub struct PostgresToolLoopRepository {
    pool: PgPool,
    continuation_targets: Option<signalbox_domain::ModelTargetCatalog>,
    continuation_credential: Option<ModelCallCredentialReference>,
    credential_families: Option<crate::ModelCredentialFamilyCatalog>,
    credential_pools: crate::model_execution::CredentialPoolRuntimeCatalog,
    cache_inclusive_input_targets: HashSet<signalbox_domain::ResolvedProviderTarget>,
    continuation_usage_limits: crate::model_execution::ToolContinuationUsageLimitCatalog,
}

impl PostgresToolLoopRepository {
    /// Uses the shared production pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            continuation_targets: None,
            continuation_credential: None,
            credential_families: None,
            credential_pools: Default::default(),
            cache_inclusive_input_targets: HashSet::new(),
            continuation_usage_limits: Default::default(),
        }
    }

    /// Uses the shared pool plus immutable model-call configuration required
    /// by atomic tool-result continuation.
    pub fn with_model_calls(
        pool: PgPool,
        targets: signalbox_domain::ModelTargetCatalog,
        credential_reference: ModelCallCredentialReference,
    ) -> Self {
        Self {
            pool,
            continuation_targets: Some(targets),
            continuation_credential: Some(credential_reference),
            credential_families: None,
            credential_pools: Default::default(),
            cache_inclusive_input_targets: HashSet::new(),
            continuation_usage_limits: Default::default(),
        }
    }

    pub(crate) fn with_cache_inclusive_input_targets(
        mut self,
        targets: HashSet<signalbox_domain::ResolvedProviderTarget>,
    ) -> Self {
        self.cache_inclusive_input_targets = targets;
        self
    }

    pub(crate) fn with_continuation_usage_limits(
        mut self,
        limits: crate::model_execution::ToolContinuationUsageLimitCatalog,
    ) -> Self {
        self.continuation_usage_limits = limits;
        self
    }

    /// Selects continuation credentials from the session's latest snapshot.
    pub fn with_session_credentials(
        mut self,
        credential_families: Option<crate::ModelCredentialFamilyCatalog>,
    ) -> Self {
        self.credential_families = credential_families;
        self
    }

    /// Enables pool selection for every same-turn continuation call.
    pub fn with_credential_pools(
        mut self,
        credential_pools: crate::model_execution::CredentialPoolRuntimeCatalog,
    ) -> Self {
        self.credential_pools = credential_pools;
        self
    }

    /// Reloads the active logical batch without granting mutation authority.
    pub async fn load_active_batch(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, session).await?;
        let result = load_active_batch_from_connection(&mut transaction, session, turn).await;
        transaction.rollback().await?;
        result
    }

    /// Finds the exact active turn whose durable execution can make progress.
    ///
    /// This is a reconciliation hint only. Every later tool transaction
    /// rechecks the complete batch under the session scheduler lock.
    pub async fn find_resumable_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<TurnId>, ToolLoopRepositoryError> {
        let turn = sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1
                AND state_kind = 'active'
                AND goal_turn_is_runtime_relevant(session_id, turn_id)
                AND (
                    EXISTS (
                        SELECT 1
                          FROM model_call AS prepared
                         WHERE prepared.session_id = turn_lifecycle.session_id
                           AND prepared.turn_id = turn_lifecycle.turn_id
                           AND prepared.turn_attempt_id =
                               turn_lifecycle.current_attempt_id
                           AND prepared.state_kind = 'prepared'
                    )
                    OR (
                        active_tool_round_call_id IS NOT NULL
                        AND (
                            active_phase_kind = 'running'
                            OR (
                                active_phase_kind = 'awaiting_child'
                                AND EXISTS (
                                    SELECT 1
                                      FROM session_delegation_wait AS waiting
                                      JOIN session_child_result_delivery AS delivery
                                        ON delivery.awaiting_tool_request_id =
                                           waiting.awaiting_tool_request_id
                                       AND delivery.spawning_tool_request_id =
                                           waiting.spawning_tool_request_id
                                       AND delivery.parent_session_id =
                                           waiting.parent_session_id
                                       AND delivery.delivery_sequence IS NULL
                                     WHERE waiting.awaiting_tool_request_id =
                                           turn_lifecycle.child_wait_request_id
                                       AND waiting.parent_session_id =
                                           turn_lifecycle.session_id
                                       AND waiting.parent_turn_id =
                                           turn_lifecycle.turn_id
                                       AND waiting.wait_mode = 'foreground'
                                )
                            )
                            OR (
                                active_phase_kind = 'awaiting_tool_approval'
                                AND EXISTS (
                                    SELECT 1
                                      FROM tool_request AS request
                                     WHERE request.request_id = approval_tool_request_id
                                       AND request.session_id =
                                           turn_lifecycle.session_id
                                       AND request.turn_id = turn_lifecycle.turn_id
                                       AND request.approval_posture = 'delegated'
                                       AND NOT EXISTS (
                                            SELECT 1
                                              FROM tool_approval_judge_model_call AS judge
                                             WHERE judge.request_id = request.request_id
                                               AND judge.state_kind = 'terminal'
                                       )
                                )
                            )
                        )
                    )
                )",
        )
        .bind(session_id_to_uuid(session))
        .fetch_optional(&self.pool)
        .await?;
        Ok(turn.map(turn_id_from_uuid))
    }

    /// Finds an active relevant turn that has not prepared its first model call.
    pub async fn find_dispatch_start_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<TurnId>, ToolLoopRepositoryError> {
        let turn = sqlx::query_scalar::<_, Uuid>(
            "SELECT lifecycle.turn_id
               FROM turn_lifecycle AS lifecycle
              WHERE lifecycle.session_id = $1
                AND lifecycle.state_kind = 'active'
                AND NOT lifecycle.delegation_runtime_terminal
                AND goal_turn_is_runtime_relevant(
                    lifecycle.session_id, lifecycle.turn_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM model_call AS call
                     WHERE call.session_id = lifecycle.session_id
                       AND call.turn_id = lifecycle.turn_id
                )
              ORDER BY lifecycle.acceptance_position
              LIMIT 1",
        )
        .bind(session_id_to_uuid(session))
        .fetch_optional(&self.pool)
        .await?;
        Ok(turn.map(turn_id_from_uuid))
    }

    /// Atomically reopens one delivered foreground child wait as a fresh turn attempt.
    pub async fn resume_child_wait(
        &self,
        session: SessionId,
        turn: TurnId,
        continuation: signalbox_domain::TurnAttemptId,
    ) -> Result<bool, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let predecessor = sqlx::query_scalar::<_, Uuid>(
                "SELECT attempt.issuing_turn_attempt_id
                   FROM turn_lifecycle AS lifecycle
                   JOIN session_delegation_wait AS waiting
                     ON waiting.awaiting_tool_request_id =
                        lifecycle.child_wait_request_id
                    AND waiting.parent_session_id = lifecycle.session_id
                    AND waiting.parent_turn_id = lifecycle.turn_id
                    AND waiting.wait_mode = 'foreground'
                   JOIN session_child_result_delivery AS delivery
                     ON delivery.awaiting_tool_request_id =
                        waiting.awaiting_tool_request_id
                    AND delivery.spawning_tool_request_id =
                        waiting.spawning_tool_request_id
                    AND delivery.parent_session_id = waiting.parent_session_id
                    AND delivery.delivery_sequence IS NULL
                   JOIN tool_attempt AS attempt
                     ON attempt.request_id = waiting.awaiting_tool_request_id
                    AND attempt.session_id = lifecycle.session_id
                    AND attempt.turn_id = lifecycle.turn_id
                    AND attempt.state_kind = 'terminal'
                    AND attempt.terminal_disposition_kind = 'awaiting_child'
                    AND attempt.wait_spawning_request_id =
                        waiting.spawning_tool_request_id
                    AND attempt.wait_child_session_id = waiting.child_session_id
                  WHERE lifecycle.session_id = $1
                    AND lifecycle.turn_id = $2
                    AND lifecycle.state_kind = 'active'
                    AND NOT lifecycle.delegation_runtime_terminal
                    AND lifecycle.active_phase_kind = 'awaiting_child'",
            )
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(predecessor) = predecessor else {
                return Ok(false);
            };
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id,
                     continued_from_attempt_id, state_kind)
                 VALUES ($1, $2, $3, $4, 'prepared')",
            )
            .bind(continuation.into_uuid())
            .bind(turn_id_to_uuid(turn))
            .bind(session_id_to_uuid(session))
            .bind(predecessor)
            .execute(&mut *transaction)
            .await?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'running',
                        current_attempt_id = $1,
                        child_wait_request_id = NULL
                  WHERE session_id = $2
                    AND turn_id = $3
                    AND state_kind = 'active'
                    AND NOT delegation_runtime_terminal
                    AND active_phase_kind = 'awaiting_child'
                    AND current_attempt_id IS NULL",
            )
            .bind(continuation.into_uuid())
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            require_single(rows, "foreground child-wait continuation")?;
            Ok(true)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Loads one recorded decision handling, or `None` only for an unseen
    /// identifier.
    pub async fn load_recorded_decision(
        &self,
        command_id: signalbox_domain::DurableCommandId,
    ) -> Result<Option<PreparedDecideToolRequest>, ToolLoopRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::DecideToolRequest) => {
                let receipt = load_decision_receipt(&mut connection, command_id)
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("decision command receipt"))?;
                Ok(Some(receipt))
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle
                | CommandKind::OverrideDeniedToolRequest,
            ) => Err(ToolLoopRepositoryError::DifferentCommandKind),
        }
    }

    /// Atomically records one replay-idempotent user decision and successor
    /// phase. A fresh continuation attempt is supplied only for the final
    /// undecided request.
    pub async fn decide<NextAttempt>(
        &self,
        command: DecideToolRequest,
        mut next_attempt: NextAttempt,
    ) -> Result<PreparedDecideToolRequest, ToolLoopRepositoryError>
    where
        NextAttempt: FnMut() -> signalbox_domain::TurnAttemptId,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if let Some(kind) = inspect_registry(&mut transaction, command.command_id()).await? {
                if kind != CommandKind::DecideToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_decision_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("decision command receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::ConflictingCommandReuse);
                }
                return Ok(receipt);
            }
            let issuer = crate::command_registry::issuer_columns(
                signalbox_domain::CommandPrincipal::Operator,
            );
            let claimed = sqlx::query(
                "INSERT INTO durable_command
                    (command_id, command_kind, storage_version, claimed_at,
                     issuer_kind, issuer_module)
                 VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(durable_command_id_to_uuid(command.command_id()))
            .bind(DECIDE_TOOL_REQUEST_KIND)
            .bind(STORAGE_VERSION)
            .bind(issuer.0)
            .bind(issuer.1)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
                == 1;
            if !claimed {
                let kind = inspect_registry(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner command claim"))?;
                if kind != CommandKind::DecideToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_decision_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner decision receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::ConflictingCommandReuse);
                }
                return Ok(receipt);
            }

            let ownership = sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT session_id, turn_id
                   FROM tool_request
                  WHERE request_id = $1",
            )
            .bind(tool_request_id_to_uuid(command.request()))
            .fetch_optional(&mut *transaction)
            .await?;
            let prepared = match ownership {
                None => command.prepare_request_not_found(),
                Some((stored_session, stored_turn)) => {
                    let session = session_id_from_uuid(stored_session);
                    let turn = turn_id_from_uuid(stored_turn);
                    lock_tool_session(&mut transaction, session).await?;
                    if decision_exists(&mut transaction, command.request()).await?
                        || request_closed_by_turn_end(&mut transaction, command.request()).await?
                    {
                        let prepared = command.prepare_already_resolved();
                        persist_decision_command(
                            &mut transaction,
                            &prepared,
                            signalbox_domain::CommandPrincipal::Operator,
                        )
                        .await?;
                        settle_decision_injection(&mut transaction, session, turn, &prepared)
                            .await?;
                        return Ok(prepared);
                    }
                    let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                        .await?
                        .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
                    let continuation_attempt = batch
                        .awaiting_approval()
                        .filter(|waiting| waiting.request() == command.request())
                        .filter(|_| {
                            batch
                                .requests()
                                .iter()
                                .filter(|request| batch.approval(request.id()).is_none())
                                .count()
                                == 1
                        })
                        .map(|_| next_attempt());
                    let decision = batch
                        .prepare_user_decision(command, continuation_attempt)
                        .map_err(|_| {
                            ToolLoopRepositoryError::InvalidTransition(
                                "user decision does not match active batch",
                            )
                        })?;
                    persist_batch_decision(&mut transaction, &decision).await?;
                    settle_decision_injection(
                        &mut transaction,
                        session,
                        turn,
                        decision.prepared_command(),
                    )
                    .await?;
                    return Ok(decision.prepared_command().clone());
                }
            };
            persist_decision_command(
                &mut transaction,
                &prepared,
                signalbox_domain::CommandPrincipal::Operator,
            )
            .await?;
            Ok(prepared)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Loads the recorded receipt for one override command without mutating
    /// any state.
    pub async fn load_recorded_override(
        &self,
        command_id: signalbox_domain::DurableCommandId,
    ) -> Result<Option<PreparedOverrideDeniedToolRequest>, ToolLoopRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::OverrideDeniedToolRequest) => {
                let receipt = load_override_receipt(&mut connection, command_id)
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("override command receipt"))?;
                Ok(Some(receipt))
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Err(ToolLoopRepositoryError::DifferentCommandKind),
        }
    }

    /// Atomically records one replay-idempotent user override of a delegate
    /// denial.
    ///
    /// The transaction claims the command, locks the denied request's owning
    /// session, evaluates the domain verification predicate against durable
    /// evidence, and records the receipt; an applied command additionally
    /// inserts the single recorded override row the next matching proposal may
    /// consume.
    pub async fn override_denied(
        &self,
        command: OverrideDeniedToolRequest,
    ) -> Result<PreparedOverrideDeniedToolRequest, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if let Some(kind) = inspect_registry(&mut transaction, command.command_id()).await? {
                if kind != CommandKind::OverrideDeniedToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_override_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("override command receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::ConflictingCommandReuse);
                }
                return Ok(receipt);
            }
            let issuer = crate::command_registry::issuer_columns(
                signalbox_domain::CommandPrincipal::Operator,
            );
            let claimed = sqlx::query(
                "INSERT INTO durable_command
                    (command_id, command_kind, storage_version, claimed_at,
                     issuer_kind, issuer_module)
                 VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(durable_command_id_to_uuid(command.command_id()))
            .bind(OVERRIDE_DENIED_TOOL_REQUEST_KIND)
            .bind(STORAGE_VERSION)
            .bind(issuer.0)
            .bind(issuer.1)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
                == 1;
            if !claimed {
                let kind = inspect_registry(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner command claim"))?;
                if kind != CommandKind::OverrideDeniedToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_override_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner override receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::ConflictingCommandReuse);
                }
                return Ok(receipt);
            }

            let request_record =
                load_request_by_id(&mut transaction, command.denied_request()).await?;
            let prepared = match request_record {
                None => command.prepare_request_not_found(),
                Some(request_record) => {
                    lock_tool_session(&mut transaction, request_record.session()).await?;
                    let approvals =
                        load_approvals_by_request(&mut transaction, &[command.denied_request()])
                            .await?;
                    let terminal_resolution = load_terminal_request_resolution(
                        &mut transaction,
                        command.denied_request(),
                    )
                    .await?;
                    let existing_override_command =
                        load_existing_override_command(&mut transaction, command.denied_request())
                            .await?;
                    let denied_request = command.denied_request();
                    let prepared = command
                        .prepare(
                            &request_record,
                            approvals.get(&denied_request),
                            terminal_resolution,
                            existing_override_command,
                        )
                        .map_err(|_| {
                            ToolLoopRepositoryError::InvalidTransition(
                                "override evidence does not match the denied request",
                            )
                        })?;
                    if let OverrideDeniedToolRequestResult::Applied(applied) = prepared.result() {
                        let recorded = applied.recorded();
                        sqlx::query(
                            "INSERT INTO tool_approval_user_override
                                (denied_request_id, session_id, command_id,
                                 judge_model_call_id)
                             VALUES ($1, $2, $3, $4)",
                        )
                        .bind(tool_request_id_to_uuid(recorded.denied_request()))
                        .bind(session_id_to_uuid(recorded.session()))
                        .bind(durable_command_id_to_uuid(recorded.command()))
                        .bind(recorded.judge_call().into_uuid())
                        .execute(&mut *transaction)
                        .await?;
                    }
                    prepared
                }
            };
            persist_override_command(&mut transaction, &prepared).await?;
            Ok(prepared)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically prepares the next proposal-order approved attempt.
    pub async fn prepare_next_attempt(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<Option<CurrentToolAttempt>, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let Some(batch) =
                load_active_batch_from_connection(&mut transaction, session, turn).await?
            else {
                return Ok(None);
            };
            let prepared = batch
                .prepare_next_attempt(attempt, effect_class)
                .map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "batch has no next serialized attempt",
                    )
                })?
                .into_attempt();
            insert_prepared_attempt(&mut transaction, &prepared).await?;
            Ok(Some(prepared))
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically authorizes one exact prepared attempt, returning the fence
    /// that must accompany executor evidence.
    pub async fn authorize_attempt(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolLoopRepositoryError> {
        match self
            .authorize_attempt_with_preauthorization(
                session,
                turn,
                attempt,
                ToolPreauthorization::Unmetered,
            )
            .await?
        {
            ToolAttemptAuthorizationOutcome::Authorized(authorized) => Ok(*authorized),
            ToolAttemptAuthorizationOutcome::PreauthorizationRejected { .. } => {
                Err(ToolLoopCorruption::Inconsistent("unmetered tool preauthorization").into())
            }
        }
    }

    /// Atomically charges typed resources and authorizes one prepared attempt.
    pub async fn authorize_attempt_with_preauthorization(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        preauthorization: ToolPreauthorization,
    ) -> Result<ToolAttemptAuthorizationOutcome, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            let authorized = batch.authorize_dispatch(attempt).map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition("tool attempt is not prepared")
            })?;
            let admission = admit_tool_preauthorization(
                &mut transaction,
                session,
                turn,
                authorized.attempt().request(),
                preauthorization,
            )
            .await?;
            if let Some(detail) = admission.into_detail()? {
                return Ok(ToolAttemptAuthorizationOutcome::PreauthorizationRejected { detail });
            }
            mark_issuing_turn_attempt_running(&mut transaction, authorized.attempt()).await?;
            let rows = sqlx::query(
                "UPDATE tool_attempt
                    SET state_kind = 'in_flight'
                  WHERE attempt_id = $1
                    AND request_id = $2
                    AND session_id = $3
                    AND turn_id = $4
                    AND issuing_turn_attempt_id = $5
                    AND dispatch_generation = $6
                    AND state_kind = 'prepared'",
            )
            .bind(tool_attempt_id_to_uuid(authorized.attempt().attempt()))
            .bind(tool_request_id_to_uuid(authorized.attempt().request()))
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .bind(authorized.attempt().issuing_attempt().into_uuid())
            .bind(Decimal::from(authorized.attempt().generation().as_u64()))
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            require_single(rows, "tool attempt authorization")?;
            Ok(ToolAttemptAuthorizationOutcome::Authorized(Box::new(
                authorized,
            )))
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Rereads exact dispatch authority after an ambiguous authorization
    /// commit acknowledgement.
    pub async fn reread_ambiguous_authorization(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, session).await?;
        let batch = load_active_batch_from_connection(&mut transaction, session, turn)
            .await?
            .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
        let current = batch
            .requests()
            .iter()
            .find_map(|request| match batch.attempt(request.id()) {
                Some(ReconstitutedToolAttempt::Current(current))
                    if current.attempt() == attempt =>
                {
                    Some(current.clone())
                }
                Some(ReconstitutedToolAttempt::Current(_))
                | Some(ReconstitutedToolAttempt::Ended(_))
                | None => None,
            })
            .ok_or(ToolLoopCorruption::Missing("authorized tool attempt"))?;
        let status = match current.state() {
            CurrentToolAttemptState::Prepared => ToolAttemptAuthorizationStatus::Prepared(current),
            CurrentToolAttemptState::InFlight => ToolAttemptAuthorizationStatus::InFlight(
                batch.resume_in_flight_dispatch(attempt).map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "in-flight authorization could not restore its fence",
                    )
                })?,
            ),
        };
        transaction.rollback().await?;
        Ok(status)
    }

    /// Atomically applies exact executor evidence through the returned fence.
    pub async fn commit_observation(
        &self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, ToolLoopRepositoryError> {
        let correlation = *observation.correlation();
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, correlation.session()).await?;
            let current = load_current_attempt(&mut transaction, correlation.attempt())
                .await?
                .ok_or(ToolLoopCorruption::Missing("in-flight tool attempt"))?;
            let ended = current
                .apply_terminal_observation(observation)
                .map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "executor evidence does not match current fence",
                    )
                })?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            if ended.end() == &ToolAttemptEnd::Ambiguous {
                persist_tool_recovery_wait(&mut transaction, &ended, false).await?;
            }
            Ok(ended)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Rereads whether one unchanged executor observation committed.
    pub async fn reread_observation(
        &self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, ToolLoopRepositoryError> {
        let correlation = observation.correlation();
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, correlation.session()).await?;
        let mut attempts = load_attempts_by_id(&mut transaction, &[correlation.attempt()]).await?;
        let attempt = attempts
            .remove(&correlation.attempt())
            .ok_or(ToolLoopCorruption::Missing("retained tool attempt"))?;
        let status = match attempt {
            ReconstitutedToolAttempt::Current(current)
                if current.state() == CurrentToolAttemptState::InFlight
                    && current.session() == correlation.session()
                    && current.turn() == correlation.turn()
                    && current.issuing_attempt() == correlation.issuing_attempt()
                    && current.request() == correlation.request()
                    && current.generation() == correlation.generation() =>
            {
                RetainedToolAttemptObservationStatus::Pending
            }
            ReconstitutedToolAttempt::Ended(ended)
                if ended.session() == correlation.session()
                    && ended.turn() == correlation.turn()
                    && ended.issuing_attempt() == correlation.issuing_attempt()
                    && ended.request() == correlation.request()
                    && ended.generation() == correlation.generation()
                    && attempt_end_matches_observation(ended.end(), observation.observation()) =>
            {
                RetainedToolAttemptObservationStatus::AlreadyCommitted
            }
            ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => {
                return Err(ToolLoopCorruption::Inconsistent("retained tool observation").into());
            }
        };
        transaction.rollback().await?;
        Ok(status)
    }

    /// Authenticates an executor-reported foreground wait against the complete
    /// durable batch and exact ended dispatch fence.
    pub async fn reread_durable_child_wait(
        &self,
        evidence: CorrelatedDurableChildWait,
    ) -> Result<bool, ToolLoopRepositoryError> {
        let correlation = evidence.correlation();
        let wait = evidence.wait();
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, correlation.session()).await?;
        let Some(batch) = load_active_batch_from_connection(
            &mut transaction,
            correlation.session(),
            correlation.turn(),
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let phase_matches = batch.phase()
            == signalbox_domain::ToolBatchPhase::AwaitingChild {
                request: wait.awaiting_request(),
                spawning_request: wait.spawning_request(),
                child: wait.child(),
            };
        let attempt_matches = match batch.attempt(correlation.request()) {
            Some(ReconstitutedToolAttempt::Ended(ended)) => {
                ended.attempt() == correlation.attempt()
                    && ended.session() == correlation.session()
                    && ended.turn() == correlation.turn()
                    && ended.issuing_attempt() == correlation.issuing_attempt()
                    && ended.request() == correlation.request()
                    && ended.generation() == correlation.generation()
                    && ended.effect_class() == ToolEffectClass::EffectFree
                    && ended.end()
                        == &(ToolAttemptEnd::AwaitingChild {
                            spawning_request: wait.spawning_request(),
                            child: wait.child(),
                        })
            }
            None | Some(ReconstitutedToolAttempt::Current(_)) => false,
        };
        transaction.rollback().await?;
        Ok(phase_matches && attempt_matches)
    }

    /// Authenticates an executor-reported terminal transition against the
    /// complete durable batch and exact ended dispatch fence.
    pub async fn reread_durable_completion(
        &self,
        correlation: ToolAttemptDispatchCorrelation,
    ) -> Result<bool, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, correlation.session()).await?;
        let Some(batch) = load_active_batch_from_connection(
            &mut transaction,
            correlation.session(),
            correlation.turn(),
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let attempt_matches = match batch.attempt(correlation.request()) {
            Some(ReconstitutedToolAttempt::Ended(ended)) => {
                let terminal_matches = match ended.end() {
                    ToolAttemptEnd::Completed { .. } | ToolAttemptEnd::KnownFailed { .. } => true,
                    ToolAttemptEnd::AwaitingChild { .. } | ToolAttemptEnd::Ambiguous => false,
                };
                ended.attempt() == correlation.attempt()
                    && ended.session() == correlation.session()
                    && ended.turn() == correlation.turn()
                    && ended.issuing_attempt() == correlation.issuing_attempt()
                    && ended.request() == correlation.request()
                    && ended.generation() == correlation.generation()
                    && terminal_matches
            }
            Some(ReconstitutedToolAttempt::Current(_)) | None => false,
        };
        transaction.rollback().await?;
        Ok(attempt_matches)
    }

    /// Atomically records a lookup/schema error before any executor effect.
    pub async fn commit_preflight_error(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let current = load_current_attempt(&mut transaction, attempt)
                .await?
                .ok_or(ToolLoopCorruption::Missing("prepared tool attempt"))?;
            if current.session() != session || current.turn() != turn {
                return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
            }
            mark_issuing_turn_attempt_running(&mut transaction, &current).await?;
            let ended = current.end_preflight_error(error).map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition("invalid tool preflight result")
            })?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            Ok(ended)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Classifies one process-lost attempt and, for known loss, atomically
    /// closes the current turn with proof-bearing failure identities.
    pub async fn classify_crash_loss_and_close<NextTurn>(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> Result<signalbox_domain::ToolAttemptCrashOutcome, ToolLoopRepositoryError>
    where
        NextTurn: FnMut(signalbox_domain::AcceptedInputId) -> TurnId,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_delegated_turn_terminal_frontier(&mut transaction, session, turn)
                .await
                .map_err(map_model_call_error)?;
            load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            let current = load_current_attempt(&mut transaction, attempt)
                .await?
                .ok_or(ToolLoopCorruption::Missing("live tool attempt"))?;
            if current.session() != session || current.turn() != turn {
                return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
            }
            let outcome = current.classify_crash_loss();
            let ended = match &outcome {
                signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(ended)
                | signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(ended) => ended,
            };
            persist_ended_attempt(&mut transaction, ended).await?;
            match &outcome {
                signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(_) => {
                    persist_tool_recovery_wait(&mut transaction, ended, true).await?;
                }
                signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(_) => {
                    let closed_batch =
                        load_active_batch_from_connection(&mut transaction, session, turn)
                            .await?
                            .ok_or(ToolLoopCorruption::Missing("crash-closed tool batch"))?;
                    let projection = closed_batch
                        .prepare_failure_projection(
                            identities.result_entries().to_vec(),
                            identities.result_frontier(),
                        )
                        .map_err(|_| {
                            ToolLoopRepositoryError::InvalidTransition(
                                "known tool crash could not close its request batch",
                            )
                        })?;
                    persist_result_entries(&mut transaction, &projection).await?;
                    insert_snapshot(&mut transaction, projection.snapshot())
                        .await
                        .map_err(|_| ToolLoopCorruption::Inconsistent("crash closure frontier"))?;
                    crate::model_execution::fail_tool_crash_in_transaction(
                        &mut transaction,
                        session,
                        turn,
                        &projection,
                        identities.failure().clone(),
                        next_turn,
                    )
                    .await
                    .map_err(map_model_call_error)?;
                }
            }
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically commits result projection, steering consumption, and the
    /// next prepared model call for the same logical turn.
    pub async fn commit_result_and_prepare_continuation(
        &self,
        producing_call: signalbox_domain::ModelCallId,
        projection: &PreparedToolResultProjection,
        prepared: &signalbox_domain::PreparedInitialModelCall,
        credential_reference: &ModelCallCredentialReference,
    ) -> Result<(), ToolLoopRepositoryError> {
        let session = prepared.session();
        let turn = prepared.turn();
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            if batch.producing_call() != producing_call
                || batch.yielded_snapshot().frontier().owning_session() != session
                || !matches!(
                    batch.phase(),
                    signalbox_domain::ToolBatchPhase::Executing { turn_attempt }
                        if turn_attempt == prepared.attempt()
                )
            {
                return Err(ToolLoopCorruption::Inconsistent("continuation batch").into());
            }
            let projection_frontier = projection.snapshot().frontier();
            let call_frontier = prepared.call().frontier();
            let frontier_matches = match prepared.steering_snapshot() {
                Some(steering_snapshot) => {
                    projection
                        .snapshot()
                        .is_semantic_prefix_of(steering_snapshot)
                        && call_frontier == steering_snapshot.frontier()
                }
                None => call_frontier == projection_frontier,
            };
            if projection_frontier.owning_session() != session || !frontier_matches {
                return Err(ToolLoopCorruption::Inconsistent("continuation call frontier").into());
            }

            persist_result_entries(&mut transaction, projection).await?;
            insert_snapshot(&mut transaction, projection.snapshot())
                .await
                .map_err(|_| ToolLoopCorruption::Inconsistent("result frontier"))?;
            outbox::append(
                &mut transaction,
                OutboxEvent::ToolBatchTransition {
                    session,
                    turn,
                    producing_call,
                    state: ToolBatchOutboxState::ResultsProjected(
                        projection.snapshot().frontier().snapshot(),
                    ),
                },
            )
            .await?;
            insert_prepared_call(
                &mut transaction,
                prepared,
                credential_reference,
                None,
                self.cache_inclusive_input_targets
                    .contains(&prepared.call().target()),
            )
            .await
            .map_err(map_model_call_error)?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_tool_round_call_id = NULL,
                        approval_tool_request_id = NULL,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $1
                    AND session_id = $2
                    AND current_attempt_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND active_tool_round_call_id = $4",
            )
            .bind(turn_id_to_uuid(turn))
            .bind(session_id_to_uuid(session))
            .bind(prepared.attempt().into_uuid())
            .bind(producing_call.into_uuid())
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            require_single(rows, "tool result continuation call")?;
            Ok(())
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically derives and commits result projection, consumes all pending
    /// steering, and prepares the next same-turn model call.
    pub async fn prepare_continuation<NextSteering>(
        &self,
        session: SessionId,
        turn: TurnId,
        producing_call: signalbox_domain::ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, ToolLoopRepositoryError>
    where
        NextSteering: FnMut(
            signalbox_domain::AcceptedInputId,
        ) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
    {
        let targets = self.continuation_targets.as_ref().ok_or(
            ToolLoopRepositoryError::InvalidTransition(
                "tool continuation model targets are not configured",
            ),
        )?;
        let credential_reference = self.continuation_credential.as_ref().ok_or(
            ToolLoopRepositoryError::InvalidTransition(
                "tool continuation credential reference is not configured",
            ),
        )?;
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_delegated_child_endpoint_sessions(&mut transaction, session)
                .await
                .map_err(map_model_call_error)?;
            lock_tool_session(&mut transaction, session).await?;
            let Some(batch) =
                load_active_batch_from_connection(&mut transaction, session, turn).await?
            else {
                return Ok(PrepareToolContinuationOutcome::NoWork);
            };
            let turn_attempt = match batch.phase() {
                signalbox_domain::ToolBatchPhase::Executing { turn_attempt }
                    if batch.producing_call() == producing_call =>
                {
                    turn_attempt
                }
                _ => return Ok(PrepareToolContinuationOutcome::NoWork),
            };
            let child_wait =
                batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(attempt)) => match attempt.end() {
                            ToolAttemptEnd::AwaitingChild {
                                spawning_request,
                                child,
                            } => Some((request.id(), *spawning_request, *child)),
                            ToolAttemptEnd::Completed { .. }
                            | ToolAttemptEnd::KnownFailed { .. }
                            | ToolAttemptEnd::Ambiguous => None,
                        },
                        Some(ReconstitutedToolAttempt::Current(_)) | None => None,
                    });
            let projection = match child_wait {
                Some((awaiting_request, spawning_request, child)) => batch
                    .prepare_delegation_result_projection(
                        identities.result_entries().to_vec(),
                        identities.result_frontier(),
                        load_foreground_delegation_outcome(
                            &mut transaction,
                            session,
                            awaiting_request,
                            spawning_request,
                            child,
                        )
                        .await?,
                    ),
                None => batch.prepare_result_projection(
                    identities.result_entries().to_vec(),
                    identities.result_frontier(),
                ),
            }
            .map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition(
                    "tool batch is not ready for continuation",
                )
            })?;
            persist_result_entries(&mut transaction, &projection).await?;
            insert_snapshot(&mut transaction, projection.snapshot())
                .await
                .map_err(|_| ToolLoopCorruption::Inconsistent("result frontier"))?;
            // Full frontier reconstruction can scan a long-lived session. Keep
            // that read outside the global writer guard while the session lock
            // preserves the transaction-local result projection unchanged.
            let execution = crate::model_execution::load_tool_continuation_execution(
                &mut transaction,
                session,
                targets,
                &projection,
            )
            .await
            .map_err(map_model_call_error)?;
            let outbox_order_guard =
                crate::model_execution::acquire_model_call_outbox_order_guard(&mut transaction)
                    .await
                    .map_err(map_model_call_error)?;
            outbox::append(
                &mut transaction,
                OutboxEvent::ToolBatchTransition {
                    session,
                    turn,
                    producing_call,
                    state: ToolBatchOutboxState::ResultsProjected(
                        projection.snapshot().frontier().snapshot(),
                    ),
                },
            )
            .await?;
            let outcome = crate::model_execution::prepare_tool_continuation_call(
                &mut transaction,
                outbox_order_guard,
                execution,
                session,
                turn,
                targets,
                credential_reference,
                self.credential_families.as_ref(),
                &self.credential_pools,
                &self.cache_inclusive_input_targets,
                &self.continuation_usage_limits,
                &projection,
                producing_call,
                identities.call(),
                identities.target_failure().clone(),
                identities.steering_frontier(),
                next_steering,
            )
            .await
            .map_err(map_model_call_error)?;
            if matches!(outcome, PrepareToolContinuationOutcome::Checkpointed(_)) {
                let rows = sqlx::query(
                    "UPDATE turn_lifecycle
                        SET active_tool_round_call_id = NULL,
                            approval_tool_request_id = NULL,
                            recovery_tool_attempt_id = NULL
                      WHERE turn_id = $1
                        AND session_id = $2
                        AND current_attempt_id = $3
                        AND state_kind = 'active'
                        AND active_phase_kind = 'running'
                        AND active_tool_round_call_id = $4",
                )
                .bind(turn_id_to_uuid(turn))
                .bind(session_id_to_uuid(session))
                .bind(turn_attempt.into_uuid())
                .bind(producing_call.into_uuid())
                .execute(&mut *transaction)
                .await?
                .rows_affected();
                require_single(rows, "tool continuation call boundary")?;
            }
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }
}

impl DecideToolRequestTransaction for PostgresToolLoopRepository {
    type Error = ToolLoopRepositoryError;

    async fn decide<NextAttempt>(
        &mut self,
        command: DecideToolRequest,
        next_attempt: NextAttempt,
    ) -> Result<PreparedDecideToolRequest, Self::Error>
    where
        NextAttempt: FnMut() -> signalbox_domain::TurnAttemptId + Send,
    {
        PostgresToolLoopRepository::decide(self, command, next_attempt).await
    }
}

impl OverrideDeniedToolRequestTransaction for PostgresToolLoopRepository {
    type Error = ToolLoopRepositoryError;

    async fn override_denied(
        &mut self,
        command: OverrideDeniedToolRequest,
    ) -> Result<PreparedOverrideDeniedToolRequest, Self::Error> {
        PostgresToolLoopRepository::override_denied(self, command).await
    }
}

impl ToolExecutionTransaction for PostgresToolLoopRepository {
    type Error = ToolLoopRepositoryError;

    async fn load_active_batch(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<ToolBatch>, Self::Error> {
        PostgresToolLoopRepository::load_active_batch(self, session, turn).await
    }

    async fn resume_child_wait(
        &mut self,
        session: SessionId,
        turn: TurnId,
        continuation: signalbox_domain::TurnAttemptId,
    ) -> Result<bool, Self::Error> {
        PostgresToolLoopRepository::resume_child_wait(self, session, turn, continuation).await
    }

    async fn prepare_next_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<Option<CurrentToolAttempt>, Self::Error> {
        PostgresToolLoopRepository::prepare_next_attempt(self, session, turn, attempt, effect_class)
            .await
    }

    async fn authorize_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        preauthorization: ToolPreauthorization,
    ) -> Result<ToolAttemptAuthorizationOutcome, Self::Error> {
        PostgresToolLoopRepository::authorize_attempt_with_preauthorization(
            self,
            session,
            turn,
            attempt,
            preauthorization,
        )
        .await
    }

    async fn reread_ambiguous_authorization(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
        PostgresToolLoopRepository::reread_ambiguous_authorization(self, session, turn, attempt)
            .await
    }

    async fn commit_preflight_error(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, Self::Error> {
        PostgresToolLoopRepository::commit_preflight_error(self, session, turn, attempt, error)
            .await
    }

    async fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, Self::Error> {
        PostgresToolLoopRepository::commit_observation(self, observation).await
    }

    async fn reread_observation(
        &mut self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
        PostgresToolLoopRepository::reread_observation(self, observation).await
    }

    async fn reread_durable_completion(
        &mut self,
        correlation: ToolAttemptDispatchCorrelation,
    ) -> Result<bool, Self::Error> {
        PostgresToolLoopRepository::reread_durable_completion(self, correlation).await
    }

    async fn reread_durable_child_wait(
        &mut self,
        wait: CorrelatedDurableChildWait,
    ) -> Result<bool, Self::Error> {
        PostgresToolLoopRepository::reread_durable_child_wait(self, wait).await
    }

    async fn classify_crash_loss<NextTurn>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> Result<signalbox_domain::ToolAttemptCrashOutcome, Self::Error>
    where
        NextTurn: FnMut(signalbox_domain::AcceptedInputId) -> TurnId + Send,
    {
        PostgresToolLoopRepository::classify_crash_loss_and_close(
            self, session, turn, attempt, identities, next_turn,
        )
        .await
    }

    async fn prepare_continuation<NextSteering>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        producing_call: signalbox_domain::ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, Self::Error>
    where
        NextSteering: FnMut(
                signalbox_domain::AcceptedInputId,
            ) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId)
            + Send,
    {
        PostgresToolLoopRepository::prepare_continuation(
            self,
            session,
            turn,
            producing_call,
            identities,
            next_steering,
        )
        .await
    }
}

fn map_model_call_error(
    error: crate::model_execution::ModelCallRepositoryError,
) -> ToolLoopRepositoryError {
    match error {
        crate::model_execution::ModelCallRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous,
        },
        crate::model_execution::ModelCallRepositoryError::IdentityCollision(_) => {
            ToolLoopRepositoryError::IdentityCollision
        }
        crate::model_execution::ModelCallRepositoryError::Corruption(_)
        | crate::model_execution::ModelCallRepositoryError::NoLiveExecution
        | crate::model_execution::ModelCallRepositoryError::InvalidTransition(_) => {
            ToolLoopCorruption::Inconsistent("continuation model call").into()
        }
    }
}

pub(crate) async fn load_active_batch_from_connection(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
    let lifecycle = sqlx::query(
        "SELECT active_phase_kind, current_attempt_id,
                active_tool_round_call_id, approval_tool_request_id,
                recovery_tool_attempt_id, child_wait_request_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'active'
            AND active_tool_round_call_id IS NOT NULL
            AND goal_turn_is_runtime_relevant(session_id, turn_id)",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(lifecycle) = lifecycle else {
        return Ok(None);
    };
    let producing_call = signalbox_domain::ModelCallId::from_uuid(required(
        &lifecycle,
        "active_tool_round_call_id",
    )?);
    let round = sqlx::query(
        "SELECT boundary_kind, boundary_frontier_id
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool round"))?;
    let boundary_kind: String = required(&round, "boundary_kind")?;
    if boundary_kind != "continuing" {
        return Err(ToolLoopCorruption::Inconsistent("active round boundary").into());
    }
    let frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&round, "boundary_frontier_id")?);
    let yielded_snapshot = load_snapshot(connection, session, frontier).await?;
    let requests = load_requests(connection, producing_call, session, turn).await?;
    let approvals = load_approvals(connection, producing_call).await?;
    let attempts = load_attempts(connection, producing_call).await?;
    let retired_attempts = load_retired_attempts(connection, producing_call).await?;
    let runner_authorized_attempts =
        load_runner_authorized_attempts(connection, producing_call).await?;
    let phase_kind: String = required(&lifecycle, "active_phase_kind")?;
    let phase = match phase_kind.as_str() {
        "awaiting_tool_approval" => ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: tool_request_id_from_uuid(required(&lifecycle, "approval_tool_request_id")?),
        },
        "running" => ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: signalbox_domain::TurnAttemptId::from_uuid(required(
                &lifecycle,
                "current_attempt_id",
            )?),
        },
        "awaiting_tool_recovery" => ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id_from_uuid(required(&lifecycle, "recovery_tool_attempt_id")?),
        },
        "awaiting_child" => {
            let request = tool_request_id_from_uuid(required(&lifecycle, "child_wait_request_id")?);
            let wait = sqlx::query(
                "SELECT spawning_tool_request_id, child_session_id
                   FROM session_delegation_wait
                  WHERE awaiting_tool_request_id = $1
                    AND parent_session_id = $2
                    AND parent_turn_id = $3
                    AND wait_mode = 'foreground'",
            )
            .bind(tool_request_id_to_uuid(request))
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(ToolLoopCorruption::Missing("foreground child wait"))?;
            ToolBatchPhaseReconstitutionInput::AwaitingChild {
                request,
                spawning_request: tool_request_id_from_uuid(required(
                    &wait,
                    "spawning_tool_request_id",
                )?),
                child: session_id_from_uuid(required(&wait, "child_session_id")?),
            }
        }
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "active_phase_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        yielded_snapshot,
        requests,
        approvals,
        attempts,
        phase,
    )
    .with_retired_attempts(retired_attempts)
    .with_runner_authorized_attempts(runner_authorized_attempts)
    .reconstitute()
    .map(Some)
    .map_err(|error| ToolLoopCorruption::Batch(error.failure()).into())
}

pub(crate) async fn deny_awaiting_approvals_for_interrupt<NextDecision, NextContinuation>(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    next_decision: &mut NextDecision,
    next_continuation: &mut NextContinuation,
) -> Result<(), ToolLoopRepositoryError>
where
    NextDecision: FnMut() -> DurableCommandId,
    NextContinuation: FnMut() -> signalbox_domain::TurnAttemptId,
{
    loop {
        let Some(batch) = load_active_batch_from_connection(connection, session, turn).await?
        else {
            return Ok(());
        };
        let Some(waiting) = batch.awaiting_approval() else {
            return Ok(());
        };
        let request = waiting.request();
        let last_undecided = batch
            .requests()
            .iter()
            .filter(|request| batch.approval(request.id()).is_none())
            .count()
            == 1;
        let continuation = last_undecided.then(&mut *next_continuation);
        let command = DecideToolRequest::try_new(
            next_decision(),
            request,
            ToolApprovalDecision::Deny { reason: None },
        )
        .map_err(|_| {
            ToolLoopRepositoryError::InvalidTransition("closure denial command identity is invalid")
        })?;
        let decision = batch
            .prepare_lifecycle_closure_denial(command, continuation)
            .map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition(
                    "closure denial does not match the approval wait",
                )
            })?;
        sqlx::query(
            "UPDATE tool_approval_judge_model_call
                SET state_kind = $1, terminal_disposition_kind = $2
              WHERE request_id = $3
                AND (state_kind = $4 OR state_kind = $5)",
        )
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Terminal,
        ))
        .bind(approval_judge_terminal_disposition_to_str(
            ApprovalJudgeTerminalDispositionStorageKind::Failed(
                FailedApprovalJudgeDisposition::Cancelled,
            ),
        ))
        .bind(tool_request_id_to_uuid(request))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Prepared,
        ))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::InFlight,
        ))
        .execute(&mut *connection)
        .await?;
        persist_batch_decision(connection, &decision).await?;
    }
}

/// Loads the exact frontier from which a runner-recovery interrupt must
/// continue. A recovery wait retaining a tool round uses that round's yielded
/// boundary; a wait without one uses the turn's starting frontier.
pub(crate) async fn load_runner_recovery_source_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<ResolvedContextFrontierSnapshot>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT lifecycle.active_tool_round_call_id,
                lifecycle.starting_frontier_id,
                round.boundary_kind,
                round.boundary_frontier_id
           FROM turn_lifecycle AS lifecycle
           LEFT JOIN tool_round AS round
             ON round.producing_model_call_id =
                    lifecycle.active_tool_round_call_id
            AND round.turn_id = lifecycle.turn_id
            AND round.session_id = lifecycle.session_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'
            AND goal_turn_is_runtime_relevant(lifecycle.session_id,
                                              lifecycle.turn_id)",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let active_tool_round: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
    let frontier = if active_tool_round.is_some() {
        let boundary_kind: String = required(&row, "boundary_kind")?;
        if boundary_kind != "continuing" {
            return Err(
                ToolLoopCorruption::Inconsistent("runner recovery tool round boundary").into(),
            );
        }
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "boundary_frontier_id")?)
    } else {
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "starting_frontier_id")?)
    };
    load_snapshot(connection, session, frontier).await.map(Some)
}

pub(crate) async fn load_runner_recovery_batch_without_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    yielded_attempt: signalbox_domain::TurnAttemptId,
) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
    load_runner_recovery_cancellation_batch(connection, session, turn, yielded_attempt, None).await
}

pub(crate) async fn load_runner_recovery_cancellation_batch(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    yielded_attempt: signalbox_domain::TurnAttemptId,
    interrupted_attempt: Option<ToolAttemptId>,
) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
    let producing_call = sqlx::query_scalar::<_, Uuid>(
        "SELECT active_tool_round_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'active'
            AND active_phase_kind = 'awaiting_runner_recovery'
            AND runner_recovery_tool_attempt_id IS NOT DISTINCT FROM $3
            AND active_tool_round_call_id IS NOT NULL",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(interrupted_attempt.map(tool_attempt_id_to_uuid))
    .fetch_optional(&mut *connection)
    .await?
    .map(signalbox_domain::ModelCallId::from_uuid);
    let Some(producing_call) = producing_call else {
        return Ok(None);
    };
    let round = sqlx::query(
        "SELECT boundary_kind, boundary_frontier_id
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("runner recovery tool round"))?;
    if required::<String>(&round, "boundary_kind")? != "continuing" {
        return Err(ToolLoopCorruption::Inconsistent("runner recovery tool round boundary").into());
    }
    let frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&round, "boundary_frontier_id")?);
    let mut retired_attempts = load_retired_attempts(connection, producing_call).await?;
    retired_attempts.retain(|attempt| Some(*attempt) != interrupted_attempt);
    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        load_snapshot(connection, session, frontier).await?,
        load_requests(connection, producing_call, session, turn).await?,
        load_approvals(connection, producing_call).await?,
        load_runner_recovery_attempts(connection, producing_call, interrupted_attempt).await?,
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: yielded_attempt,
        },
    )
    .with_retired_attempts(retired_attempts)
    .reconstitute()
    .map(Some)
    .map_err(|error| ToolLoopCorruption::Batch(error.failure()).into())
}

pub(crate) async fn load_recovery_batch_by_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    recovery_attempt: ToolAttemptId,
) -> Result<ToolBatch, ToolLoopRepositoryError> {
    let producing_call = sqlx::query_scalar::<_, Uuid>(
        "SELECT request.producing_model_call_id
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE attempt.attempt_id = $1
            AND attempt.session_id = $2
            AND attempt.turn_id = $3",
    )
    .bind(tool_attempt_id_to_uuid(recovery_attempt))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .map(signalbox_domain::ModelCallId::from_uuid)
    .ok_or(ToolLoopCorruption::Missing("tool recovery round"))?;
    let round = sqlx::query(
        "SELECT boundary_kind, boundary_frontier_id
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool recovery round"))?;
    if required::<String>(&round, "boundary_kind")? != "continuing" {
        return Err(ToolLoopCorruption::Inconsistent("tool recovery round boundary").into());
    }
    let frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&round, "boundary_frontier_id")?);
    let mut retired_attempts = load_retired_attempts(connection, producing_call).await?;
    retired_attempts.retain(|attempt| *attempt != recovery_attempt);
    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        load_snapshot(connection, session, frontier).await?,
        load_requests(connection, producing_call, session, turn).await?,
        load_approvals(connection, producing_call).await?,
        load_runner_recovery_attempts(connection, producing_call, Some(recovery_attempt)).await?,
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: recovery_attempt,
        },
    )
    .with_retired_attempts(retired_attempts)
    .reconstitute()
    .map_err(|error| ToolLoopCorruption::Batch(error.failure()).into())
}

/// Identifies the single continuing tool round whose request suffix exactly
/// fills the checked frontier before `trailing_member_count` trailing entries,
/// returning the boundary member position and request count that bound its
/// result window.
///
/// A `None` result names a frontier with no continuing tool round in that
/// window, so no tool results or denials back its trailing entries.
async fn load_tool_round_result_window(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    terminal_frontier: signalbox_domain::ContextFrontierId,
    trailing_member_count: Decimal,
) -> Result<Option<(Decimal, Decimal)>, ToolLoopRepositoryError> {
    let candidate_rounds = sqlx::query(
        "WITH candidate_round AS MATERIALIZED (
            SELECT round.producing_model_call_id,
                   round.session_id,
                   round.boundary_frontier_id,
                   boundary.member_count AS boundary_member_count,
                   round.request_count
              FROM tool_round AS round
              JOIN context_frontier AS boundary
                ON boundary.owning_session_id = round.session_id
               AND boundary.context_frontier_id = round.boundary_frontier_id
              JOIN context_frontier AS terminal
                ON terminal.owning_session_id = round.session_id
               AND terminal.context_frontier_id = $3
             WHERE round.session_id = $1
               AND round.turn_id = $2
               AND round.boundary_kind = 'continuing'
               AND terminal.member_count =
                       boundary.member_count + round.request_count + $4
         ), terminal_member AS MATERIALIZED (
            SELECT member_position,
                   source_session_id,
                   semantic_entry_id
              FROM context_frontier_member
             WHERE owning_session_id = $1
               AND context_frontier_id = $3
         )
         SELECT candidate.producing_model_call_id,
                candidate.boundary_member_count,
                candidate.request_count
           FROM candidate_round AS candidate
          WHERE NOT EXISTS (
                (SELECT member_position,
                        source_session_id,
                        semantic_entry_id
                   FROM context_frontier_member
                  WHERE owning_session_id = candidate.session_id
                    AND context_frontier_id = candidate.boundary_frontier_id)
                EXCEPT
                (SELECT member_position,
                        source_session_id,
                        semantic_entry_id
                   FROM terminal_member)
            )
          ORDER BY candidate.producing_model_call_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(terminal_frontier.into_uuid())
    .bind(trailing_member_count)
    .fetch_all(&mut *connection)
    .await?;
    let candidate = match candidate_rounds.as_slice() {
        [] => return Ok(None),
        [candidate] => candidate,
        [_, ..] => {
            return Err(ToolLoopCorruption::Inconsistent("terminal tool round identity").into());
        }
    };
    let boundary_member_count: Decimal = required(candidate, "boundary_member_count")?;
    let request_count: Decimal = required(candidate, "request_count")?;
    Ok(Some((boundary_member_count, request_count)))
}

pub(crate) async fn load_terminal_result_attempts(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    terminal_frontier: signalbox_domain::ContextFrontierId,
) -> Result<Vec<EndedToolAttempt>, ToolLoopRepositoryError> {
    let Some((boundary_member_count, request_count)) =
        load_tool_round_result_window(connection, session, turn, terminal_frontier, Decimal::ONE)
            .await?
    else {
        return Ok(Vec::new());
    };
    load_window_result_attempts(
        connection,
        session,
        terminal_frontier,
        boundary_member_count,
        request_count,
    )
    .await
}

async fn load_window_result_attempts(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
    boundary_member_count: Decimal,
    request_count: Decimal,
) -> Result<Vec<EndedToolAttempt>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT attempt.*
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           JOIN tool_attempt AS attempt
             ON attempt.attempt_id = entry.tool_result_attempt_id
          WHERE member.member_position > $3
            AND member.member_position <= $3 + $4
            AND entry.payload_kind = 'tool_execution_result'
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .bind(boundary_member_count)
    .bind(request_count)
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(decode_attempt)
        .map(|attempt| match attempt? {
            ReconstitutedToolAttempt::Ended(ended) => Ok(ended),
            ReconstitutedToolAttempt::Current(_) => {
                Err(ToolLoopCorruption::Inconsistent("terminal tool attempt state").into())
            }
        })
        .collect()
}

/// Loads the user-sourced denial resolution backing every `tool_denied` entry
/// in a terminal tool round's result suffix.
///
/// The reconstitution guard tightened by the terminal tool-round evidence work
/// requires each terminal `ToolDenied` result to name its exact durable
/// resolution, so a cancelled or failed turn whose terminal round denied a
/// request cannot reconstitute from the empty default.
pub(crate) async fn load_terminal_result_denials(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    terminal_frontier: signalbox_domain::ContextFrontierId,
) -> Result<Vec<signalbox_domain::ToolApprovalResolution>, ToolLoopRepositoryError> {
    let Some((boundary_member_count, request_count)) =
        load_tool_round_result_window(connection, session, turn, terminal_frontier, Decimal::ONE)
            .await?
    else {
        return Ok(Vec::new());
    };
    load_window_result_denials(
        connection,
        session,
        terminal_frontier,
        boundary_member_count,
        request_count,
    )
    .await
}

/// Loads the round result evidence backing one steering-consuming call
/// prepared at a tool-round continuation boundary: the terminal tool attempts
/// and user-sourced denial resolutions whose result entries fill the call's
/// frontier between the round boundary and the consumed steering suffix.
///
/// A `None` result names a call frontier with no continuing tool round in
/// that window — a call prepared against its turn's starting frontier.
pub(crate) async fn load_steering_continuation_round_evidence(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call_frontier: signalbox_domain::ContextFrontierId,
    consumed_count: u64,
) -> Result<
    Option<(
        Vec<EndedToolAttempt>,
        Vec<signalbox_domain::ToolApprovalResolution>,
    )>,
    ToolLoopRepositoryError,
> {
    let Some((boundary_member_count, request_count)) = load_tool_round_result_window(
        connection,
        session,
        turn,
        call_frontier,
        Decimal::from(consumed_count),
    )
    .await?
    else {
        return Ok(None);
    };
    let attempts = load_window_result_attempts(
        connection,
        session,
        call_frontier,
        boundary_member_count,
        request_count,
    )
    .await?;
    let denials = load_window_result_denials(
        connection,
        session,
        call_frontier,
        boundary_member_count,
        request_count,
    )
    .await?;
    Ok(Some((attempts, denials)))
}

/// Loads the round result evidence backing one steering-free continuation
/// call named by a terminal or recovery gate: the terminal tool attempts and
/// user-sourced denial resolutions whose result entries fill the call's
/// whole frontier after the round boundary, with no trailing suffix.
///
/// A `None` result names a call frontier with no continuing tool round in
/// that window — a call prepared against its turn's starting frontier.
pub(crate) async fn load_continuation_round_evidence(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call_frontier: signalbox_domain::ContextFrontierId,
) -> Result<
    Option<(
        Vec<EndedToolAttempt>,
        Vec<signalbox_domain::ToolApprovalResolution>,
    )>,
    ToolLoopRepositoryError,
> {
    let Some((boundary_member_count, request_count)) =
        load_tool_round_result_window(connection, session, turn, call_frontier, Decimal::ZERO)
            .await?
    else {
        return Ok(None);
    };
    let attempts = load_window_result_attempts(
        connection,
        session,
        call_frontier,
        boundary_member_count,
        request_count,
    )
    .await?;
    let denials = load_window_result_denials(
        connection,
        session,
        call_frontier,
        boundary_member_count,
        request_count,
    )
    .await?;
    Ok(Some((attempts, denials)))
}

async fn load_window_result_denials(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
    boundary_member_count: Decimal,
    request_count: Decimal,
) -> Result<Vec<signalbox_domain::ToolApprovalResolution>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind,
                approval.decision_source, approval.denial_reason,
                approval.user_command_id,
                approval.delegate_model_selection_id,
                approval.delegate_model_call_id, approval.rationale
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           JOIN tool_approval_decision AS approval
             ON approval.request_id = entry.tool_result_request_id
          WHERE member.member_position > $3
            AND member.member_position <= $3 + $4
            AND entry.payload_kind = 'tool_denied'
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .bind(boundary_member_count)
    .bind(request_count)
    .fetch_all(&mut *connection)
    .await?;
    decode_approvals(connection, rows).await
}

async fn load_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
) -> Result<ResolvedContextFrontierSnapshot, ToolLoopRepositoryError> {
    let declared: Decimal = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool round frontier"))?;
    let rows = sqlx::query_as::<_, (Decimal, Uuid, Uuid)>(
        "SELECT member_position, source_session_id, semantic_entry_id
           FROM resolve_context_frontier_members($1, $2)
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if declared
        != Decimal::from(
            u64::try_from(rows.len())
                .map_err(|_| ToolLoopCorruption::Inconsistent("frontier count"))?,
        )
    {
        return Err(ToolLoopCorruption::Inconsistent("frontier count").into());
    }
    let mut entries = Vec::with_capacity(rows.len());
    for (index, (position, source_session, entry)) in rows.into_iter().enumerate() {
        let expected = u64::try_from(index + 1)
            .map_err(|_| ToolLoopCorruption::Inconsistent("frontier position"))?;
        if position != Decimal::from(expected) {
            return Err(ToolLoopCorruption::Inconsistent("frontier position").into());
        }
        entries.push(signalbox_domain::SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(source_session),
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(entry),
        ));
    }
    ResolvedContextFrontierReconstitutionInput::new(session, frontier, entries)
        .reconstitute()
        .ok_or_else(|| ToolLoopCorruption::Inconsistent("frontier snapshot").into())
}

async fn load_requests(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text, approval_posture
           FROM tool_request
          WHERE producing_model_call_id = $1
          ORDER BY request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| decode_request(row, producing_call, session, turn))
        .collect()
}

pub(crate) fn decode_request(
    row: PgRow,
    producing_call: signalbox_domain::ModelCallId,
    session: SessionId,
    turn: TurnId,
) -> Result<signalbox_domain::ToolRequest, ToolLoopRepositoryError> {
    let ordinal: Decimal = required(&row, "request_ordinal")?;
    let ordinal =
        u32::try_from(ordinal).map_err(|_| ToolLoopCorruption::Inconsistent("request ordinal"))?;
    let name = ToolName::try_new(required(&row, "tool_name")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("tool name"))?;
    let arguments_kind = match required::<String>(&row, "arguments_kind")?.as_str() {
        "json" => ToolArgumentsKind::Json,
        "undecodable" => ToolArgumentsKind::Undecodable,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "arguments_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let arguments =
        NormalizedToolArguments::try_from_stored(arguments_kind, required(&row, "arguments_text")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("normalized arguments"))?;
    let stored_posture: String = required(&row, "approval_posture")?;
    let posture = match tool_approval_posture_from_str(&stored_posture) {
        Some(posture) => posture,
        None => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "approval_posture",
                value: stored_posture,
            }
            .into());
        }
    };
    Ok(ToolRequestReconstitutionInput::new(
        tool_request_id_from_uuid(required(&row, "request_id")?),
        session,
        turn,
        producing_call,
        ToolRequestOrdinal::from_u32(ordinal),
        name,
        arguments,
    )
    .with_approval_posture(posture)
    .into_request())
}

async fn load_approvals(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<signalbox_domain::ToolApprovalResolution>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind,
                approval.decision_source, approval.denial_reason,
                approval.user_command_id,
                approval.delegate_model_selection_id,
                approval.delegate_model_call_id, approval.rationale,
                approval.override_denied_request_id,
                recorded.command_id AS override_command_id
           FROM tool_approval_decision AS approval
           JOIN tool_request AS request
             ON request.request_id = approval.request_id
           LEFT JOIN tool_approval_user_override AS recorded
             ON recorded.denied_request_id = approval.override_denied_request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    decode_approvals(connection, rows).await
}

pub(crate) async fn decode_approvals(
    connection: &mut PgConnection,
    rows: Vec<PgRow>,
) -> Result<Vec<signalbox_domain::ToolApprovalResolution>, ToolLoopRepositoryError> {
    let user_commands = rows
        .iter()
        .map(|row| row.try_get::<Option<Uuid>, _>("user_command_id"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(|command| {
            durable_command_id_from_uuid(command).map_err(|_| {
                ToolLoopCorruption::Inconsistent("approval user command identity").into()
            })
        })
        .collect::<Result<Vec<_>, ToolLoopRepositoryError>>()?;
    let receipts = load_user_decision_receipts(connection, &user_commands).await?;
    let mut delegate_request_ids = Vec::new();
    for row in &rows {
        if matches!(
            tool_approval_decision_source_from_str(
                required::<String>(row, "decision_source")?.as_str()
            ),
            Some(
                ToolApprovalDecisionSourceStorageKind::Delegate
                    | ToolApprovalDecisionSourceStorageKind::UserOverride
            )
        ) {
            delegate_request_ids.push(tool_request_id_from_uuid(required(row, "request_id")?));
        }
    }
    let delegate_requests = load_requests_by_id(connection, &delegate_request_ids).await?;
    let mut approvals = Vec::with_capacity(rows.len());
    for row in rows {
        approvals.push(decode_approval(connection, row, &receipts, &delegate_requests).await?);
    }
    Ok(approvals)
}

async fn decode_approval(
    connection: &mut PgConnection,
    row: PgRow,
    user_receipts: &BTreeMap<DurableCommandId, PreparedDecideToolRequest>,
    delegate_requests: &BTreeMap<ToolRequestId, signalbox_domain::ToolRequest>,
) -> Result<signalbox_domain::ToolApprovalResolution, ToolLoopRepositoryError> {
    let request = tool_request_id_from_uuid(required(&row, "request_id")?);
    let reason: Option<String> = row.try_get("denial_reason")?;
    let decision = match required::<String>(&row, "decision_kind")?.as_str() {
        "approve" if reason.is_none() => ToolApprovalDecision::Approve,
        "deny" => ToolApprovalDecision::Deny {
            reason: reason
                .map(|value| {
                    ToolDenialReason::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("denial reason"))
                })
                .transpose()?,
        },
        "approve" => {
            return Err(ToolLoopCorruption::Inconsistent("approval payload").into());
        }
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "decision_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let user_command: Option<Uuid> = row.try_get("user_command_id")?;
    let source = required::<String>(&row, "decision_source")?;
    let source_kind = tool_approval_decision_source_from_str(&source).ok_or_else(|| {
        ToolLoopRepositoryError::from(ToolLoopCorruption::Unsupported {
            field: "decision_source",
            value: source,
        })
    })?;
    let input = match source_kind {
        ToolApprovalDecisionSourceStorageKind::UserCommand => {
            let command_id = durable_command_id_from_uuid(
                user_command.ok_or(ToolLoopCorruption::Missing("approval user command"))?,
            )
            .map_err(|_| ToolLoopCorruption::Inconsistent("approval user command identity"))?;
            let command = user_receipts
                .get(&command_id)
                .cloned()
                .ok_or(ToolLoopCorruption::Missing("approval user command receipt"))?;
            if command.command().request() != request
                || command.command().decision() != &decision
                || !matches!(command.result(), DecideToolRequestResult::Applied(_))
            {
                return Err(
                    ToolLoopCorruption::Inconsistent("approval user command receipt").into(),
                );
            }
            ToolApprovalResolutionReconstitutionInput::user_command(command)
        }
        ToolApprovalDecisionSourceStorageKind::PolicyAuto
            if user_command.is_none() && decision == ToolApprovalDecision::Approve =>
        {
            ToolApprovalResolutionReconstitutionInput::policy_auto(request)
        }
        ToolApprovalDecisionSourceStorageKind::SessionBlanket
            if user_command.is_none() && decision == ToolApprovalDecision::Approve =>
        {
            ToolApprovalResolutionReconstitutionInput::session_blanket(
                request,
                load_frozen_dangerous_tool_auto_approval(connection, request).await?,
            )
        }
        ToolApprovalDecisionSourceStorageKind::RuntimeSafety if user_command.is_none() => {
            let expected = ToolApprovalResolutionReconstitutionInput::runtime_safety(request)
                .reconstitute()
                .map_err(|_| {
                    ToolLoopCorruption::Inconsistent("runtime safety approval evidence")
                })?;
            if expected.decision() != &decision {
                return Err(
                    ToolLoopCorruption::Inconsistent("runtime safety approval evidence").into(),
                );
            }
            ToolApprovalResolutionReconstitutionInput::runtime_safety(request)
        }
        ToolApprovalDecisionSourceStorageKind::LifecycleClosure
            if decision == (ToolApprovalDecision::Deny { reason: None }) =>
        {
            let command_id = durable_command_id_from_uuid(
                user_command.ok_or(ToolLoopCorruption::Missing("closure decision command"))?,
            )
            .map_err(|_| ToolLoopCorruption::Inconsistent("closure decision command identity"))?;
            let command = user_receipts
                .get(&command_id)
                .ok_or(ToolLoopCorruption::Missing(
                    "closure decision command receipt",
                ))?;
            if command.command().request() != request
                || command.command().decision() != &decision
                || !matches!(command.result(), DecideToolRequestResult::Applied(_))
            {
                return Err(
                    ToolLoopCorruption::Inconsistent("closure decision command receipt").into(),
                );
            }
            ToolApprovalResolutionReconstitutionInput::lifecycle_closure(request)
        }
        ToolApprovalDecisionSourceStorageKind::Delegate if user_command.is_none() => {
            let delegate_model: Option<Uuid> = row.try_get("delegate_model_selection_id")?;
            let delegate_call: Option<Uuid> = row.try_get("delegate_model_call_id")?;
            let rationale: Option<String> = row.try_get("rationale")?;
            let request_record = delegate_requests
                .get(&request)
                .ok_or(ToolLoopCorruption::Missing("delegate approval request"))?;
            let recommendation = match decision {
                ToolApprovalDecision::Approve => DelegateApprovalRecommendation::Approve,
                ToolApprovalDecision::Deny { .. } => DelegateApprovalRecommendation::Deny,
            };
            let rationale = signalbox_domain::ToolDecisionRationale::try_new(
                rationale.ok_or(ToolLoopCorruption::Missing("delegate rationale"))?,
            )
            .map_err(|_| ToolLoopCorruption::Inconsistent("delegate rationale"))?;
            let stored_denial_reason = match &decision {
                ToolApprovalDecision::Approve => None,
                ToolApprovalDecision::Deny { reason } => reason.clone(),
            };
            let approval = DelegateToolApproval::try_new(
                request_record,
                DirectModelSelection::from_uuid(
                    delegate_model.ok_or(ToolLoopCorruption::Missing("delegate model"))?,
                ),
                signalbox_domain::ModelCallId::from_uuid(
                    delegate_call.ok_or(ToolLoopCorruption::Missing("delegate call"))?,
                ),
                recommendation,
                rationale,
            )
            .map_err(|_| ToolLoopCorruption::Inconsistent("delegate authority"))?;
            ToolApprovalResolutionReconstitutionInput::delegate(approval, stored_denial_reason)
        }
        ToolApprovalDecisionSourceStorageKind::UserOverride
            if user_command.is_none() && decision == ToolApprovalDecision::Approve =>
        {
            let denied_request: Option<Uuid> = row.try_get("override_denied_request_id")?;
            let override_command: Option<Uuid> = row.try_get("override_command_id")?;
            let request_record =
                delegate_requests
                    .get(&request)
                    .ok_or(ToolLoopCorruption::Missing(
                        "user-override approval request",
                    ))?;
            let denied_request = tool_request_id_from_uuid(
                denied_request.ok_or(ToolLoopCorruption::Missing("override denied request"))?,
            );
            let command = durable_command_id_from_uuid(
                override_command.ok_or(ToolLoopCorruption::Missing("override command"))?,
            )
            .map_err(|_| ToolLoopCorruption::Inconsistent("override command identity"))?;
            ToolApprovalResolutionReconstitutionInput::user_override(
                request,
                command,
                denied_request,
                request_record.approval_posture(),
            )
        }
        ToolApprovalDecisionSourceStorageKind::PolicyAuto
        | ToolApprovalDecisionSourceStorageKind::SessionBlanket => {
            return Err(ToolLoopCorruption::Inconsistent("automatic approval evidence").into());
        }
        ToolApprovalDecisionSourceStorageKind::Delegate => {
            return Err(ToolLoopCorruption::Inconsistent("delegate approval evidence").into());
        }
        ToolApprovalDecisionSourceStorageKind::RuntimeSafety => {
            return Err(
                ToolLoopCorruption::Inconsistent("runtime safety approval evidence").into(),
            );
        }
        ToolApprovalDecisionSourceStorageKind::LifecycleClosure => {
            return Err(
                ToolLoopCorruption::Inconsistent("lifecycle closure approval evidence").into(),
            );
        }
        ToolApprovalDecisionSourceStorageKind::UserOverride => {
            return Err(ToolLoopCorruption::Inconsistent("user-override approval evidence").into());
        }
    };
    input
        .reconstitute()
        .map_err(|_| ToolLoopCorruption::Inconsistent("approval resolution").into())
}

async fn load_user_decision_receipts(
    connection: &mut PgConnection,
    commands: &[DurableCommandId],
) -> Result<BTreeMap<DurableCommandId, PreparedDecideToolRequest>, ToolLoopRepositoryError> {
    if commands.is_empty() {
        return Ok(BTreeMap::new());
    }
    let command_uuids = commands
        .iter()
        .map(|command| durable_command_id_to_uuid(*command))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT command.command_id, command.request_id,
                command.decision_kind, command.denial_reason,
                command.result_kind, command.rejection_kind,
                command.result_earliest_undecided_request_id,
                approval.decision_source,
                request.request_ordinal, request.tool_name,
                request.arguments_kind, request.arguments_text,
                request.approval_posture,
                request.producing_model_call_id, request.session_id,
                request.turn_id
           FROM decide_tool_request_command AS command
           JOIN tool_request AS request
             ON request.request_id = command.request_id
           LEFT JOIN tool_approval_decision AS approval
             ON approval.user_command_id = command.command_id
          WHERE command.command_id = ANY($1)
            AND command.command_kind = 'decide_tool_request'
            AND command.storage_version = 1",
    )
    .bind(&command_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut receipts = BTreeMap::new();
    for row in rows {
        let command_id = durable_command_id_from_uuid(required(&row, "command_id")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("decision command identity"))?;
        let request = tool_request_id_from_uuid(required(&row, "request_id")?);
        let decision = decode_command_decision(&row)?;
        let command = DecideToolRequest::try_new(command_id, request, decision)
            .map_err(|_| ToolLoopCorruption::Inconsistent("decision command identity"))?;
        let result_kind: String = required(&row, "result_kind")?;
        let rejection: Option<String> = row.try_get("rejection_kind")?;
        let earliest: Option<Uuid> = row.try_get("result_earliest_undecided_request_id")?;
        if result_kind != "applied" || rejection.is_some() || earliest.is_some() {
            return Err(ToolLoopCorruption::Inconsistent("approval user command receipt").into());
        }
        let producing_call =
            signalbox_domain::ModelCallId::from_uuid(required(&row, "producing_model_call_id")?);
        let session = session_id_from_uuid(required(&row, "session_id")?);
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        let source: Option<String> = row.try_get("decision_source")?;
        let request_record = decode_request(row, producing_call, session, turn)?;
        let prepared = if source.as_deref() == Some("lifecycle_closure") {
            command.prepare_lifecycle_closure_applied(&request_record)
        } else {
            command.prepare_applied(&request_record)
        }
        .map_err(|_| ToolLoopCorruption::Inconsistent("applied decision receipt"))?;
        if receipts.insert(command_id, prepared).is_some() {
            return Err(ToolLoopCorruption::Inconsistent(
                "duplicate approval user command receipt",
            )
            .into());
        }
    }
    Ok(receipts)
}

async fn load_frozen_dangerous_tool_auto_approval(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<DangerousToolAutoApproval, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "WITH RECURSIVE configuration_origin AS (
             SELECT stored.*
               FROM tool_request AS requested
               JOIN queued_input_origin AS stored
                 ON stored.turn_id = requested.turn_id
                AND stored.session_id = requested.session_id
              WHERE requested.request_id = $1
             UNION
             SELECT source.*
               FROM configuration_origin AS current
               JOIN queued_input_origin AS source
                 ON source.turn_id = current.source_configuration_turn_id
                AND source.session_id = current.session_id
         )
         SELECT dangerous_tool_auto_approval
           FROM configuration_origin
          WHERE source_configuration_turn_id IS NULL",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_all(&mut *connection)
    .await?;
    let [row] = rows.as_slice() else {
        return Err(ToolLoopCorruption::Inconsistent("frozen session blanket authority").into());
    };
    let value: String = required(row, "dangerous_tool_auto_approval")?;
    dangerous_tool_auto_approval_from_str(&value).ok_or_else(|| {
        ToolLoopCorruption::Unsupported {
            field: "dangerous_tool_auto_approval",
            value,
        }
        .into()
    })
}

async fn load_attempts(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<ReconstitutedToolAttempt>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT attempt.*
           FROM runner_current_tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter().map(decode_attempt).collect()
}

/// Restores a runner-recovery round after its exact interrupted attempt has
/// been terminalized. The current-attempt view intentionally hides terminal
/// pure and idempotent attempts whose lease remains lost; this recovery-only
/// loader adds back only the attempt authenticated by the lifecycle wait.
async fn load_runner_recovery_attempts(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
    interrupted_attempt: Option<ToolAttemptId>,
) -> Result<Vec<ReconstitutedToolAttempt>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT attempt.*
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE request.producing_model_call_id = $1
            AND (
                EXISTS (
                    SELECT 1
                      FROM runner_current_tool_attempt AS current
                     WHERE current.attempt_id = attempt.attempt_id
                )
                OR attempt.attempt_id = $2
            )
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .bind(interrupted_attempt.map(tool_attempt_id_to_uuid))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter().map(decode_attempt).collect()
}

/// Loads the round's physical-attempt identities that
/// `runner_current_tool_attempt` hides as retired claimed-retry predecessors,
/// so batch reconstitution restores the durable retired-identity inventory and
/// keeps identity reuse a domain rejection rather than a key collision.
async fn load_retired_attempts(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<ToolAttemptId>, ToolLoopRepositoryError> {
    let attempts = sqlx::query_scalar::<_, Uuid>(
        "SELECT attempt.attempt_id
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE request.producing_model_call_id = $1
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_current_tool_attempt AS current
                 WHERE current.attempt_id = attempt.attempt_id
            )
          ORDER BY attempt.attempt_id",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    Ok(attempts
        .into_iter()
        .map(tool_attempt_id_from_uuid)
        .collect())
}

async fn load_runner_authorized_attempts(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<ToolAttemptId>, ToolLoopRepositoryError> {
    let attempts = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT generation.attempt_id
           FROM runner_lease_generation AS generation
           JOIN runner_current_tool_attempt AS attempt
             ON attempt.attempt_id = generation.attempt_id
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY generation.attempt_id",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    Ok(attempts
        .into_iter()
        .map(tool_attempt_id_from_uuid)
        .collect())
}

pub(crate) fn decode_attempt(
    row: PgRow,
) -> Result<ReconstitutedToolAttempt, ToolLoopRepositoryError> {
    let effect_class = match required::<String>(&row, "effect_class")?.as_str() {
        "effect_free" => ToolEffectClass::EffectFree,
        "external_effect" => ToolEffectClass::ExternalEffect,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "effect_class",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let generation: Decimal = required(&row, "dispatch_generation")?;
    let generation = u64::try_from(generation)
        .ok()
        .and_then(ToolDispatchGeneration::try_from_u64)
        .ok_or(ToolLoopCorruption::Inconsistent("dispatch generation"))?;
    let terminal: Option<String> = row.try_get("terminal_disposition_kind")?;
    let state = match (
        required::<String>(&row, "state_kind")?.as_str(),
        terminal.as_deref(),
    ) {
        ("prepared", None) => ToolAttemptReconstitutionState::Prepared,
        ("in_flight", None) => ToolAttemptReconstitutionState::InFlight,
        ("terminal", Some(_)) => ToolAttemptReconstitutionState::Ended(decode_attempt_end(&row)?),
        ("prepared" | "in_flight" | "terminal", _) => {
            return Err(ToolLoopCorruption::Inconsistent("attempt state payload").into());
        }
        (value, _) => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "tool_attempt.state_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    ToolAttemptReconstitutionInput::new(
        tool_attempt_id_from_uuid(required(&row, "attempt_id")?),
        tool_request_id_from_uuid(required(&row, "request_id")?),
        session_id_from_uuid(required(&row, "session_id")?),
        turn_id_from_uuid(required(&row, "turn_id")?),
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "issuing_turn_attempt_id")?),
        effect_class,
        generation,
        state,
    )
    .reconstitute()
    .map_err(|_| ToolLoopCorruption::Inconsistent("dispatch generation").into())
}

pub(crate) async fn load_approvals_by_request(
    connection: &mut PgConnection,
    requests: &[ToolRequestId],
) -> Result<
    BTreeMap<ToolRequestId, signalbox_domain::ToolApprovalResolution>,
    ToolLoopRepositoryError,
> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let request_uuids = requests
        .iter()
        .map(|request| tool_request_id_to_uuid(*request))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind, approval.decision_source,
                approval.denial_reason, approval.user_command_id,
                approval.delegate_model_selection_id, approval.delegate_model_call_id,
                approval.rationale, approval.override_denied_request_id,
                recorded.command_id AS override_command_id
           FROM tool_approval_decision AS approval
           LEFT JOIN tool_approval_user_override AS recorded
             ON recorded.denied_request_id = approval.override_denied_request_id
          WHERE approval.request_id = ANY($1)",
    )
    .bind(&request_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut approvals = BTreeMap::new();
    for approval in decode_approvals(connection, rows).await? {
        if approvals.insert(approval.request(), approval).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool approval").into());
        }
    }
    Ok(approvals)
}

pub(crate) async fn load_attempts_by_id(
    connection: &mut PgConnection,
    attempts: &[ToolAttemptId],
) -> Result<BTreeMap<ToolAttemptId, ReconstitutedToolAttempt>, ToolLoopRepositoryError> {
    if attempts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let attempt_uuids = attempts
        .iter()
        .map(|attempt| tool_attempt_id_to_uuid(*attempt))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT *
           FROM tool_attempt
          WHERE attempt_id = ANY($1)",
    )
    .bind(&attempt_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut loaded = BTreeMap::new();
    for row in rows {
        let attempt = tool_attempt_id_from_uuid(required(&row, "attempt_id")?);
        let reconstituted = decode_attempt(row)?;
        if loaded.insert(attempt, reconstituted).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool attempt").into());
        }
    }
    Ok(loaded)
}

fn decode_attempt_end(row: &PgRow) -> Result<ToolAttemptEnd, ToolLoopRepositoryError> {
    let stored_disposition = required::<String>(row, "terminal_disposition_kind")?;
    match tool_attempt_disposition_from_str(&stored_disposition) {
        Some(ToolAttemptDispositionStorageKind::Completed) => {
            match required::<String>(row, "result_content_kind")?.as_str() {
                "text" => Ok(ToolAttemptEnd::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(required(row, "result_text")?)
                            .map_err(|_| ToolLoopCorruption::Inconsistent("tool result text"))?,
                    ),
                }),
                value => Err(ToolLoopCorruption::Unsupported {
                    field: "result_content_kind",
                    value: value.to_owned(),
                }
                .into()),
            }
        }
        Some(ToolAttemptDispositionStorageKind::KnownFailed) => {
            let kind = decode_error_kind(&required::<String>(row, "error_kind")?)?;
            let detail = row
                .try_get::<Option<String>, _>("error_detail")?
                .map(|value| {
                    ToolExecutionErrorDetail::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("tool error detail"))
                })
                .transpose()?;
            Ok(ToolAttemptEnd::KnownFailed {
                error: ToolExecutionError::new(kind, detail),
            })
        }
        Some(ToolAttemptDispositionStorageKind::AwaitingChild) => {
            Ok(ToolAttemptEnd::AwaitingChild {
                spawning_request: tool_request_id_from_uuid(required(
                    row,
                    "wait_spawning_request_id",
                )?),
                child: session_id_from_uuid(required(row, "wait_child_session_id")?),
            })
        }
        Some(ToolAttemptDispositionStorageKind::Ambiguous) => Ok(ToolAttemptEnd::Ambiguous),
        None => Err(ToolLoopCorruption::Unsupported {
            field: "terminal_disposition_kind",
            value: stored_disposition,
        }
        .into()),
    }
}

fn attempt_end_matches_observation(
    end: &ToolAttemptEnd,
    observation: &ToolAttemptObservation,
) -> bool {
    matches!(
        (end, observation),
        (
            ToolAttemptEnd::Completed { result: stored },
            ToolAttemptObservation::Completed { result: observed },
        ) if stored == observed
    ) || matches!(
        (end, observation),
        (
            ToolAttemptEnd::KnownFailed { error: stored },
            ToolAttemptObservation::KnownFailed { error: observed },
        ) if stored == observed
    ) || matches!(
        (end, observation),
        (ToolAttemptEnd::Ambiguous, ToolAttemptObservation::Ambiguous)
    )
}

fn decode_error_kind(value: &str) -> Result<ToolExecutionErrorKind, ToolLoopRepositoryError> {
    match value {
        "unknown_tool" => Ok(ToolExecutionErrorKind::UnknownTool),
        "invalid_arguments" => Ok(ToolExecutionErrorKind::InvalidArguments),
        "preauthorization_rejected" => Ok(ToolExecutionErrorKind::PreauthorizationRejected),
        "execution_failed" => Ok(ToolExecutionErrorKind::ExecutionFailed),
        "result_too_large" => Ok(ToolExecutionErrorKind::ResultTooLarge),
        "crash_lost" => Ok(ToolExecutionErrorKind::CrashLost),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "error_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn load_current_attempt(
    connection: &mut PgConnection,
    attempt: ToolAttemptId,
) -> Result<Option<CurrentToolAttempt>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT *
           FROM tool_attempt
          WHERE attempt_id = $1
            AND state_kind IN ('prepared', 'in_flight')",
    )
    .bind(tool_attempt_id_to_uuid(attempt))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(decode_attempt)
        .transpose()?
        .map(|attempt| match attempt {
            ReconstitutedToolAttempt::Current(current) => Ok(current),
            ReconstitutedToolAttempt::Ended(_) => {
                Err(ToolLoopCorruption::Inconsistent("live attempt decode").into())
            }
        })
        .transpose()
}

async fn insert_prepared_attempt(
    connection: &mut PgConnection,
    attempt: &CurrentToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    if attempt.state() != CurrentToolAttemptState::Prepared {
        return Err(ToolLoopRepositoryError::InvalidTransition(
            "only a prepared attempt can be inserted",
        ));
    }
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')",
    )
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(tool_request_id_to_uuid(attempt.request()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(match attempt.effect_class() {
        ToolEffectClass::EffectFree => "effect_free",
        ToolEffectClass::ExternalEffect => "external_effect",
    })
    .bind(Decimal::from(attempt.generation().as_u64()))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn persist_ended_attempt(
    connection: &mut PgConnection,
    attempt: &EndedToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    let (
        disposition,
        result_kind,
        result_text,
        error_kind,
        error_detail,
        wait_spawning_request,
        wait_child,
    ) = encode_attempt_end(attempt.end());
    let rows = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = $1,
                result_content_kind = $2,
                result_text = $3,
                error_kind = $4,
                error_detail = $5,
                wait_spawning_request_id = $6,
                wait_child_session_id = $7
          WHERE attempt_id = $8
            AND request_id = $9
            AND session_id = $10
            AND turn_id = $11
            AND issuing_turn_attempt_id = $12
            AND dispatch_generation = $13
            AND state_kind IN ('prepared', 'in_flight')
            AND terminal_disposition_kind IS NULL",
    )
    .bind(disposition)
    .bind(result_kind)
    .bind(result_text)
    .bind(error_kind)
    .bind(error_detail)
    .bind(wait_spawning_request)
    .bind(wait_child)
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(tool_request_id_to_uuid(attempt.request()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(Decimal::from(attempt.generation().as_u64()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal tool attempt")
}

async fn mark_issuing_turn_attempt_running(
    connection: &mut PgConnection,
    attempt: &CurrentToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "tool issuing attempt authorization")
}

type EncodedToolAttemptEnd<'a> = (
    &'static str,
    Option<&'static str>,
    Option<&'a str>,
    Option<&'static str>,
    Option<&'a str>,
    Option<Uuid>,
    Option<Uuid>,
);

fn encode_attempt_end(end: &ToolAttemptEnd) -> EncodedToolAttemptEnd<'_> {
    match end {
        ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(text),
        } => (
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::Completed),
            Some("text"),
            Some(text.as_str()),
            None,
            None,
            None,
            None,
        ),
        ToolAttemptEnd::KnownFailed { error } => (
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::KnownFailed),
            None,
            None,
            Some(encode_error_kind(error.kind())),
            error.detail().map(ToolExecutionErrorDetail::as_str),
            None,
            None,
        ),
        ToolAttemptEnd::AwaitingChild {
            spawning_request,
            child,
        } => (
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::AwaitingChild),
            None,
            None,
            None,
            None,
            Some(tool_request_id_to_uuid(*spawning_request)),
            Some(session_id_to_uuid(*child)),
        ),
        ToolAttemptEnd::Ambiguous => (
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::Ambiguous),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    }
}

const fn encode_error_kind(value: ToolExecutionErrorKind) -> &'static str {
    match value {
        ToolExecutionErrorKind::UnknownTool => "unknown_tool",
        ToolExecutionErrorKind::InvalidArguments => "invalid_arguments",
        ToolExecutionErrorKind::PreauthorizationRejected => "preauthorization_rejected",
        ToolExecutionErrorKind::ExecutionFailed => "execution_failed",
        ToolExecutionErrorKind::ResultTooLarge => "result_too_large",
        ToolExecutionErrorKind::CrashLost => "crash_lost",
    }
}

pub(crate) async fn persist_tool_recovery_wait(
    connection: &mut PgConnection,
    attempt: &EndedToolAttempt,
    crash_lost: bool,
) -> Result<(), ToolLoopRepositoryError> {
    let turn_disposition = if crash_lost { "lost" } else { "ambiguous" };
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = $1
          WHERE turn_attempt_id = $2
            AND turn_id = $3
            AND session_id = $4
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(turn_disposition)
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "ambiguous tool issuing attempt")?;
    let lifecycle_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_tool_recovery',
                recovery_tool_attempt_id = $1,
                approval_tool_request_id = NULL
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4
            AND active_tool_round_call_id IS NOT NULL",
    )
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(attempt.issuing_attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(lifecycle_rows, "tool recovery lifecycle")?;
    let producing_call: Uuid = sqlx::query_scalar(
        "SELECT producing_model_call_id
           FROM tool_request
          WHERE request_id = $1
            AND turn_id = $2
            AND session_id = $3",
    )
    .bind(tool_request_id_to_uuid(attempt.request()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .fetch_one(&mut *connection)
    .await?;
    outbox::append(
        connection,
        OutboxEvent::ToolBatchTransition {
            session: attempt.session(),
            turn: attempt.turn(),
            producing_call: signalbox_domain::ModelCallId::from_uuid(producing_call),
            state: ToolBatchOutboxState::RecoveryRequired(attempt.attempt()),
        },
    )
    .await?;
    Ok(())
}

async fn persist_batch_decision(
    connection: &mut PgConnection,
    decision: &PreparedToolBatchDecision,
) -> Result<(), ToolLoopRepositoryError> {
    let (source, principal) = match decision.prepared_command().result() {
        DecideToolRequestResult::Applied(applied) => match applied.resolution().source() {
            signalbox_domain::ToolDecisionSource::UserCommand => (
                ToolApprovalDecisionSourceStorageKind::UserCommand,
                signalbox_domain::CommandPrincipal::Operator,
            ),
            signalbox_domain::ToolDecisionSource::LifecycleClosure => (
                ToolApprovalDecisionSourceStorageKind::LifecycleClosure,
                signalbox_domain::CommandPrincipal::Core,
            ),
            _ => {
                return Err(ToolLoopRepositoryError::InvalidTransition(
                    "explicit decision used an unsupported source",
                ));
            }
        },
        DecideToolRequestResult::Rejected(_) => {
            persist_decision_command(
                connection,
                decision.prepared_command(),
                signalbox_domain::CommandPrincipal::Operator,
            )
            .await?;
            return Ok(());
        }
    };
    persist_decision_command(connection, decision.prepared_command(), principal).await?;
    let DecideToolRequestResult::Applied(applied) = decision.prepared_command().result() else {
        return Ok(());
    };
    let (decision_kind, denial_reason) = encode_approval(applied.resolution().decision());
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tool_request_id_to_uuid(applied.resolution().request()))
    .bind(decision_kind)
    .bind(tool_approval_decision_source_to_str(source))
    .bind(denial_reason)
    .bind(durable_command_id_to_uuid(
        decision.prepared_command().command().command_id(),
    ))
    .execute(&mut *connection)
    .await?;
    match decision.active_phase() {
        ActiveTurnPhase::AwaitingApproval { request } => {
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET approval_tool_request_id = $1
                  WHERE turn_id = $2
                    AND session_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND approval_tool_request_id = $4
                    AND active_tool_round_call_id = $5",
            )
            .bind(tool_request_id_to_uuid(*request))
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(
                decision.prepared_command().command().request(),
            ))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "next tool approval wait")?;
        }
        ActiveTurnPhase::Running { current_attempt } => {
            if current_attempt.state() != &signalbox_domain::CurrentTurnAttemptState::Prepared {
                return Err(ToolLoopRepositoryError::InvalidTransition(
                    "decision continuation attempt is not prepared",
                ));
            }
            let predecessor: Uuid = sqlx::query_scalar(
                "SELECT turn_attempt_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND turn_id = $2
                    AND session_id = $3",
            )
            .bind(decision.batch().producing_call().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .fetch_one(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id,
                     continued_from_attempt_id, state_kind)
                 VALUES ($1, $2, $3, $4, 'prepared')",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(predecessor)
            .execute(&mut *connection)
            .await?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'running',
                        current_attempt_id = $1,
                        approval_tool_request_id = NULL
                  WHERE turn_id = $2
                    AND session_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND current_attempt_id IS NULL
                    AND approval_tool_request_id = $4
                    AND active_tool_round_call_id = $5",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(
                decision.prepared_command().command().request(),
            ))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "approved tool execution phase")?;
        }
        ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
            return Err(ToolLoopRepositoryError::InvalidTransition(
                "approval command cannot enter recovery",
            ));
        }
    }
    if applied.resolution().decider().is_some() {
        outbox::append(
            connection,
            OutboxEvent::ToolApprovalDecided {
                session: decision.batch().session(),
                turn: decision.batch().turn(),
                request: applied.resolution().request(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Settles a decision's injection receipt: applied decisions deliver to
/// the request's turn; a decision that arrives after its request was decided
/// or its turn ended is `not_delivered`. A decision naming no request has no
/// session to carry a receipt.
async fn settle_decision_injection(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    prepared: &PreparedDecideToolRequest,
) -> Result<(), ToolLoopRepositoryError> {
    let outcome = match prepared.result() {
        DecideToolRequestResult::Applied(_) => {
            outbox::InjectionOutcomeOutbox::Delivered { turn: Some(turn) }
        }
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::AlreadyResolved {
            ..
        }) => outbox::InjectionOutcomeOutbox::NotDelivered,
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::RequestNotFound {
            ..
        }) => return Ok(()),
        DecideToolRequestResult::Rejected(
            DecideToolRequestRejectedResult::NotEarliestUndecided { .. },
        ) => outbox::InjectionOutcomeOutbox::Rejected {
            kind: "not_earliest_undecided",
        },
    };
    outbox::append(
        connection,
        OutboxEvent::InjectionSettled {
            session,
            command: prepared.command().command_id(),
            outcome,
        },
    )
    .await?;
    Ok(())
}

async fn persist_decision_command(
    connection: &mut PgConnection,
    prepared: &PreparedDecideToolRequest,
    principal: signalbox_domain::CommandPrincipal,
) -> Result<(), ToolLoopRepositoryError> {
    let command = prepared.command();
    let (decision_kind, denial_reason) = encode_approval(command.decision());
    let (result_kind, rejection_kind, earliest) = match prepared.result() {
        DecideToolRequestResult::Applied(_) => ("applied", None, None),
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::RequestNotFound {
            ..
        }) => ("rejected", Some("request_not_found"), None),
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::AlreadyResolved {
            ..
        }) => ("rejected", Some("already_resolved"), None),
        DecideToolRequestResult::Rejected(
            DecideToolRequestRejectedResult::NotEarliestUndecided { earliest, .. },
        ) => (
            "rejected",
            Some("not_earliest_undecided"),
            Some(tool_request_id_to_uuid(*earliest)),
        ),
    };
    let issuer = crate::command_registry::issuer_columns(principal);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(DECIDE_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(DECIDE_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .bind(tool_request_id_to_uuid(command.request()))
    .bind(decision_kind)
    .bind(denial_reason)
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(earliest)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_decision_receipt(
    connection: &mut PgConnection,
    command_id: signalbox_domain::DurableCommandId,
) -> Result<Option<PreparedDecideToolRequest>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT command.request_id, command.decision_kind, command.denial_reason,
                result_kind, rejection_kind,
                result_earliest_undecided_request_id,
                approval.decision_source
           FROM decide_tool_request_command AS command
           LEFT JOIN tool_approval_decision AS approval
             ON approval.user_command_id = command.command_id
          WHERE command.command_id = $1
            AND command.command_kind = 'decide_tool_request'
            AND command.storage_version = 1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let request = tool_request_id_from_uuid(required(&row, "request_id")?);
    let decision = decode_command_decision(&row)?;
    let command = DecideToolRequest::try_new(command_id, request, decision)
        .map_err(|_| ToolLoopCorruption::Inconsistent("decision command identity"))?;
    let result_kind: String = required(&row, "result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let prepared = match (result_kind.as_str(), rejection.as_deref()) {
        ("applied", None) => {
            let request_record = load_request_by_id(connection, request)
                .await?
                .ok_or(ToolLoopCorruption::Missing("applied decision request"))?;
            let source: Option<String> = row.try_get("decision_source")?;
            if source.as_deref() == Some("lifecycle_closure") {
                command.prepare_lifecycle_closure_applied(&request_record)
            } else {
                command.prepare_applied(&request_record)
            }
            .map_err(|_| ToolLoopCorruption::Inconsistent("applied decision receipt"))?
        }
        ("rejected", Some("request_not_found")) => command.prepare_request_not_found(),
        ("rejected", Some("already_resolved")) => command.prepare_already_resolved(),
        ("rejected", Some("not_earliest_undecided")) => command.prepare_not_earliest(
            tool_request_id_from_uuid(required(&row, "result_earliest_undecided_request_id")?),
        ),
        _ => return Err(ToolLoopCorruption::Inconsistent("decision receipt result").into()),
    };
    Ok(Some(prepared))
}

async fn persist_override_command(
    connection: &mut PgConnection,
    prepared: &PreparedOverrideDeniedToolRequest,
) -> Result<(), ToolLoopRepositoryError> {
    let command = prepared.command();
    let (result_kind, rejection_kind) = match prepared.result() {
        OverrideDeniedToolRequestResult::Applied(_) => ("applied", None),
        OverrideDeniedToolRequestResult::Rejected(
            signalbox_domain::OverrideDeniedToolRequestRejectedResult::RequestNotFound { .. },
        ) => ("rejected", Some("request_not_found")),
        OverrideDeniedToolRequestResult::Rejected(
            signalbox_domain::OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                ..
            },
        ) => ("rejected", Some("request_not_in_session")),
        OverrideDeniedToolRequestResult::Rejected(
            signalbox_domain::OverrideDeniedToolRequestRejectedResult::NotDelegateDenied { .. },
        ) => ("rejected", Some("not_delegate_denied")),
        OverrideDeniedToolRequestResult::Rejected(
            signalbox_domain::OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                ..
            },
        ) => ("rejected", Some("not_terminally_denied")),
        OverrideDeniedToolRequestResult::Rejected(
            signalbox_domain::OverrideDeniedToolRequestRejectedResult::AlreadyOverridden { .. },
        ) => ("rejected", Some("already_overridden")),
    };
    let issuer =
        crate::command_registry::issuer_columns(signalbox_domain::CommandPrincipal::Operator);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(OVERRIDE_DENIED_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO override_denied_tool_request_command
            (command_id, command_kind, storage_version, session_id,
             request_id, result_kind, rejection_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(OVERRIDE_DENIED_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(tool_request_id_to_uuid(command.denied_request()))
    .bind(result_kind)
    .bind(rejection_kind)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_override_receipt(
    connection: &mut PgConnection,
    command_id: signalbox_domain::DurableCommandId,
) -> Result<Option<PreparedOverrideDeniedToolRequest>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT session_id, request_id, result_kind, rejection_kind
           FROM override_denied_tool_request_command
          WHERE command_id = $1
            AND command_kind = 'override_denied_tool_request'
            AND storage_version = 1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session = session_id_from_uuid(required(&row, "session_id")?);
    let denied_request = tool_request_id_from_uuid(required(&row, "request_id")?);
    let command = OverrideDeniedToolRequest::try_new(command_id, session, denied_request)
        .map_err(|_| ToolLoopCorruption::Inconsistent("override command identity"))?;
    let result_kind: String = required(&row, "result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let prepared = match (result_kind.as_str(), rejection.as_deref()) {
        ("applied", None) => {
            let recorded = load_recorded_override(connection, denied_request)
                .await?
                .ok_or(ToolLoopCorruption::Missing("applied override row"))?;
            command
                .reconstitute_applied(recorded)
                .map_err(|_| ToolLoopCorruption::Inconsistent("applied override receipt"))?
        }
        ("rejected", Some("request_not_found")) => command.prepare_request_not_found(),
        ("rejected", Some("request_not_in_session")) => command.prepare_request_not_in_session(),
        ("rejected", Some("not_delegate_denied")) => command.prepare_not_delegate_denied(),
        ("rejected", Some("not_terminally_denied")) => command.prepare_not_terminally_denied(),
        ("rejected", Some("already_overridden")) => command.prepare_already_overridden(),
        _ => return Err(ToolLoopCorruption::Inconsistent("override receipt result").into()),
    };
    Ok(Some(prepared))
}

/// Loads one complete recorded override row with the denied request's matching
/// command shape.
async fn load_recorded_override(
    connection: &mut PgConnection,
    denied_request: ToolRequestId,
) -> Result<Option<signalbox_domain::RecordedUserOverride>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT recorded.command_id, recorded.session_id, recorded.judge_model_call_id,
                request.tool_name, request.arguments_kind, request.arguments_text
           FROM tool_approval_user_override AS recorded
           JOIN tool_request AS request
             ON request.request_id = recorded.denied_request_id
          WHERE recorded.denied_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(denied_request))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let command = durable_command_id_from_uuid(required(&row, "command_id")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("recorded override command identity"))?;
    let session = session_id_from_uuid(required(&row, "session_id")?);
    let judge_call =
        signalbox_domain::ModelCallId::from_uuid(required(&row, "judge_model_call_id")?);
    let tool = ToolName::try_new(required(&row, "tool_name")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("recorded override tool name"))?;
    let arguments_kind = match required::<String>(&row, "arguments_kind")?.as_str() {
        "json" => ToolArgumentsKind::Json,
        "undecodable" => ToolArgumentsKind::Undecodable,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "arguments_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let arguments =
        NormalizedToolArguments::try_from_stored(arguments_kind, required(&row, "arguments_text")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("recorded override arguments"))?;
    Ok(Some(signalbox_domain::RecordedUserOverride::new(
        command,
        session,
        denied_request,
        judge_call,
        tool,
        arguments,
    )))
}

/// Loads the request's durable terminal logical resolution, when it exists.
///
/// The resolution is the request's materialized result entry: a denied
/// result, an executed attempt's result, or the turn-end closure. A request
/// whose round is still resolving has none.
async fn load_terminal_request_resolution(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<signalbox_domain::ToolRequestResolution>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT entry.payload_kind, entry.tool_result_attempt_id
           FROM semantic_transcript_entry AS entry
           LEFT JOIN tool_attempt AS result_attempt
             ON result_attempt.attempt_id = entry.tool_result_attempt_id
          WHERE (
                    entry.payload_kind IN ('tool_denied', 'tool_closed_by_turn_end')
                    AND entry.tool_result_request_id = $1
                )
             OR (
                    entry.payload_kind = 'tool_execution_result'
                    AND result_attempt.request_id = $1
                )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    match required::<String>(&row, "payload_kind")?.as_str() {
        "tool_denied" => Ok(Some(signalbox_domain::ToolRequestResolution::Denied {
            request,
        })),
        "tool_closed_by_turn_end" => Ok(Some(
            signalbox_domain::ToolRequestResolution::ClosedByTurnEnd { request },
        )),
        "tool_execution_result" => Ok(Some(signalbox_domain::ToolRequestResolution::Executed {
            attempt: tool_attempt_id_from_uuid(required(&row, "tool_result_attempt_id")?),
        })),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "payload_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

/// Loads the command that already recorded an override for the request, if any.
async fn load_existing_override_command(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<DurableCommandId>, ToolLoopRepositoryError> {
    let command: Option<Uuid> = sqlx::query_scalar(
        "SELECT command_id
           FROM tool_approval_user_override
          WHERE denied_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut *connection)
    .await?;
    command
        .map(|value| {
            durable_command_id_from_uuid(value).map_err(|_| {
                ToolLoopCorruption::Inconsistent("recorded override command identity").into()
            })
        })
        .transpose()
}

async fn decision_exists(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, ToolLoopRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM tool_approval_decision
              WHERE request_id = $1
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(&mut *connection)
    .await
    .map_err(Into::into)
}

async fn request_closed_by_turn_end(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, ToolLoopRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM tool_request AS request
               JOIN turn_lifecycle AS turn
                 ON turn.turn_id = request.turn_id
                AND turn.session_id = request.session_id
              WHERE request.request_id = $1
                AND turn.state_kind = 'terminal'
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(connection)
    .await
    .map_err(Into::into)
}

pub(crate) async fn load_request_by_id(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text, approval_posture,
                producing_model_call_id, session_id, turn_id
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| {
        let call =
            signalbox_domain::ModelCallId::from_uuid(required(&row, "producing_model_call_id")?);
        let session = session_id_from_uuid(required(&row, "session_id")?);
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        decode_request(row, call, session, turn)
    })
    .transpose()
}

pub(crate) async fn load_requests_by_id(
    connection: &mut PgConnection,
    requests: &[ToolRequestId],
) -> Result<BTreeMap<ToolRequestId, signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let request_uuids = requests
        .iter()
        .map(|request| tool_request_id_to_uuid(*request))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text, approval_posture,
                producing_model_call_id, session_id, turn_id
           FROM tool_request
          WHERE request_id = ANY($1)",
    )
    .bind(&request_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut loaded = BTreeMap::new();
    for row in rows {
        let request = tool_request_id_from_uuid(required(&row, "request_id")?);
        let call =
            signalbox_domain::ModelCallId::from_uuid(required(&row, "producing_model_call_id")?);
        let session = session_id_from_uuid(required(&row, "session_id")?);
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        let record = decode_request(row, call, session, turn)?;
        if loaded.insert(request, record).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool request").into());
        }
    }
    Ok(loaded)
}

fn decode_command_decision(row: &PgRow) -> Result<ToolApprovalDecision, ToolLoopRepositoryError> {
    let reason: Option<String> = row.try_get("denial_reason")?;
    match required::<String>(row, "decision_kind")?.as_str() {
        "approve" if reason.is_none() => Ok(ToolApprovalDecision::Approve),
        "deny" => Ok(ToolApprovalDecision::Deny {
            reason: reason
                .map(|value| {
                    ToolDenialReason::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("command denial reason"))
                })
                .transpose()?,
        }),
        "approve" => Err(ToolLoopCorruption::Inconsistent("command decision payload").into()),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "decision_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn encode_approval(decision: &ToolApprovalDecision) -> (&'static str, Option<&str>) {
    match decision {
        ToolApprovalDecision::Approve => ("approve", None),
        ToolApprovalDecision::Deny { reason } => {
            ("deny", reason.as_ref().map(ToolDenialReason::as_str))
        }
    }
}

pub(crate) async fn persist_result_entries(
    connection: &mut PgConnection,
    projection: &PreparedToolResultProjection,
) -> Result<(), ToolLoopRepositoryError> {
    persist_result_entry_slice(connection, projection.entries()).await
}

pub(crate) async fn load_foreground_delegation_outcome(
    connection: &mut PgConnection,
    parent: SessionId,
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    child: SessionId,
) -> Result<DelegationOutcome, ToolLoopRepositoryError> {
    load_optional_foreground_delegation_outcome(
        connection,
        parent,
        awaiting_request,
        spawning_request,
        child,
    )
    .await?
    .ok_or_else(|| ToolLoopCorruption::Missing("foreground child result").into())
}

pub(crate) async fn load_optional_foreground_delegation_outcome(
    connection: &mut PgConnection,
    parent: SessionId,
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    child: SessionId,
) -> Result<Option<DelegationOutcome>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT result.outcome_kind, result.content_text,
                event.reason_kind, event.provenance_kind,
                event.provenance_session_id, event.provenance_turn_id,
                event.provenance_goal_generation, event.provenance_command_id
           FROM session_child_result_delivery AS delivery
           JOIN session_delegation_wait AS waiting
             ON waiting.awaiting_tool_request_id = delivery.awaiting_tool_request_id
            AND waiting.spawning_tool_request_id = delivery.spawning_tool_request_id
            AND waiting.parent_session_id = delivery.parent_session_id
            AND waiting.child_session_id = $4
            AND waiting.wait_mode = 'foreground'
           JOIN session_child_result AS result
             ON result.spawning_tool_request_id = delivery.spawning_tool_request_id
           JOIN session_delegation_event AS event
             ON event.spawning_tool_request_id = result.spawning_tool_request_id
            AND event.event_ordinal = result.event_ordinal
            AND event.event_kind = result.event_kind
          WHERE delivery.awaiting_tool_request_id = $1
            AND delivery.spawning_tool_request_id = $2
            AND delivery.parent_session_id = $3
            AND delivery.delivery_sequence IS NULL",
    )
    .bind(tool_request_id_to_uuid(awaiting_request))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(session_id_to_uuid(parent))
    .bind(session_id_to_uuid(child))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind = match required::<String>(&row, "outcome_kind")?.as_str() {
        "result_returned" => DelegationOutcomeKind::ResultReturned,
        "child_failed" => DelegationOutcomeKind::ChildFailed,
        "child_stopped" => DelegationOutcomeKind::ChildStopped,
        "child_cancelled" => DelegationOutcomeKind::ChildCancelled,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "delegation outcome",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let content = row
        .try_get::<Option<String>, _>("content_text")?
        .map(DelegationContent::try_new)
        .transpose()
        .map_err(|_| ToolLoopCorruption::Inconsistent("delegation result content"))?;
    let reason = match required::<String>(&row, "reason_kind")?.as_str() {
        "child_completed" => DelegationOutcomeReason::ChildCompleted,
        "child_execution_failed" => DelegationOutcomeReason::ChildExecutionFailed,
        "child_result_unavailable" => DelegationOutcomeReason::ChildResultUnavailable,
        "child_cancelled" => DelegationOutcomeReason::ChildCancelled,
        "parent_stopped_parent_and_descendants" => DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        },
        "parent_cancelled_parent_and_descendants" => DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        },
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "delegation outcome reason",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let provenance_session = session_id_from_uuid(required(&row, "provenance_session_id")?);
    let provenance = match (
        required::<String>(&row, "provenance_kind")?.as_str(),
        row.try_get::<Option<Uuid>, _>("provenance_turn_id")?,
        row.try_get::<Option<Decimal>, _>("provenance_goal_generation")?,
        row.try_get::<Option<Uuid>, _>("provenance_command_id")?,
    ) {
        ("child_turn", Some(turn), None, None) => {
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: provenance_session,
                turn: turn_id_from_uuid(turn),
            }
        }
        ("parent_turn_command", Some(turn), None, Some(command)) => {
            DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session: provenance_session,
                turn: turn_id_from_uuid(turn),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    ToolLoopCorruption::Inconsistent("delegation provenance command")
                })?,
            }
        }
        ("parent_goal_command", None, Some(generation), Some(command)) => {
            let generation = crate::mapping::positive_u64_from_numeric(generation)
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or(ToolLoopCorruption::Inconsistent(
                    "delegation provenance generation",
                ))?;
            DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session: provenance_session,
                generation: GoalGeneration::new(generation),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    ToolLoopCorruption::Inconsistent("delegation provenance command")
                })?,
            }
        }
        ("parent_lifecycle_command", None, None, Some(command)) => {
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session: provenance_session,
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    ToolLoopCorruption::Inconsistent("delegation provenance command")
                })?,
            }
        }
        _ => {
            return Err(ToolLoopCorruption::Inconsistent("delegation result provenance").into());
        }
    };
    DelegationOutcome::reconstitute(kind, content, reason, provenance)
        .map(Some)
        .ok_or_else(|| ToolLoopCorruption::Inconsistent("delegation result outcome").into())
}

pub(crate) async fn persist_result_entry_slice(
    connection: &mut PgConnection,
    entries: &[signalbox_domain::SemanticTranscriptEntry],
) -> Result<(), ToolLoopRepositoryError> {
    for entry in entries {
        let (kind, request, attempt, delegation_awaiting, delegation_spawning) =
            match entry.payload() {
                SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => (
                    "tool_execution_result",
                    None,
                    Some(tool_attempt_id_to_uuid(*attempt)),
                    None,
                    None,
                ),
                SemanticTranscriptEntryPayload::ToolDenied { request } => (
                    "tool_denied",
                    Some(tool_request_id_to_uuid(*request)),
                    None,
                    None,
                    None,
                ),
                SemanticTranscriptEntryPayload::ToolClosed { request } => (
                    "tool_closed_by_turn_end",
                    Some(tool_request_id_to_uuid(*request)),
                    None,
                    None,
                    None,
                ),
                SemanticTranscriptEntryPayload::DelegationResult {
                    awaiting_request,
                    spawning_request,
                    mode: signalbox_domain::DelegationWaitMode::Foreground,
                    delivery_sequence: None,
                    ..
                } => (
                    "delegation_result",
                    Some(tool_request_id_to_uuid(*awaiting_request)),
                    None,
                    Some(tool_request_id_to_uuid(*awaiting_request)),
                    Some(tool_request_id_to_uuid(*spawning_request)),
                ),
                _ => {
                    return Err(ToolLoopCorruption::Inconsistent("tool result payload").into());
                }
            };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id, tool_result_attempt_id,
                 delegation_result_awaiting_tool_request_id,
                 delegation_result_spawning_tool_request_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(kind)
        .bind(request)
        .bind(attempt)
        .bind(delegation_awaiting)
        .bind(delegation_spawning)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: signalbox_domain::DurableCommandId,
) -> Result<Option<CommandKind>, ToolLoopRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => error.into(),
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
                ToolLoopCorruption::Unsupported {
                    field: "durable_command.command_kind",
                    value,
                }
                .into()
            }
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(_)) => {
                ToolLoopCorruption::Inconsistent("durable command storage version").into()
            }
            RegistryInspectionError::Corruption(
                RegistryCorruption::MissingTypedRecord(_)
                | RegistryCorruption::ConflictingTypedRecords,
            ) => ToolLoopCorruption::Inconsistent("durable command typed record").into(),
        })
}

pub(crate) async fn lock_tool_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ToolLoopRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::START_ELIGIBLE_TURN)
            .bind(session_id_to_uuid(session))
            .fetch_one(connection)
            .await?;
    match (session_exists, scheduler) {
        (true, Some(_)) => Ok(()),
        (true, None) => Err(ToolLoopCorruption::Missing("session scheduler row").into()),
        (false, None) => Err(ToolLoopCorruption::Missing("session").into()),
        (false, Some(_)) => Err(ToolLoopCorruption::Inconsistent("orphan scheduler row").into()),
    }
}

async fn admit_tool_preauthorization(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
    preauthorization: ToolPreauthorization,
) -> Result<BlobReadAdmission, ToolLoopRepositoryError> {
    let (digest, decoded_bytes) = match preauthorization {
        ToolPreauthorization::Unmetered => return Ok(BlobReadAdmission::Admitted),
        ToolPreauthorization::BlobMetadata { digest } => (digest, None),
        ToolPreauthorization::BlobRead {
            digest,
            decoded_bytes,
        } => (digest, Some(decoded_bytes)),
    };
    let visible: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM tool_request AS request
              JOIN model_call AS call
                ON call.model_call_id = request.producing_model_call_id
               AND call.session_id = request.session_id
              JOIN context_frontier_member AS member
                ON member.owning_session_id = call.session_id
               AND member.context_frontier_id = call.context_frontier_id
              JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = member.source_session_id
               AND entry.semantic_entry_id = member.semantic_entry_id
              JOIN accepted_input_content_part AS part
                ON part.accepted_input_id = entry.origin_accepted_input_id
             WHERE request.request_id = $1
               AND part.part_kind = 'attachment'
               AND part.blob_digest = $2
        )",
    )
    .bind(tool_request_id_to_uuid(request))
    .bind(digest.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await?;
    if !visible {
        return Ok(BlobReadAdmission::NotVisible);
    }
    let Some(decoded_bytes) = decoded_bytes else {
        return Ok(BlobReadAdmission::Admitted);
    };
    if decoded_bytes.get() > MAX_BLOB_READ_TOOL_BYTES {
        return Err(ToolLoopCorruption::Inconsistent("blob read request byte bound").into());
    }

    let existing = sqlx::query(
        "SELECT session_id, turn_id, blob_digest, decoded_byte_count, admitted,
                rejection_reason
           FROM blob_read_tool_charge
          WHERE request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(row) = existing {
        let recorded_bytes = positive_u64_from_numeric(required(&row, "decoded_byte_count")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("blob read charged byte count"))?;
        if required::<Uuid>(&row, "session_id")? != session_id_to_uuid(session)
            || required::<Uuid>(&row, "turn_id")? != turn_id_to_uuid(turn)
            || required::<Vec<u8>>(&row, "blob_digest")? != digest.as_bytes().as_slice()
            || recorded_bytes != decoded_bytes.get()
        {
            return Err(ToolLoopCorruption::Inconsistent("blob read request charge").into());
        }
        return BlobReadAdmission::from_charge(
            required(&row, "admitted")?,
            row.try_get::<Option<String>, _>("rejection_reason")?,
        );
    }

    let totals = sqlx::query(
        "SELECT count(*) AS request_count,
                COALESCE(sum(decoded_byte_count), 0) AS decoded_bytes
           FROM blob_read_tool_charge
          WHERE turn_id = $1 AND admitted",
    )
    .bind(turn_id_to_uuid(turn))
    .fetch_one(&mut **transaction)
    .await?;
    let request_count: i64 = required(&totals, "request_count")?;
    let decoded_total: Decimal = required(&totals, "decoded_bytes")?;
    if request_count < 0 || !decoded_total.fract().is_zero() || decoded_total.is_sign_negative() {
        return Err(ToolLoopCorruption::Inconsistent("blob read turn charge totals").into());
    }
    let decoded_total = u64::try_from(decoded_total)
        .map_err(|_| ToolLoopCorruption::Inconsistent("blob read turn charged bytes"))?;
    let admission = blob_read_charge_admission(request_count, decoded_total, decoded_bytes);
    let admitted = admission == BlobReadAdmission::Admitted;
    let rows = sqlx::query(
        "INSERT INTO blob_read_tool_charge
            (request_id, session_id, turn_id, blob_digest, decoded_byte_count, admitted,
             rejection_reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tool_request_id_to_uuid(request))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(digest.as_bytes().as_slice())
    .bind(Decimal::from(decoded_bytes.get()))
    .bind(admitted)
    .bind(admission.rejection().map(blob_read_rejection_to_str))
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    require_single(rows, "blob read request charge")?;
    Ok(admission)
}

fn blob_read_charge_admission(
    request_count: i64,
    decoded_total: u64,
    requested: NonZeroU64,
) -> BlobReadAdmission {
    if request_count >= MAX_BLOB_READ_REQUESTS_PER_TURN {
        BlobReadAdmission::TurnReadCountExceeded
    } else if decoded_total
        .checked_add(requested.get())
        .is_none_or(|total| total > MAX_BLOB_READ_TURN_BYTES)
    {
        BlobReadAdmission::TurnByteBudgetExceeded
    } else {
        BlobReadAdmission::Admitted
    }
}

fn required<T>(row: &PgRow, column: &'static str) -> Result<T, ToolLoopRepositoryError>
where
    for<'value> T: sqlx::Decode<'value, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or_else(|| ToolLoopCorruption::Missing(column).into())
}

fn require_single(rows: u64, relationship: &'static str) -> Result<(), ToolLoopRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ToolLoopCorruption::Inconsistent(relationship).into())
    }
}

async fn finish_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, ToolLoopRepositoryError>,
) -> Result<T, ToolLoopRepositoryError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|source| {
                let commit_ambiguous = commit_failure_is_ambiguous(&source);
                ToolLoopRepositoryError::Database {
                    source,
                    commit_ambiguous,
                }
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod blob_read_budget_tests {
    use super::*;

    #[test]
    fn exact_blob_read_turn_byte_bound_is_admitted() {
        assert_eq!(
            blob_read_charge_admission(
                3,
                MAX_BLOB_READ_TURN_BYTES - MAX_BLOB_READ_TOOL_BYTES,
                NonZeroU64::new(MAX_BLOB_READ_TOOL_BYTES).expect("the tool bound is positive"),
            ),
            BlobReadAdmission::Admitted
        );
    }

    #[test]
    fn blob_read_turn_byte_overflow_is_rejected() {
        assert_eq!(
            blob_read_charge_admission(
                4,
                MAX_BLOB_READ_TURN_BYTES,
                NonZeroU64::new(1).expect("one is positive"),
            ),
            BlobReadAdmission::TurnByteBudgetExceeded
        );
    }

    #[test]
    fn blob_read_turn_request_count_bound_is_rejected() {
        assert_eq!(
            blob_read_charge_admission(
                MAX_BLOB_READ_REQUESTS_PER_TURN,
                0,
                NonZeroU64::new(1).expect("one is positive"),
            ),
            BlobReadAdmission::TurnReadCountExceeded
        );
    }
}
