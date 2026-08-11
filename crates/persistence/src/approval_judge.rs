//! Durable lifecycle for delegated tool-approval judge calls.

use std::{collections::HashSet, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ApprovalJudgeAuthorization, ClassifyOperatorFailure, ModelCallCredentialReference,
    OperatorFailureClass,
};
use signalbox_domain::{
    ActiveTurnPhase, DelegateApprovalRecommendation, DelegateToolApproval, DirectModelSelection,
    FrozenModelSelection, GoalStatement, ModelCallId, ModelTargetCatalog, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SessionId, SessionSystemPrompt,
    SessionTemplateName, ToolApprovalPosture, ToolDecisionRationale, ToolRequest, ToolRequestId,
    TurnAttemptId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    ModelCredentialFamilyCatalog, commit_failure_is_ambiguous,
    goal::{GoalRepositoryError, load_goal_from_connection},
    mapping::{
        ApprovalJudgeStateStorageKind, ApprovalJudgeTerminalDispositionStorageKind,
        ToolApprovalDecisionSourceStorageKind, approval_judge_recommendation_from_str,
        approval_judge_recommendation_to_str, approval_judge_state_from_str,
        approval_judge_state_to_str, approval_judge_terminal_disposition_from_str,
        approval_judge_terminal_disposition_to_str, session_id_to_uuid,
        tool_approval_decision_source_to_str, tool_request_id_to_uuid, turn_id_to_uuid,
    },
    model_execution::{lock_session, resolve_session_credential},
    outbox::{self, OutboxEvent},
    tool_loop::{ToolLoopRepositoryError, load_active_batch_from_connection},
};

/// The session-scoped authority a delegated request was produced under.
///
/// Delegation may only narrow authority, so a judge cannot decide scope
/// without seeing what authority the session was granted. Every field is
/// session-supplied text that untrusted sources may have influenced, so each
/// one is carried as its exact admitted domain value and is never treated as
/// instruction by its consumers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionAuthorityContext {
    goal: Option<GoalStatement>,
    template: Option<SessionTemplateName>,
    system_prompt: Option<SessionSystemPrompt>,
}

impl SessionAuthorityContext {
    /// Composes one context from exactly the values a session recorded.
    #[must_use]
    pub const fn new(
        goal: Option<GoalStatement>,
        template: Option<SessionTemplateName>,
        system_prompt: Option<SessionSystemPrompt>,
    ) -> Self {
        Self {
            goal,
            template,
            system_prompt,
        }
    }

    /// Borrows the statement of the generation the judged turn is bound to.
    #[must_use]
    pub const fn goal(&self) -> Option<&GoalStatement> {
        self.goal.as_ref()
    }

    /// Borrows the template name creation copied into this session.
    #[must_use]
    pub const fn template(&self) -> Option<&SessionTemplateName> {
        self.template.as_ref()
    }

    /// Borrows the system prompt frozen for the judged request's turn.
    #[must_use]
    pub const fn system_prompt(&self) -> Option<&SessionSystemPrompt> {
        self.system_prompt.as_ref()
    }
}

/// Exact durable facts committed before approval-judge provider preparation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedApprovalJudge {
    request: ToolRequest,
    call: ModelCallId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential_reference: String,
    input_includes_cache_tokens: bool,
    session_context: SessionAuthorityContext,
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

    /// Borrows the session authority this request was produced under.
    ///
    /// The context is read fresh on every preparation and is deliberately
    /// absent from the durable judge binding, so it never participates in the
    /// exact-call recheck that guards authorization and completion.
    pub const fn session_context(&self) -> &SessionAuthorityContext {
        &self.session_context
    }
}

/// Non-cloneable proof that one exact judge call committed `InFlight`.
#[derive(Debug, Eq, PartialEq)]
pub struct AuthorizedApprovalJudge {
    prepared: PreparedApprovalJudge,
}

impl ApprovalJudgeAuthorization for AuthorizedApprovalJudge {
    fn request(&self) -> &ToolRequest {
        &self.prepared.request
    }

    fn call(&self) -> ModelCallId {
        self.prepared.call
    }

    fn selection(&self) -> DirectModelSelection {
        self.prepared.selection
    }

    fn target(&self) -> ResolvedProviderTarget {
        self.prepared.target
    }

    fn credential_reference(&self) -> &str {
        &self.prepared.credential_reference
    }
}

