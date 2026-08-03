//! Durable lifecycle for delegated tool-approval judge calls.

use std::{collections::HashSet, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCredentialReference, OperatorFailureClass,
};
use signalbox_domain::{
    ActiveTurnPhase, DelegateApprovalRecommendation, DelegateToolApproval, DirectModelSelection,
    FrozenModelSelection, ModelCallId, ModelTargetCatalog, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SessionId, ToolApprovalPosture,
    ToolDecisionRationale, ToolRequest, ToolRequestId, TurnAttemptId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    ModelCredentialFamilyCatalog, commit_failure_is_ambiguous,
    mapping::{session_id_to_uuid, tool_request_id_to_uuid, turn_id_to_uuid},
    model_execution::{lock_session, resolve_session_credential},
    tool_loop::{ToolLoopRepositoryError, load_active_batch_from_connection},
};

/// Exact durable facts committed before approval-judge provider preparation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedApprovalJudge {
    request: ToolRequest,
    call: ModelCallId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential_reference: String,
    input_includes_cache_tokens: bool,
}

impl PreparedApprovalJudge {
    /// Borrows the exact parked request being judged.
    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    /// Returns the dedicated model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the direct selection frozen for the judge call.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    /// Returns the exact resolved provider target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Borrows the pinned non-secret credential reference.
    pub fn credential_reference(&self) -> &str {
        &self.credential_reference
    }

    /// Reports whether the provider's input total includes cache axes.
    pub const fn input_includes_cache_tokens(&self) -> bool {
        self.input_includes_cache_tokens
    }
}

/// Result of reconciling the active delegated wait with its dedicated call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareApprovalJudgeOutcome {
    /// No unjudged delegated request is currently active.
    NoWork,
    /// A Prepared call may prepare and then authorize one provider interaction.
    Ready(Box<PreparedApprovalJudge>),
    /// A prior process authorized this call without recording a terminal result.
    InFlightAfterRestart(Box<PreparedApprovalJudge>),
}

/// Durable effect of a successfully completed judge call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteApprovalJudgeOutcome {
    /// Approve or deny was recorded and batch progression may continue.
    Decided,
    /// The judge explicitly left the request parked for a user decision.
    EscalatedToHuman,
}

/// Closed failure disposition stored for a judge call that cannot decide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailedApprovalJudgeDisposition {
    /// A trustworthy local failure occurred before or after authorization.
    KnownFailed,
    /// The provider explicitly refused the judge request.
    Refused,
    /// Cancellation was definitive.
    Cancelled,
    /// Provider acceptance or completion is uncertain.
    Ambiguous,
}

impl FailedApprovalJudgeDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::KnownFailed => "known_failed",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// PostgreSQL-backed delegated approval-judge lifecycle.
#[derive(Clone, Debug)]
pub struct PostgresApprovalJudgeRepository {
    pool: PgPool,
    targets: ModelTargetCatalog,
    fallback_credential: ModelCallCredentialReference,
    credential_families: Option<ModelCredentialFamilyCatalog>,
    cache_inclusive_input_targets: HashSet<ResolvedProviderTarget>,
}

impl PostgresApprovalJudgeRepository {
    /// Uses the exact model configuration shared by ordinary call persistence.
    pub(crate) const fn new(
        pool: PgPool,
        targets: ModelTargetCatalog,
        fallback_credential: ModelCallCredentialReference,
        credential_families: Option<ModelCredentialFamilyCatalog>,
        cache_inclusive_input_targets: HashSet<ResolvedProviderTarget>,
    ) -> Self {
        Self {
            pool,
            targets,
            fallback_credential,
            credential_families,
            cache_inclusive_input_targets,
        }
    }