/// Result of freshly rechecking one judge-call authorization hint.
#[derive(Debug, Eq, PartialEq)]
pub enum AuthorizeApprovalJudgeOutcome {
    /// The call was already authorized or terminal; no provider send may begin.
    NoSend,
    /// This transaction committed the exact `Prepared -> InFlight` transition.
    Authorized(Box<AuthorizedApprovalJudge>),
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
        let session_context =
            load_session_authority_context(&mut transaction, session, turn).await?;
        if let Some(row) = load_judge(&mut transaction, request.id()).await? {
            let state = decode_state(required(&row, "state_kind")?)?;
            let prepared = decode_prepared(row, request.clone(), session_context)?;
            transaction.rollback().await?;
            return match state {
                ApprovalJudgeStateStorageKind::Prepared => {
                    Ok(PrepareApprovalJudgeOutcome::Ready(Box::new(prepared)))
                }
                ApprovalJudgeStateStorageKind::InFlight => Ok(
                    PrepareApprovalJudgeOutcome::InFlightAfterRestart(Box::new(prepared)),
                ),
                ApprovalJudgeStateStorageKind::Terminal => Ok(PrepareApprovalJudgeOutcome::NoWork),
            };
        }
        let producing_model =
            load_producing_model(&mut transaction, batch.producing_call()).await?;
        let (selection, target) = match configured_selection {
            Some(selection) => {
                let resolved = self
                    .targets
                    .resolve(FrozenModelSelection::Direct(selection))
                    .map_err(|_| ApprovalJudgeRepositoryError::TargetUnavailable)?;
                (selection, resolved.target())
            }
            None => (producing_model.selection, producing_model.target),
        };
        let credential = if configured_selection.is_none()
            && self
                .credential_families
                .as_ref()
                .is_some_and(|families| families.family(target).is_none())
        {
            producing_model.credential
        } else {
            resolve_session_credential(
                &mut transaction,
                session,
                target,
                &self.fallback_credential,
                self.credential_families.as_ref(),
            )
            .await
            .map_err(map_model_error)?
        };
        let input_includes_cache_tokens = self.cache_inclusive_input_targets.contains(&target);
        let inserted = sqlx::query(
            "INSERT INTO tool_approval_judge_model_call
                (model_call_id, request_id, session_id, turn_id,
                 direct_model_selection_id, resolved_provider_model_identity_id,
                 credential_reference, usage_input_includes_cache_tokens, state_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(call.into_uuid())
        .bind(tool_request_id_to_uuid(request.id()))
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .bind(selection.into_uuid())
        .bind(target.identity().into_uuid())
        .bind(credential.as_str())
        .bind(input_includes_cache_tokens)
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Prepared,
        ))
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
            session_context,
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
    ) -> Result<AuthorizeApprovalJudgeOutcome, ApprovalJudgeRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_session(&mut transaction, prepared.request.session())
            .await
            .map_err(map_model_error)?;
        let state = require_exact_judge(&mut transaction, prepared).await?;
        if matches!(
            state,
            ApprovalJudgeStateStorageKind::InFlight | ApprovalJudgeStateStorageKind::Terminal
        ) {
            transaction.rollback().await?;
            return Ok(AuthorizeApprovalJudgeOutcome::NoSend);
        }
        if state != ApprovalJudgeStateStorageKind::Prepared {
            return Err(ApprovalJudgeCorruption::Inconsistent("judge authorization state").into());
        }
        let rows = sqlx::query(
            "UPDATE tool_approval_judge_model_call SET state_kind = $1
              WHERE model_call_id = $2 AND session_id = $3 AND state_kind = $4",
        )
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::InFlight,
        ))
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Prepared,
        ))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        require_single(rows, "judge authorization")?;
        transaction
            .commit()
            .await
            .map_err(ApprovalJudgeRepositoryError::commit)?;
        Ok(AuthorizeApprovalJudgeOutcome::Authorized(Box::new(
            AuthorizedApprovalJudge {
                prepared: prepared.clone(),
            },
        )))
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
        if state == ApprovalJudgeStateStorageKind::Terminal {
            let outcome = exact_completed(
                &mut transaction,
                prepared,
                recommendation,
                &rationale,
                usage,
                continuation_attempt,
            )
            .await?;
            transaction.rollback().await?;
            return outcome.ok_or_else(|| {
                ApprovalJudgeCorruption::Inconsistent("completed judge replay").into()
            });
        }
        if state != ApprovalJudgeStateStorageKind::InFlight {
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
                SET state_kind = $1, terminal_disposition_kind = $2,
                    recommendation_kind = $3, rationale = $4,
                    input_tokens = $5, output_tokens = $6,
                    cache_creation_input_tokens = $7, cache_read_input_tokens = $8
              WHERE model_call_id = $9 AND session_id = $10 AND state_kind = $11",
        )
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Terminal,
        ))
        .bind(approval_judge_terminal_disposition_to_str(
            ApprovalJudgeTerminalDispositionStorageKind::Completed,
        ))
        .bind(approval_judge_recommendation_to_str(recommendation))
        .bind(decision.approval().rationale().as_str())
        .bind(encoded.input)
        .bind(encoded.output)
        .bind(encoded.cache_creation)
        .bind(encoded.cache_read)
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::InFlight,
        ))
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
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(tool_request_id_to_uuid(resolution.request()))
                .bind(decision_kind)
                .bind(tool_approval_decision_source_to_str(
                    ToolApprovalDecisionSourceStorageKind::Delegate,
                ))
                .bind(denial_reason)
                .bind(prepared.selection.into_uuid())
                .bind(prepared.call.into_uuid())
                .bind(decision.approval().rationale().as_str())
                .execute(&mut *transaction)
                .await?;
                persist_successor_phase(&mut transaction, &decision).await?;
                outbox::append(
                    &mut transaction,
                    OutboxEvent::ToolApprovalDecided {
                        session: prepared.request.session(),
                        turn: prepared.request.turn(),
                        request: resolution.request(),
                    },
                )
                .await?;
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
        if state == ApprovalJudgeStateStorageKind::Terminal {
            let exact: bool = sqlx::query_scalar(
                "SELECT terminal_disposition_kind = $1
                        AND recommendation_kind IS NULL AND rationale IS NULL
                        AND input_tokens IS NOT DISTINCT FROM $2
                        AND output_tokens IS NOT DISTINCT FROM $3
                        AND cache_creation_input_tokens IS NOT DISTINCT FROM $4
                        AND cache_read_input_tokens IS NOT DISTINCT FROM $5
                   FROM tool_approval_judge_model_call WHERE model_call_id = $6",
            )
            .bind(approval_judge_terminal_disposition_to_str(
                ApprovalJudgeTerminalDispositionStorageKind::Failed(disposition),
            ))
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
        if !matches!(
            state,
            ApprovalJudgeStateStorageKind::Prepared | ApprovalJudgeStateStorageKind::InFlight
        ) {
            return Err(ApprovalJudgeCorruption::Inconsistent("judge failure state").into());
        }
        if state == ApprovalJudgeStateStorageKind::Prepared
            && usage != ProviderReportedTokenUsage::unreported()
        {
            return Err(
                ApprovalJudgeCorruption::Inconsistent("judge usage before authorization").into(),
            );
        }
        let rows = sqlx::query(
            "UPDATE tool_approval_judge_model_call
                SET state_kind = $1, terminal_disposition_kind = $2,
                    input_tokens = $3, output_tokens = $4,
                    cache_creation_input_tokens = $5, cache_read_input_tokens = $6
              WHERE model_call_id = $7 AND session_id = $8
                AND (state_kind = $9 OR state_kind = $10)",
        )
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Terminal,
        ))
        .bind(approval_judge_terminal_disposition_to_str(
            ApprovalJudgeTerminalDispositionStorageKind::Failed(disposition),
        ))
        .bind(encoded.input)
        .bind(encoded.output)
        .bind(encoded.cache_creation)
        .bind(encoded.cache_read)
        .bind(prepared.call.into_uuid())
        .bind(session_id_to_uuid(prepared.request.session()))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::Prepared,
        ))
        .bind(approval_judge_state_to_str(
            ApprovalJudgeStateStorageKind::InFlight,
        ))
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
            .await
            .map_err(classify_insert)?;
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
        ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
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