    /// Records or reloads the call for the exact earliest delegated wait.
    ///
    /// An absent configured selection resolves to the direct selection used by
    /// the ordinary call that proposed the request.
    pub async fn prepare(
        &self,
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        configured_selection: Option<DirectModelSelection>,
    ) -> Result<PrepareApprovalJudgeOutcome, ApprovalJudgeRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_session(&mut transaction, session)
            .await
            .map_err(map_model_error)?;
        let Some(batch) = load_active_batch_from_connection(&mut transaction, session, turn)
            .await
            .map_err(map_tool_error)?
        else {
            transaction.rollback().await?;
            return Ok(PrepareApprovalJudgeOutcome::NoWork);
        };
        let Some(waiting) = batch.awaiting_approval() else {
            transaction.rollback().await?;
            return Ok(PrepareApprovalJudgeOutcome::NoWork);
        };
        let request = batch
            .requests()
            .iter()
            .find(|request| request.id() == waiting.request())
            .ok_or(ApprovalJudgeCorruption::Missing(
                "awaiting approval request",
            ))?;
        if request.approval_posture() != ToolApprovalPosture::Delegated {
            transaction.rollback().await?;
            return Ok(PrepareApprovalJudgeOutcome::NoWork);
        }
        if let Some(row) = load_judge(&mut transaction, request.id()).await? {
            let state: String = required(&row, "state_kind")?;
            let prepared = decode_prepared(row, request.clone())?;
            transaction.rollback().await?;
            return match state.as_str() {
                "prepared" => Ok(PrepareApprovalJudgeOutcome::Ready(Box::new(prepared))),
                "in_flight" => Ok(PrepareApprovalJudgeOutcome::InFlightAfterRestart(Box::new(
                    prepared,
                ))),
                "terminal" => Ok(PrepareApprovalJudgeOutcome::NoWork),
                value => Err(ApprovalJudgeCorruption::UnsupportedState(value.to_owned()).into()),
            };
        }
        let judged_selection =
            load_producing_selection(&mut transaction, batch.producing_call()).await?;
        let selection = configured_selection.unwrap_or(judged_selection);
        let resolved = self
            .targets
            .resolve(FrozenModelSelection::Direct(selection))
            .map_err(|_| ApprovalJudgeRepositoryError::TargetUnavailable)?;
        let target = resolved.target();
        let credential = resolve_session_credential(
            &mut transaction,
            session,
            target,
            &self.fallback_credential,
            self.credential_families.as_ref(),
        )
        .await
        .map_err(map_model_error)?;
        let input_includes_cache_tokens = self.cache_inclusive_input_targets.contains(&target);
        let inserted = sqlx::query(
            "INSERT INTO tool_approval_judge_model_call
                (model_call_id, request_id, session_id, turn_id,
                 direct_model_selection_id, resolved_provider_model_identity_id,
                 credential_reference, usage_input_includes_cache_tokens, state_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'prepared')",
        )
        .bind(call.into_uuid())
        .bind(tool_request_id_to_uuid(request.id()))
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .bind(selection.into_uuid())
        .bind(target.identity().into_uuid())
        .bind(credential.as_str())
        .bind(input_includes_cache_tokens)
        .execute(&mut *transaction)
        .await;
        if let Err(error) = inserted {
            transaction.rollback().await?;
            return Err(classify_insert(error));
        }
        let prepared = PreparedApprovalJudge {
            request: request.clone(),
            call,
            selection,
            target,
            credential_reference: credential.as_str().to_owned(),
            input_includes_cache_tokens,
        };
        transaction
            .commit()
            .await
            .map_err(ApprovalJudgeRepositoryError::commit)?;
        Ok(PrepareApprovalJudgeOutcome::Ready(Box::new(prepared)))
    }

    /// Commits InFlight before the dedicated provider interaction begins.
    pub async fn authorize(
        &self,
        prepared: &PreparedApprovalJudge,
    ) -> Result<(), ApprovalJudgeRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_session(&mut transaction, prepared.request.session())
            .await
            .map_err(map_model_error)?;
        let state = require_exact_judge(&mut transaction, prepared).await?;
        if state == "in_flight" {
            transaction.rollback().await?;
            return Ok(());
        }
        if state != "prepared" {
            return Err(ApprovalJudgeCorruption::Inconsistent("judge authorization state").into());
        }
        let rows = sqlx::query(
            "UPDATE tool_approval_judge_model_call SET state_kind = 'in_flight'
              WHERE model_call_id = $1 AND session_id = $2 AND state_kind = 'prepared'",
        )
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(rows, "judge authorization")?;
        transaction
            .commit()
            .await
            .map_err(ApprovalJudgeRepositoryError::commit)
    }

    /// Atomically records a completed recommendation and any decision effect.
    pub async fn complete(
        &self,
        prepared: &PreparedApprovalJudge,
        recommendation: DelegateApprovalRecommendation,
        rationale: ToolDecisionRationale,
        usage: ProviderReportedTokenUsage,
        continuation_attempt: TurnAttemptId,
    ) -> Result<CompleteApprovalJudgeOutcome, ApprovalJudgeRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_session(&mut transaction, prepared.request.session())
            .await
            .map_err(map_model_error)?;
        let state = require_exact_judge(&mut transaction, prepared).await?;
        if state == "terminal" {
            let outcome = exact_completed(
                &mut transaction,
                prepared,
                recommendation,
                &rationale,
                usage,
            )
            .await?;
            transaction.rollback().await?;
            return outcome.ok_or_else(|| {
                ApprovalJudgeCorruption::Inconsistent("completed judge replay").into()
            });
        }
        if state != "in_flight" {
            return Err(ApprovalJudgeCorruption::Inconsistent("judge completion state").into());
        }
        let batch = load_active_batch_from_connection(
            &mut transaction,
            prepared.request.session(),
            prepared.request.turn(),
        )
        .await
        .map_err(map_tool_error)?
        .ok_or(ApprovalJudgeCorruption::Missing("active delegated batch"))?;
        let approval = DelegateToolApproval::try_new(
            prepared.request(),
            prepared.selection,
            prepared.call,
            recommendation,
            rationale,
        )
        .map_err(|_| ApprovalJudgeRepositoryError::AuthorityExceeded)?;
        let final_request = batch
            .requests()
            .iter()
            .filter(|request| batch.approval(request.id()).is_none())
            .count()
            == 1;
        let continuation = (recommendation != DelegateApprovalRecommendation::EscalateToHuman
            && final_request)
            .then_some(continuation_attempt);
        let decision = batch
            .prepare_delegate_decision(approval, continuation)
            .map_err(|_| ApprovalJudgeCorruption::Inconsistent("delegate transition"))?;
        let encoded = encode_usage(usage);
        let rows = sqlx::query(
            "UPDATE tool_approval_judge_model_call
                SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                    recommendation_kind = $1, rationale = $2,
                    input_tokens = $3, output_tokens = $4,
                    cache_creation_input_tokens = $5, cache_read_input_tokens = $6
              WHERE model_call_id = $7 AND session_id = $8 AND state_kind = 'in_flight'",
        )
        .bind(encode_recommendation(recommendation))
        .bind(decision.approval().rationale().as_str())
        .bind(encoded.input)
        .bind(encoded.output)
        .bind(encoded.cache_creation)
        .bind(encoded.cache_read)
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(rows, "completed judge call")?;
        let outcome = match decision.resolution() {
            Some(resolution) => {
                let (decision_kind, denial_reason) = match resolution.decision() {
                    signalbox_domain::ToolApprovalDecision::Approve => ("approve", None),
                    signalbox_domain::ToolApprovalDecision::Deny { reason } => {
                        ("deny", reason.as_ref().map(|reason| reason.as_str()))
                    }
                };
                sqlx::query(
                    "INSERT INTO tool_approval_decision
                        (request_id, decision_kind, decision_source, denial_reason,
                         delegate_model_selection_id, delegate_model_call_id, rationale)
                     VALUES ($1, $2, 'delegate', $3, $4, $5, $6)",
                )
                .bind(tool_request_id_to_uuid(resolution.request()))
                .bind(decision_kind)
                .bind(denial_reason)
                .bind(prepared.selection.into_uuid())
                .bind(prepared.call.into_uuid())
                .bind(decision.approval().rationale().as_str())
                .execute(&mut *transaction)
                .await?;
                persist_successor_phase(&mut transaction, &decision).await?;
                CompleteApprovalJudgeOutcome::Decided
            }
            None => CompleteApprovalJudgeOutcome::EscalatedToHuman,
        };
        transaction
            .commit()
            .await
            .map_err(ApprovalJudgeRepositoryError::commit)?;
        Ok(outcome)
    }

    /// Records a failed or uncertain call while leaving the request parked.
    pub async fn fail(
        &self,
        prepared: &PreparedApprovalJudge,
        disposition: FailedApprovalJudgeDisposition,
        usage: ProviderReportedTokenUsage,
    ) -> Result<(), ApprovalJudgeRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_session(&mut transaction, prepared.request.session())
            .await
            .map_err(map_model_error)?;
        let state = require_exact_judge(&mut transaction, prepared).await?;
        let encoded = encode_usage(usage);
        if state == "terminal" {
            let exact: bool = sqlx::query_scalar(
                "SELECT terminal_disposition_kind = $1
                        AND recommendation_kind IS NULL AND rationale IS NULL
                        AND input_tokens IS NOT DISTINCT FROM $2
                        AND output_tokens IS NOT DISTINCT FROM $3
                        AND cache_creation_input_tokens IS NOT DISTINCT FROM $4
                        AND cache_read_input_tokens IS NOT DISTINCT FROM $5
                   FROM tool_approval_judge_model_call WHERE model_call_id = $6",
            )
            .bind(disposition.as_str())
            .bind(encoded.input)
            .bind(encoded.output)
            .bind(encoded.cache_creation)
            .bind(encoded.cache_read)
            .bind(prepared.call.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
            transaction.rollback().await?;
            return if exact {
                Ok(())
            } else {
                Err(ApprovalJudgeCorruption::Inconsistent("failed judge replay").into())
            };
        }
        if !matches!(state.as_str(), "prepared" | "in_flight") {
            return Err(ApprovalJudgeCorruption::Inconsistent("judge failure state").into());
        }
        if state == "prepared" && usage != ProviderReportedTokenUsage::unreported() {
            return Err(
                ApprovalJudgeCorruption::Inconsistent("judge usage before authorization").into(),
            );
        }
        let rows = sqlx::query(
            "UPDATE tool_approval_judge_model_call
                SET state_kind = 'terminal', terminal_disposition_kind = $1,
                    input_tokens = $2, output_tokens = $3,
                    cache_creation_input_tokens = $4, cache_read_input_tokens = $5
              WHERE model_call_id = $6 AND session_id = $7
                AND state_kind IN ('prepared', 'in_flight')",
        )
        .bind(disposition.as_str())
        .bind(encoded.input)
        .bind(encoded.output)
        .bind(encoded.cache_creation)
        .bind(encoded.cache_read)
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(rows, "failed judge call")?;
        transaction
            .commit()
            .await
            .map_err(ApprovalJudgeRepositoryError::commit)
    }
}

async fn persist_successor_phase(
    connection: &mut PgConnection,
    decision: &signalbox_domain::PreparedDelegateToolApproval,
) -> Result<(), ApprovalJudgeRepositoryError> {
    match decision.active_phase() {
        ActiveTurnPhase::AwaitingApproval { request } => {
            let rows = sqlx::query(
                "UPDATE turn_lifecycle SET approval_tool_request_id = $1
                  WHERE turn_id = $2 AND session_id = $3 AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND approval_tool_request_id = $4 AND active_tool_round_call_id = $5",
            )
            .bind(tool_request_id_to_uuid(*request))
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(decision.approval().request()))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "next delegated approval wait")?;
        }
        ActiveTurnPhase::Running { current_attempt } => {
            let predecessor: Uuid = sqlx::query_scalar(
                "SELECT turn_attempt_id FROM model_call
                  WHERE model_call_id = $1 AND turn_id = $2 AND session_id = $3",
            )
            .bind(decision.batch().producing_call().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .fetch_one(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind)
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
                    SET active_phase_kind = 'running', current_attempt_id = $1,
                        approval_tool_request_id = NULL
                  WHERE turn_id = $2 AND session_id = $3 AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND current_attempt_id IS NULL AND approval_tool_request_id = $4
                    AND active_tool_round_call_id = $5",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(decision.approval().request()))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "delegated tool execution phase")?;
        }
        ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
            return Err(ApprovalJudgeCorruption::Inconsistent("delegate entered recovery").into());
        }
    }
    Ok(())
}