/// Reads the session authority in force for one judged request's turn.
///
/// The system prompt is the epoch the turn froze rather than the session's
/// current epoch, so a defaults replacement racing the judge cannot present an
/// authority the request was never produced under. The shared configuration
/// resolver supplies that epoch for every origin kind, including a
/// delegation-origin turn that owns no `queued_input_origin` row. A turn that
/// produced a model call always resolves one epoch, so absence is corruption.
async fn load_session_authority_context(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<SessionAuthorityContext, ApprovalJudgeRepositoryError> {
    let row = sqlx::query(
        "SELECT s.template_name, frozen.system_prompt
           FROM session AS s
           JOIN session_defaults_version AS frozen
             ON frozen.session_id = s.session_id
            AND frozen.version = (
                 SELECT configuration.defaults_version
                   FROM turn_origin_exact_model_configuration($2, $1)
                        AS configuration
                )
          WHERE s.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ApprovalJudgeCorruption::Missing(
        "judged turn defaults epoch",
    ))?;
    let template = row
        .try_get::<Option<String>, _>("template_name")?
        .map(SessionTemplateName::try_new)
        .transpose()
        .map_err(|_| ApprovalJudgeCorruption::Inconsistent("session template admission"))?;
    let system_prompt = row
        .try_get::<Option<String>, _>("system_prompt")?
        .map(SessionSystemPrompt::try_new)
        .transpose()
        .map_err(|_| ApprovalJudgeCorruption::Inconsistent("system prompt admission"))?;
    let goal = load_judged_turn_goal(&mut *connection, session, turn).await?;
    Ok(SessionAuthorityContext::new(goal, template, system_prompt))
}

/// Resolves the goal statement the judged turn was actually produced under.
///
/// A pursuing generation may be superseded while its already-active turn is
/// parked for delegated approval. Reading the session's current generation
/// would then show the judge a broadened replacement and let it authorize a
/// request the originating goal never covered, defeating the narrow-only
/// guarantee. The generation recorded for the turn is the only sound binding,
/// and a turn that no goal drives carries no goal authority to show.
async fn load_judged_turn_goal(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<GoalStatement>, ApprovalJudgeRepositoryError> {
    let Some(generation) = sqlx::query_scalar::<_, Decimal>(
        "SELECT goal_generation
           FROM goal_turn
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let goal = load_goal_from_connection(&mut *connection, session)
        .await
        .map_err(map_goal_error)?
        .ok_or(ApprovalJudgeCorruption::Missing("judged turn goal"))?;
    goal.generations()
        .iter()
        .find(|snapshot| Decimal::from(snapshot.generation().get()) == generation)
        .map(|snapshot| snapshot.statement().clone())
        .ok_or_else(|| ApprovalJudgeCorruption::Inconsistent("judged turn goal generation").into())
        .map(Some)
}

fn decode_prepared(
    row: sqlx::postgres::PgRow,
    request: ToolRequest,
    session_context: SessionAuthorityContext,
) -> Result<PreparedApprovalJudge, ApprovalJudgeRepositoryError> {
    Ok(PreparedApprovalJudge {
        request,
        session_context,
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

struct ProducingModel {
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential: ModelCallCredentialReference,
}

async fn load_producing_model(
    connection: &mut PgConnection,
    call: ModelCallId,
) -> Result<ProducingModel, ApprovalJudgeRepositoryError> {
    let row = sqlx::query(
        "SELECT COALESCE(direct_model_selection_id, frozen_alias_selected_direct_id)
                    AS direct_model_selection_id,
                resolved_provider_model_identity_id, credential_reference
           FROM model_call WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_optional(connection)
    .await?
    .ok_or(ApprovalJudgeCorruption::Missing("producing model call"))?;
    Ok(ProducingModel {
        selection: DirectModelSelection::from_uuid(required(&row, "direct_model_selection_id")?),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
            &row,
            "resolved_provider_model_identity_id",
        )?)),
        credential: ModelCallCredentialReference::new(required::<String>(
            &row,
            "credential_reference",
        )?),
    })
}

async fn require_exact_judge(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
) -> Result<ApprovalJudgeStateStorageKind, ApprovalJudgeRepositoryError> {
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
    decode_state(required(&row, "state_kind")?)
}

fn decode_state(
    value: String,
) -> Result<ApprovalJudgeStateStorageKind, ApprovalJudgeRepositoryError> {
    approval_judge_state_from_str(&value)
        .ok_or_else(|| ApprovalJudgeCorruption::UnsupportedState(value).into())
}

async fn exact_completed(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
    recommendation: DelegateApprovalRecommendation,
    rationale: &ToolDecisionRationale,
    usage: ProviderReportedTokenUsage,
    continuation_attempt: TurnAttemptId,
) -> Result<Option<CompleteApprovalJudgeOutcome>, ApprovalJudgeRepositoryError> {
    let encoded = encode_usage(usage);
    let row = sqlx::query(
        "SELECT terminal_disposition_kind, recommendation_kind, rationale,
                input_tokens, output_tokens, cache_creation_input_tokens,
                cache_read_input_tokens
           FROM tool_approval_judge_model_call WHERE model_call_id = $1",
    )
    .bind(prepared.call.into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    let terminal_disposition: String = required(&row, "terminal_disposition_kind")?;
    let stored_recommendation: String = required(&row, "recommendation_kind")?;
    let exact = approval_judge_terminal_disposition_from_str(&terminal_disposition)
        == Some(ApprovalJudgeTerminalDispositionStorageKind::Completed)
        && approval_judge_recommendation_from_str(&stored_recommendation) == Some(recommendation)
        && required::<String>(&row, "rationale")? == rationale.as_str()
        && row.try_get::<Option<Decimal>, _>("input_tokens")? == encoded.input
        && row.try_get::<Option<Decimal>, _>("output_tokens")? == encoded.output
        && row.try_get::<Option<Decimal>, _>("cache_creation_input_tokens")?
            == encoded.cache_creation
        && row.try_get::<Option<Decimal>, _>("cache_read_input_tokens")? == encoded.cache_read;
    if !exact {
        return Ok(None);
    }
    let continuation_exact = recommendation == DelegateApprovalRecommendation::EscalateToHuman
        || exact_completion_continuation(connection, prepared, continuation_attempt).await?;
    Ok(continuation_exact.then_some(match recommendation {
        DelegateApprovalRecommendation::Approve | DelegateApprovalRecommendation::Deny => {
            CompleteApprovalJudgeOutcome::Decided
        }
        DelegateApprovalRecommendation::EscalateToHuman => {
            CompleteApprovalJudgeOutcome::EscalatedToHuman
        }
    }))
}

async fn exact_completion_continuation(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
    continuation_attempt: TurnAttemptId,
) -> Result<bool, ApprovalJudgeRepositoryError> {
    let completion_was_nonfinal: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM tool_request AS later
            LEFT JOIN tool_approval_decision AS decision
              ON decision.request_id = later.request_id
           WHERE later.producing_model_call_id = $1
             AND later.request_ordinal > $2
             AND (
                 decision.request_id IS NULL
                 OR decision.decision_source NOT IN ('policy_auto', 'session_blanket')
             )
        )",
    )
    .bind(prepared.request.producing_call().into_uuid())
    .bind(Decimal::from(prepared.request.ordinal().as_u32()))
    .fetch_one(&mut *connection)
    .await?;
    if completion_was_nonfinal {
        return Ok(true);
    }
    let persisted_continuation: Option<Uuid> = sqlx::query_scalar(
        "SELECT continuation.turn_attempt_id
           FROM model_call AS producing
           JOIN turn_attempt AS continuation
             ON continuation.continued_from_attempt_id = producing.turn_attempt_id
          WHERE producing.model_call_id = $1",
    )
    .bind(prepared.request.producing_call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    Ok(persisted_continuation == Some(continuation_attempt.into_uuid()))
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
                "tool_approval_judge_model_call_pkey"
                    | "model_call_identity_pkey"
                    | "turn_attempt_pkey"
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

fn map_goal_error(error: GoalRepositoryError) -> ApprovalJudgeRepositoryError {
    match error {
        GoalRepositoryError::Database(source) => ApprovalJudgeRepositoryError::Database {
            source,
            commit_ambiguous: false,
        },
        GoalRepositoryError::CommitAmbiguous(source) => ApprovalJudgeRepositoryError::Database {
            source,
            commit_ambiguous: true,
        },
        GoalRepositoryError::Corruption(_) | GoalRepositoryError::DifferentCommandKind { .. } => {
            ApprovalJudgeCorruption::Inconsistent("goal dependency").into()
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
        match self {
            Self::Missing(relationship) => {
                write!(
                    formatter,
                    "approval judge storage is missing {relationship}"
                )
            }
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "approval judge storage has inconsistent {relationship}"
                )
            }
            Self::UnsupportedState(discriminator) => write!(
                formatter,
                "approval judge storage has unsupported state {discriminator}"
            ),
        }
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
        match self {
            Self::Database {
                commit_ambiguous: true,
                ..
            } => formatter.write_str("approval judge database commit outcome is ambiguous"),
            Self::Database {
                commit_ambiguous: false,
                ..
            } => formatter.write_str("approval judge database operation failed"),
            Self::IdentityCollision => {
                formatter.write_str("approval judge identity collided with durable state")
            }
            Self::TargetUnavailable => {
                formatter.write_str("approval judge model target is unavailable")
            }
            Self::AuthorityExceeded => {
                formatter.write_str("approval judge recommendation exceeded delegated authority")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
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

#[cfg(test)]
mod tests {
    use super::ApprovalJudgeRepositoryError;

    #[test]
    fn repository_errors_display_distinct_failure_classes() {
        assert_eq!(
            ApprovalJudgeRepositoryError::IdentityCollision.to_string(),
            "approval judge identity collided with durable state"
        );
        assert_eq!(
            ApprovalJudgeRepositoryError::TargetUnavailable.to_string(),
            "approval judge model target is unavailable"
        );
        assert_eq!(
            ApprovalJudgeRepositoryError::AuthorityExceeded.to_string(),
            "approval judge recommendation exceeded delegated authority"
        );
    }
}