async fn load_judge(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<sqlx::postgres::PgRow>, ApprovalJudgeRepositoryError> {
    sqlx::query(
        "SELECT model_call_id, direct_model_selection_id,
                resolved_provider_model_identity_id, credential_reference,
                usage_input_includes_cache_tokens, state_kind
           FROM tool_approval_judge_model_call WHERE request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(connection)
    .await
    .map_err(Into::into)
}

fn decode_prepared(
    row: sqlx::postgres::PgRow,
    request: ToolRequest,
) -> Result<PreparedApprovalJudge, ApprovalJudgeRepositoryError> {
    Ok(PreparedApprovalJudge {
        request,
        call: ModelCallId::from_uuid(required(&row, "model_call_id")?),
        selection: DirectModelSelection::from_uuid(required(&row, "direct_model_selection_id")?),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
            &row,
            "resolved_provider_model_identity_id",
        )?)),
        credential_reference: required(&row, "credential_reference")?,
        input_includes_cache_tokens: required(&row, "usage_input_includes_cache_tokens")?,
    })
}

async fn load_producing_selection(
    connection: &mut PgConnection,
    call: ModelCallId,
) -> Result<DirectModelSelection, ApprovalJudgeRepositoryError> {
    let selection: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(direct_model_selection_id, frozen_alias_selected_direct_id)
           FROM model_call WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_optional(connection)
    .await?
    .flatten();
    selection
        .map(DirectModelSelection::from_uuid)
        .ok_or_else(|| ApprovalJudgeCorruption::Missing("producing direct selection").into())
}

async fn require_exact_judge(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
) -> Result<String, ApprovalJudgeRepositoryError> {
    let row = sqlx::query(
        "SELECT request_id, direct_model_selection_id,
                resolved_provider_model_identity_id, credential_reference,
                usage_input_includes_cache_tokens, state_kind
           FROM tool_approval_judge_model_call
          WHERE model_call_id = $1 AND session_id = $2",
    )
    .bind(prepared.call.into_uuid())
    .bind(session_id_to_uuid(prepared.request.session()))
    .fetch_optional(connection)
    .await?
    .ok_or(ApprovalJudgeCorruption::Missing("approval judge call"))?;
    if required::<Uuid>(&row, "request_id")? != tool_request_id_to_uuid(prepared.request.id())
        || required::<Uuid>(&row, "direct_model_selection_id")? != prepared.selection.into_uuid()
        || required::<Uuid>(&row, "resolved_provider_model_identity_id")?
            != prepared.target.identity().into_uuid()
        || required::<String>(&row, "credential_reference")? != prepared.credential_reference
        || required::<bool>(&row, "usage_input_includes_cache_tokens")?
            != prepared.input_includes_cache_tokens
    {
        return Err(ApprovalJudgeCorruption::Inconsistent("approval judge binding").into());
    }
    required(&row, "state_kind")
}

async fn exact_completed(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
    recommendation: DelegateApprovalRecommendation,
    rationale: &ToolDecisionRationale,
    usage: ProviderReportedTokenUsage,
) -> Result<Option<CompleteApprovalJudgeOutcome>, ApprovalJudgeRepositoryError> {
    let encoded = encode_usage(usage);
    let exact: bool = sqlx::query_scalar(
        "SELECT terminal_disposition_kind = 'completed'
                AND recommendation_kind = $1 AND rationale = $2
                AND input_tokens IS NOT DISTINCT FROM $3
                AND output_tokens IS NOT DISTINCT FROM $4
                AND cache_creation_input_tokens IS NOT DISTINCT FROM $5
                AND cache_read_input_tokens IS NOT DISTINCT FROM $6
           FROM tool_approval_judge_model_call WHERE model_call_id = $7",
    )
    .bind(encode_recommendation(recommendation))
    .bind(rationale.as_str())
    .bind(encoded.input)
    .bind(encoded.output)
    .bind(encoded.cache_creation)
    .bind(encoded.cache_read)
    .bind(prepared.call.into_uuid())
    .fetch_one(connection)
    .await?;
    Ok(exact.then_some(match recommendation {
        DelegateApprovalRecommendation::Approve | DelegateApprovalRecommendation::Deny => {
            CompleteApprovalJudgeOutcome::Decided
        }
        DelegateApprovalRecommendation::EscalateToHuman => {
            CompleteApprovalJudgeOutcome::EscalatedToHuman
        }
    }))
}

fn encode_recommendation(recommendation: DelegateApprovalRecommendation) -> &'static str {
    match recommendation {
        DelegateApprovalRecommendation::Approve => "approve",
        DelegateApprovalRecommendation::Deny => "deny",
        DelegateApprovalRecommendation::EscalateToHuman => "escalate_to_human",
    }
}

struct EncodedUsage {
    input: Option<Decimal>,
    output: Option<Decimal>,
    cache_creation: Option<Decimal>,
    cache_read: Option<Decimal>,
}

fn encode_usage(usage: ProviderReportedTokenUsage) -> EncodedUsage {
    EncodedUsage {
        input: usage.input_tokens().map(Decimal::from),
        output: usage.output_tokens().map(Decimal::from),
        cache_creation: usage.cache_creation_input_tokens().map(Decimal::from),
        cache_read: usage.cache_read_input_tokens().map(Decimal::from),
    }
}

fn required<T>(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
) -> Result<T, ApprovalJudgeRepositoryError>
where
    for<'value> T: sqlx::Decode<'value, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or_else(|| ApprovalJudgeCorruption::Missing(column).into())
}

fn require_single(
    rows: u64,
    relationship: &'static str,
) -> Result<(), ApprovalJudgeRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ApprovalJudgeCorruption::Inconsistent(relationship).into())
    }
}

fn classify_insert(error: sqlx::Error) -> ApprovalJudgeRepositoryError {
    if error
        .as_database_error()
        .and_then(|database| database.constraint())
        .is_some_and(|constraint| {
            matches!(
                constraint,
                "tool_approval_judge_model_call_pkey" | "model_call_identity_pkey"
            )
        })
    {
        ApprovalJudgeRepositoryError::IdentityCollision
    } else {
        error.into()
    }
}

fn map_model_error(
    error: crate::model_execution::ModelCallRepositoryError,
) -> ApprovalJudgeRepositoryError {
    match error {
        crate::model_execution::ModelCallRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ApprovalJudgeRepositoryError::Database {
            source,
            commit_ambiguous,
        },
        crate::model_execution::ModelCallRepositoryError::IdentityCollision(_) => {
            ApprovalJudgeRepositoryError::IdentityCollision
        }
        crate::model_execution::ModelCallRepositoryError::Corruption(_)
        | crate::model_execution::ModelCallRepositoryError::NoLiveExecution
        | crate::model_execution::ModelCallRepositoryError::InvalidTransition(_) => {
            ApprovalJudgeCorruption::Inconsistent("model execution dependency").into()
        }
    }
}

fn map_tool_error(error: ToolLoopRepositoryError) -> ApprovalJudgeRepositoryError {
    match error {
        ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ApprovalJudgeRepositoryError::Database {
            source,
            commit_ambiguous,
        },
        ToolLoopRepositoryError::IdentityCollision => {
            ApprovalJudgeRepositoryError::IdentityCollision
        }
        ToolLoopRepositoryError::Corruption(_)
        | ToolLoopRepositoryError::DifferentCommandKind
        | ToolLoopRepositoryError::ConflictingCommandReuse
        | ToolLoopRepositoryError::InvalidTransition(_) => {
            ApprovalJudgeCorruption::Inconsistent("tool loop dependency").into()
        }
    }
}

/// Committed judge facts could not form one exact lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalJudgeCorruption {
    /// Required durable fact was absent.
    Missing(&'static str),
    /// Related lifecycle facts disagreed.
    Inconsistent(&'static str),
    /// A stored state discriminator was unknown.
    UnsupportedState(String),
}

impl fmt::Display for ApprovalJudgeCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("approval judge storage is inconsistent")
    }
}

impl Error for ApprovalJudgeCorruption {}

/// Database, collision, configuration, authority, or corruption failure.
#[derive(Debug)]
pub enum ApprovalJudgeRepositoryError {
    /// PostgreSQL failure with explicit commit ambiguity.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
    /// A daemon-minted call identity collided globally.
    IdentityCollision,
    /// The selected judge model has no configured target.
    TargetUnavailable,
    /// The proposed recommendation exceeded the request's frozen posture.
    AuthorityExceeded,
    /// Durable rows contradicted the closed lifecycle.
    Corruption(ApprovalJudgeCorruption),
}

impl ApprovalJudgeRepositoryError {
    fn commit(error: sqlx::Error) -> Self {
        Self::Database {
            commit_ambiguous: commit_failure_is_ambiguous(&error),
            source: error,
        }
    }
}

impl fmt::Display for ApprovalJudgeRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("approval judge persistence failed")
    }
}

impl Error for ApprovalJudgeRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision | Self::TargetUnavailable | Self::AuthorityExceeded => None,
        }
    }
}

impl ClassifyOperatorFailure for ApprovalJudgeRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::TargetUnavailable | Self::AuthorityExceeded => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }
}

impl From<sqlx::Error> for ApprovalJudgeRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

impl From<ApprovalJudgeCorruption> for ApprovalJudgeRepositoryError {
    fn from(error: ApprovalJudgeCorruption) -> Self {
        Self::Corruption(error)
    }
}
