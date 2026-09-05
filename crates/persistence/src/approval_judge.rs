//! Durable lifecycle for delegated tool-approval judge calls.

use std::{collections::HashSet, error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    ApprovalJudgeAuthorization, ApprovalJudgeBranchAuthority, ApprovalJudgeBranchAuthorityInput,
    ApprovalJudgeCompletionIdentities, ApprovalJudgeDispatchAuthority,
    ApprovalJudgeDispatchProvenance, ApprovalJudgePullRequestAuthority,
    ApprovalJudgePullRequestAuthorityInput, ClassifyOperatorFailure, ModelCallCredentialReference,
    OperatorFailureClass,
};
use signalbox_domain::{
    ActiveTurnPhase, BranchName, CommissionedDispatchId, CommitSha, ContextFrontierId,
    DelegateApprovalRecommendation, DelegateToolApproval, DirectModelSelection,
    FrozenModelSelection, GoalGeneration, GoalGenerationSnapshot, GoalNeed,
    GoalSchedulerProvenance, GoalStatement, ModelCallId, ModelTargetCatalog, ProviderModelIdentity,
    ProviderReportedTokenUsage, PullRequestNumber, RepositorySlug, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId, SessionSystemPrompt,
    SessionTemplateName, ToolApprovalPosture, ToolDecisionRationale, ToolRequest, ToolRequestId,
    TurnAttemptId, TurnId, TurnTerminalCause,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    ModelCredentialFamilyCatalog, commit_failure_is_ambiguous,
    goal::{self, GoalRepositoryError, GoalTransitionOutcome, load_goal_from_connection},
    mapping::{
        ApprovalJudgeStateStorageKind, ApprovalJudgeTerminalDispositionStorageKind,
        ToolApprovalDecisionSourceStorageKind, approval_judge_recommendation_from_str,
        approval_judge_recommendation_to_str, approval_judge_state_from_str,
        approval_judge_state_to_str, approval_judge_terminal_disposition_from_str,
        approval_judge_terminal_disposition_to_str, positive_u64_from_numeric, session_id_to_uuid,
        tool_approval_decision_source_to_str, tool_request_id_to_uuid, turn_id_to_uuid,
        turn_terminal_cause_to_str,
    },
    model_execution::{lock_session, resolve_session_credential},
    outbox::{self, OutboxEvent, TurnTerminalOutboxDisposition},
    tool_loop::{ToolLoopRepositoryError, load_active_batch_from_connection},
};

/// The session-scoped authority a delegated request was produced under.
///
/// Delegation may only narrow authority, so a judge cannot decide scope
/// without seeing what authority the session was granted. Every field is
/// session or repository-watch state that untrusted sources may have
/// influenced, so each one is carried as its exact admitted domain value and
/// is never treated as instruction by its consumers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionAuthorityContext {
    goal: Option<GoalStatement>,
    template: Option<SessionTemplateName>,
    system_prompt: Option<SessionSystemPrompt>,
    dispatch: Option<ApprovalJudgeDispatchAuthority>,
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
            dispatch: None,
        }
    }

    /// Attaches the immutable repository-watch fence for a dispatched session.
    #[must_use]
    pub fn with_dispatch(mut self, dispatch: ApprovalJudgeDispatchAuthority) -> Self {
        self.dispatch = Some(dispatch);
        self
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

    /// Borrows the append-only repository-watch fence when dispatch created the session.
    #[must_use]
    pub const fn dispatch(&self) -> Option<&ApprovalJudgeDispatchAuthority> {
        self.dispatch.as_ref()
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
    /// exact-call recheck that guards authorization and completion. Completion
    /// does compare the goal it carries against the statement in force at that
    /// moment, but by resolving that statement again rather than by binding
    /// this value durably.
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
    /// An unattended turn was terminalized and audited for its dispatch.
    ///
    /// The owning dispatch module records its command settlement separately.
    HeadlessEscalationTerminalized,
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
                signalbox_domain::FastMode::Disabled,
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
    pub async fn complete<NextClosedResultEntry>(
        &self,
        prepared: &PreparedApprovalJudge,
        recommendation: DelegateApprovalRecommendation,
        rationale: ToolDecisionRationale,
        usage: ProviderReportedTokenUsage,
        identities: ApprovalJudgeCompletionIdentities,
        mut next_closed_result_entry: NextClosedResultEntry,
    ) -> Result<CompleteApprovalJudgeOutcome, ApprovalJudgeRepositoryError>
    where
        NextClosedResultEntry: FnMut(ToolRequestId) -> SemanticTranscriptEntryId,
    {
        let mut transaction = self.pool.begin().await?;
        // Completion rechecks the goal authority in force, and goal system
        // transitions serialize on the session row without ever taking the
        // scheduler row (`goal::handle_system_transition`), so excluding a
        // concurrent `declare_achieved` requires holding the session row
        // from before that read until this commit. It is taken before the
        // scheduler lock below because every transaction locking both rows
        // acquires the session row first (see `lock_inventory`); acquiring
        // it after would deadlock against every applied goal command.
        if !goal::lock_session(&mut transaction, prepared.request.session()).await? {
            return Err(ApprovalJudgeCorruption::Missing("judge completion session").into());
        }
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
                identities,
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
        // Resolved under the completion lock rather than trusted from the
        // prepared binding: the session was unlocked for the whole provider
        // round-trip, which is exactly when a user stopping the goal lands.
        // This runs only for a completion that has not committed yet, so a
        // replay never recomputes a decision against authority that moved after
        // the decision was already durable.
        let in_force = load_judged_turn_authority_in_force(
            &mut transaction,
            prepared.request.session(),
            prepared.request.turn(),
        )
        .await?;
        let authority_stands = read_authority_still_stands(JudgedTurnAuthority {
            read: prepared.session_context.goal(),
            in_force: in_force.as_ref(),
        });
        let recommendation = if authority_stands {
            recommendation
        } else {
            DelegateApprovalRecommendation::EscalateToHuman
        };
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
            .then_some(identities.continuation_attempt());
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
            None => {
                if unattended_escalation_applies(&mut transaction, prepared, authority_stands)
                    .await?
                {
                    persist_headless_escalation(
                        &mut transaction,
                        prepared,
                        decision.batch(),
                        identities,
                        authority_stands,
                        &mut next_closed_result_entry,
                    )
                    .await?;
                    CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized
                } else {
                    CompleteApprovalJudgeOutcome::EscalatedToHuman
                }
            }
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

/// Need text for the execution-failure block an unattended escalation appends.
const HEADLESS_ESCALATION_GOAL_NEED: &str = "A delegated tool approval escalated with no attending user, so this goal turn was failed. No automatic resumption is scheduled. Resume this goal only to continue this session by hand: a further escalation on work you resumed waits for you instead of failing the turn again.";

/// The generation a repository-watch dispatch commissions in the session it
/// creates, which is the only generation its authority describes.
///
/// The dispatch creates the session and commissions its goal in one
/// transaction, so the commission is that session's first generation. Repository
/// watch identifies the commission it owns through the same provenance.
const DISPATCH_COMMISSIONED_GENERATION: GoalGeneration = GoalGeneration::new(NonZeroU64::MIN);

/// Whether this escalation takes the unattended path rather than parking.
///
/// Three conditions, each answering a different question about whether a user
/// is there and whether the path has anything left to do.
///
/// Without dispatch authority the session is an ordinary one, and the ordinary
/// park is what its escalation gets. A steer accepted while this turn awaited
/// its judge is a user attending the session, and is also the one shape the
/// unattended path cannot durably take: terminalizing a turn a
/// `pending_steering` input still names violates
/// `turn_lifecycle_pending_steering_closed` and would fail the whole
/// completion, leaving the request parked and the judge call in flight, while
/// reclassifying the steer into a queued successor would start fresh work in a
/// session whose dispatch is being released for redispatch.
///
/// Work a repository-watch session has already escalated once is the third.
/// Its exceptional block is never resumed automatically, so a later turn in
/// that session is work an operator resumed and waits for them. An
/// operator-commissioned session is attended by the commissioning operator:
/// its completed delegate escalation is the bounded automatic decision's
/// exhaustion point and parks the exact request for that operator instead of
/// spending a goal retry on the same undecided action.
///
/// Standing authority is the last word on it. Withdrawn authority means the
/// goal ended while this judge was in flight, so nobody is behind the work
/// after all and it is terminalized rather than parked for a user who will
/// never come. A turn no escalation preceded is the dispatched work itself,
/// including one an ordinary execution failure had automatically resumed, and
/// stays unattended.
///
/// [`goal mode`]: ../../../docs/spec/goal-mode.md
async fn unattended_escalation_applies(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
    authority_stands: bool,
) -> Result<bool, ApprovalJudgeRepositoryError> {
    let Some(dispatch) = prepared.session_context.dispatch() else {
        return Ok(false);
    };
    if turn_awaits_pending_steering(
        connection,
        prepared.request.session(),
        prepared.request.turn(),
    )
    .await?
    {
        return Ok(false);
    }
    match dispatch.dispatch() {
        ApprovalJudgeDispatchProvenance::Commissioned(_) => Ok(!authority_stands),
        ApprovalJudgeDispatchProvenance::RepoWatch(_) => Ok(false),
    }
}

/// Whether a `pending_steering` accepted input still names this turn.
///
/// The session row is already held, so the answer cannot change under the
/// caller: accepting pending steering locks the same row to check that its
/// source turn is active.
async fn turn_awaits_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<bool, ApprovalJudgeRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM accepted_input
             WHERE session_id = $1
               AND expected_active_turn_id = $2
               AND disposition_kind = 'pending_steering'
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_one(&mut *connection)
    .await
    .map_err(Into::into)
}

async fn persist_headless_escalation(
    connection: &mut PgConnection,
    prepared: &PreparedApprovalJudge,
    batch: &signalbox_domain::ToolBatch,
    identities: ApprovalJudgeCompletionIdentities,
    authority_stands: bool,
    next_closed_result_entry: &mut impl FnMut(ToolRequestId) -> SemanticTranscriptEntryId,
) -> Result<(), ApprovalJudgeRepositoryError> {
    let session = prepared.request.session();
    let turn = prepared.request.turn();
    let dispatch = prepared
        .session_context
        .dispatch()
        .map(ApprovalJudgeDispatchAuthority::dispatch)
        .ok_or(ApprovalJudgeCorruption::Missing(
            "headless dispatch authority",
        ))?;
    let predecessor_attempt: Uuid = sqlx::query_scalar(
        "SELECT turn_attempt_id FROM model_call
          WHERE model_call_id = $1 AND session_id = $2 AND turn_id = $3",
    )
    .bind(batch.producing_call().into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ApprovalJudgeCorruption::Missing(
        "headless escalation predecessor attempt",
    ))?;
    let attempt = identities.continuation_attempt().into_uuid();
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind)
         VALUES ($1, $2, $3, $4, 'prepared')",
    )
    .bind(attempt)
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(predecessor_attempt)
    .execute(&mut *connection)
    .await
    .map_err(classify_insert)?;
    let ended = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3
            AND state_kind = 'prepared'",
    )
    .bind(attempt)
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(ended, "headless escalation terminal attempt")?;

    let mut terminal_results = Vec::with_capacity(batch.requests().len());
    for request in batch.requests() {
        let rows = sqlx::query(
            "SELECT entry.source_session_id, entry.semantic_entry_id
               FROM semantic_transcript_entry AS entry
               LEFT JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = entry.tool_result_attempt_id
              WHERE entry.source_session_id = $1
                AND entry.payload_kind IN (
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end'
                )
                AND (entry.tool_result_request_id = $2 OR attempt.request_id = $2)",
        )
        .bind(session_id_to_uuid(session))
        .bind(tool_request_id_to_uuid(request.id()))
        .fetch_all(&mut *connection)
        .await?;
        let result = match rows.as_slice() {
            [] => {
                let entry = next_closed_result_entry(request.id());
                let decision_kind: Option<String> = sqlx::query_scalar(
                    "SELECT decision_kind FROM tool_approval_decision WHERE request_id = $1",
                )
                .bind(tool_request_id_to_uuid(request.id()))
                .fetch_optional(&mut *connection)
                .await?;
                let payload_kind = if decision_kind.as_deref() == Some("deny") {
                    "tool_denied"
                } else {
                    "tool_closed_by_turn_end"
                };
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         tool_result_request_id)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(session_id_to_uuid(session))
                .bind(entry.into_uuid())
                .bind(payload_kind)
                .bind(tool_request_id_to_uuid(request.id()))
                .execute(&mut *connection)
                .await
                .map_err(classify_insert)?;
                SemanticTranscriptEntryRef::from_source(session, entry)
            }
            [row] => SemanticTranscriptEntryRef::from_source(
                SessionId::from_uuid(row.try_get("source_session_id")?),
                SemanticTranscriptEntryId::from_uuid(row.try_get("semantic_entry_id")?),
            ),
            _ => {
                return Err(ApprovalJudgeCorruption::Inconsistent(
                    "headless escalation tool result",
                )
                .into());
            }
        };
        terminal_results.push(result);
    }

    let failure_entry = identities.failure_entry();
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', $3)",
    )
    .bind(session_id_to_uuid(session))
    .bind(failure_entry.into_uuid())
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await
    .map_err(classify_insert)?;
    let prefix = batch.yielded_snapshot().frontier().snapshot();
    let prefix_member_count = u64::try_from(batch.yielded_snapshot().entry_count())
        .map_err(|_| ApprovalJudgeCorruption::Inconsistent("headless frontier member count"))?;
    let appended_member_count = u64::try_from(terminal_results.len())
        .map_err(|_| ApprovalJudgeCorruption::Inconsistent("headless result member count"))?
        .checked_add(1)
        .ok_or(ApprovalJudgeCorruption::Inconsistent(
            "headless frontier member count",
        ))?;
    let member_count = prefix_member_count
        .checked_add(appended_member_count)
        .ok_or(ApprovalJudgeCorruption::Inconsistent(
            "headless frontier member count",
        ))?;
    terminal_results.push(SemanticTranscriptEntryRef::from_source(
        session,
        failure_entry,
    ));
    crate::model_execution::insert_snapshot_append(
        connection,
        crate::model_execution::SnapshotAppend {
            owning_session: session,
            frontier: identities.terminal_frontier(),
            prefix: Some(prefix),
            member_count,
            prefix_member_count,
            appended_entries: terminal_results,
        },
    )
    .await
    .map_err(map_snapshot_append_error)?;
    let terminalized = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', terminal_frontier_id = $1,
                active_phase_kind = NULL, current_attempt_id = NULL,
                recovery_model_call_id = NULL, active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL, child_wait_request_id = NULL,
                recovery_tool_attempt_id = NULL, runner_recovery_runner_id = NULL,
                runner_recovery_placement_revision = NULL,
                runner_recovery_tool_attempt_id = NULL, terminal_attempt_id = $2,
                terminal_model_call_id = NULL, terminal_tool_attempt_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = $7
          WHERE turn_id = $3 AND session_id = $4 AND state_kind = 'active'
            AND active_phase_kind = 'awaiting_tool_approval'
            AND active_tool_round_call_id = $5 AND approval_tool_request_id = $6",
    )
    .bind(identities.terminal_frontier().into_uuid())
    .bind(attempt)
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(batch.producing_call().into_uuid())
    .bind(tool_request_id_to_uuid(prepared.request.id()))
    .bind(turn_terminal_cause_to_str(
        TurnTerminalCause::HeadlessApprovalEscalation,
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(terminalized, "headless escalation turn terminalization")?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session,
            turn,
            disposition: TurnTerminalOutboxDisposition::Failed {
                failure_entry,
                terminal_frontier: identities.terminal_frontier(),
            },
        },
    )
    .await?;
    let audited = match dispatch {
        ApprovalJudgeDispatchProvenance::RepoWatch(_) => {
            return Err(ApprovalJudgeCorruption::Inconsistent(
                "retired repository-watch dispatch authority",
            )
            .into());
        }
        ApprovalJudgeDispatchProvenance::Commissioned(dispatch) => sqlx::query(
            "INSERT INTO commissioned_dispatch_headless_approval_escalation
                    (model_call_id, request_id, dispatch_id, session_id, turn_id,
                     terminal_attempt_id, failure_entry_id, terminal_frontier_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(prepared.call.into_uuid())
        .bind(tool_request_id_to_uuid(prepared.request.id()))
        .bind(dispatch.as_uuid())
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .bind(attempt)
        .bind(failure_entry.into_uuid())
        .bind(identities.terminal_frontier().into_uuid())
        .execute(&mut *connection)
        .await
        .map_err(classify_insert)?
        .rows_affected(),
    };
    require_single(audited, "headless escalation audit")?;

    if authority_stands && matches!(dispatch, ApprovalJudgeDispatchProvenance::RepoWatch(_)) {
        // Deliberately not routed through `PostgresGoalPassDisposition`, which
        // owns the bounded automatic resumption every other execution-failure
        // block receives (`docs/spec/goal-mode.md`). Two reasons, both stated
        // by that page's repository-watch exception. It must commit inside this
        // transaction, atomically with the terminalization, the audit row, and
        // the release attempt below; and the retry this failure is owed already
        // exists and is a different one — repository watch redispatches the
        // work under a fresh dispatch, so resuming the goal here would re-run
        // the same escalating turn against a request no user is attending, up
        // to the resumption budget, beside that redispatch. Where that
        // redispatch is withheld, because the rule was deactivated or the pull
        // request closed, the work is not wanted at all and resuming it is
        // worse still. The need text above therefore names the repair itself
        // rather than promising resumption. A commissioned dispatch owns no
        // redispatch, so its terminal turn is left pursuing for the ordinary
        // goal disposition adapter and durable eligibility sweep to reconcile
        // into a bounded execution-failure resumption.
        let need = GoalNeed::try_new(String::from(HEADLESS_ESCALATION_GOAL_NEED))
            .map_err(|_| ApprovalJudgeCorruption::Inconsistent("headless escalation goal need"))?;
        let outcome = goal::block_execution_failure_locked(
            connection,
            session,
            need,
            GoalSchedulerProvenance::new(turn),
        )
        .await
        .map_err(map_goal_error)?;
        if !matches!(outcome, GoalTransitionOutcome::Applied(_)) {
            return Err(ApprovalJudgeCorruption::Inconsistent(
                "headless escalation goal transition",
            )
            .into());
        }
    }
    Ok(())
}

fn map_snapshot_append_error(
    error: crate::model_execution::SnapshotAppendError,
) -> ApprovalJudgeRepositoryError {
    match error {
        crate::model_execution::SnapshotAppendError::FrontierInsert(error) => {
            classify_insert(error)
        }
        crate::model_execution::SnapshotAppendError::MemberInsert(error) => error.into(),
        crate::model_execution::SnapshotAppendError::MemberPositionOverflow => {
            ApprovalJudgeCorruption::Inconsistent("headless frontier member position").into()
        }
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
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
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
    let context = SessionAuthorityContext::new(goal, template, system_prompt);
    let generation = judged_turn_goal_generation(&mut *connection, session, turn).await?;
    Ok(
        match load_dispatch_authority(connection, session, generation).await? {
            Some(dispatch) => context.with_dispatch(dispatch),
            None => context,
        },
    )
}

/// Reads the dispatch authority in force for one judged turn.
///
/// Two append-only sources may record a fence: the repository-watch dispatch
/// action and the operator-commissioned dispatch. Both commission generation
/// one of the session they create in the transaction that creates it, so one
/// generation gate serves both, and one session recording both is corruption.
///
/// A dispatch commissions generation one of the session it creates and owns
/// nothing else in it: [`docs/spec/repo-watch.md`] admits a later unrelated
/// successor goal on the same session, and that generation's turns were never
/// described by the dispatch's repository, head, and base values. Binding by
/// session alone would judge such a turn against that stale fence and send its
/// escalation down the headless path, which fails the turn and blocks the goal
/// instead of parking it for the user whose goal it is.
///
/// A turn no generation recorded is not dispatched work either, and resolves to
/// no authority for the same reason.
///
/// [`docs/spec/repo-watch.md`]: ../../../docs/spec/repo-watch.md
async fn load_dispatch_authority(
    connection: &mut PgConnection,
    session: SessionId,
    generation: Option<GoalGeneration>,
) -> Result<Option<ApprovalJudgeDispatchAuthority>, ApprovalJudgeRepositoryError> {
    if generation != Some(DISPATCH_COMMISSIONED_GENERATION) {
        return Ok(None);
    }
    load_commissioned_dispatch_authority(&mut *connection, session).await
}

/// Reads the repository-watch fence recorded for one dispatched session.
/// Reads the commissioned fence recorded for one operator-commissioned session.
///
/// The row is written by the commissioning transaction itself, so unlike the
/// repository-watch source there is no action/event join and no multi-action
/// ambiguity: the session identity is unique in the table.
async fn load_commissioned_dispatch_authority(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<ApprovalJudgeDispatchAuthority>, ApprovalJudgeRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT dispatch_id, repository, target_kind, pull_request_number,
                head_sha, head_repository, head_branch, base_branch, branch
           FROM commissioned_dispatch
          WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let dispatch = ApprovalJudgeDispatchProvenance::Commissioned(
        CommissionedDispatchId::from_uuid(required(&row, "dispatch_id")?),
    );
    decode_dispatch_authority(DispatchAuthorityRow {
        dispatch,
        repository: required(&row, "repository")?,
        target_kind: required(&row, "target_kind")?,
        pull_request_number: row.try_get("pull_request_number")?,
        head_sha: row.try_get("head_sha")?,
        head_repository: row.try_get("head_repository")?,
        head_branch: row.try_get("head_branch")?,
        base_branch: row.try_get("base_branch")?,
        branch: row.try_get("branch")?,
    })
    .map(Some)
}

/// One fence row read from either append-only dispatch source.
struct DispatchAuthorityRow {
    dispatch: ApprovalJudgeDispatchProvenance,
    repository: String,
    target_kind: String,
    pull_request_number: Option<Decimal>,
    head_sha: Option<String>,
    head_repository: Option<String>,
    head_branch: Option<String>,
    base_branch: Option<String>,
    branch: Option<String>,
}

/// Admits one stored fence row into the exact authority the judge consumes.
fn decode_dispatch_authority(
    row: DispatchAuthorityRow,
) -> Result<ApprovalJudgeDispatchAuthority, ApprovalJudgeRepositoryError> {
    let repository = RepositorySlug::try_new(row.repository)
        .map_err(|_| ApprovalJudgeCorruption::Inconsistent("dispatch repository admission"))?;
    let stored = |value: Option<String>, relationship: &'static str| {
        value.ok_or(ApprovalJudgeCorruption::Missing(relationship))
    };
    match row.target_kind.as_str() {
        "pull_request" => {
            let number = row
                .pull_request_number
                .ok_or(ApprovalJudgeCorruption::Missing("pull_request_number"))
                .map(positive_u64_from_numeric)?
                .ok()
                .and_then(NonZeroU64::new)
                .map(PullRequestNumber::new)
                .ok_or(ApprovalJudgeCorruption::Inconsistent(
                    "dispatch pull request admission",
                ))?;
            let input = ApprovalJudgePullRequestAuthorityInput {
                dispatch: row.dispatch,
                repository,
                pull_request: number,
                head_sha: CommitSha::try_new(stored(row.head_sha, "head_sha")?).map_err(|_| {
                    ApprovalJudgeCorruption::Inconsistent("dispatch head SHA admission")
                })?,
                head_repository: RepositorySlug::try_new(stored(
                    row.head_repository,
                    "head_repository",
                )?)
                .map_err(|_| {
                    ApprovalJudgeCorruption::Inconsistent("dispatch head repository admission")
                })?,
                head_branch: BranchName::try_new(stored(row.head_branch, "head_branch")?).map_err(
                    |_| ApprovalJudgeCorruption::Inconsistent("dispatch head branch admission"),
                )?,
                base_branch: BranchName::try_new(stored(row.base_branch, "base_branch")?).map_err(
                    |_| ApprovalJudgeCorruption::Inconsistent("dispatch base branch admission"),
                )?,
            };
            Ok(ApprovalJudgeDispatchAuthority::PullRequest(
                ApprovalJudgePullRequestAuthority::new(input),
            ))
        }
        "branch" => {
            let input = ApprovalJudgeBranchAuthorityInput {
                dispatch: row.dispatch,
                repository,
                branch: BranchName::try_new(stored(row.branch, "branch")?).map_err(|_| {
                    ApprovalJudgeCorruption::Inconsistent("dispatch branch admission")
                })?,
            };
            Ok(ApprovalJudgeDispatchAuthority::Branch(
                ApprovalJudgeBranchAuthority::new(input),
            ))
        }
        _ => Err(ApprovalJudgeCorruption::Inconsistent("dispatch target kind").into()),
    }
}

/// Resolves the goal statement the judged turn was actually produced under.
///
/// A pursuing generation may be superseded while its already-active turn is
/// parked for delegated approval. Reading the session's current generation
/// would then show the judge a broadened replacement and let it authorize a
/// request the originating goal never covered, defeating the narrow-only
/// guarantee. The generation recorded for the turn is therefore the binding
/// whenever the turn has one.
///
/// A turn with no such record resolves to no statement, and the judge
/// escalates. A goal session runs turns the goal machinery did not schedule —
/// an ordinary input submitted into a session that has a goal — and the
/// generation states nothing about those, so reading one against the session's
/// lineage would let a goal attached after the turn already existed supply
/// authority it never covered. Repository-watch dispatch, which is why a
/// dispatched session's requests reach the judge at all, no longer produces
/// one: the turn carrying its tagged context is the commissioned generation's
/// own turn and carries the record.
///
/// This is what the judge reads while it is prepared. Completion asks a
/// different question of the same lineage and uses
/// `judged_turn_authority_in_force`.
async fn load_judged_turn_goal(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<GoalStatement>, ApprovalJudgeRepositoryError> {
    load_judged_turn_lineage(connection, session, turn, judged_turn_goal_statement).await
}

/// Resolves the statement in force for the judged turn under the commit lock.
///
/// Reading and committing ask different questions of the same lineage, so they
/// do not share a resolution. Reading binds the recorded generation exactly, on
/// purpose: a supersession while the turn is parked must not broaden what the
/// model is shown. Committing asks whether the authority the decision was
/// formed under is still in force, and a generation that has since been
/// stopped, achieved, or superseded is not — however exactly it is bound.
///
/// Reusing the reading resolution here would make the check vacuous for every
/// turn goal mode scheduled: the recorded branch returns its generation's
/// statement whatever state that generation reached, so the comparison would
/// find the bytes equal and commit under withdrawn authority.
async fn load_judged_turn_authority_in_force(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<GoalStatement>, ApprovalJudgeRepositoryError> {
    load_judged_turn_lineage(connection, session, turn, judged_turn_authority_in_force).await
}

/// Reads the goal generation the judged turn was scheduled in, if any.
///
/// A turn outside goal mode records none, and the callers say what that means
/// for the question each of them asks.
async fn judged_turn_goal_generation(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<GoalGeneration>, ApprovalJudgeRepositoryError> {
    let recorded = sqlx::query_scalar::<_, Decimal>(
        "SELECT goal_generation
           FROM goal_turn
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    // A row with no positive generation is corruption, not an absent one: the
    // column is checked positive where it is written, so zero cannot have been
    // stored. Collapsing it into absence would hand the callers the answer they
    // give a turn outside goal mode — no fence, and a lineage that resolves to
    // no statement, which reads as authority nothing withdrew.
    Ok(recorded
        .map(|value| {
            positive_u64_from_numeric(value)
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or(ApprovalJudgeCorruption::Inconsistent(
                    "judged turn goal generation",
                ))
        })
        .transpose()?
        .map(GoalGeneration::new))
}

/// Selects a statement from the judged turn's lineage.
///
/// Reading and committing ask different questions of the same lineage, so the
/// loading is shared and the question is the parameter.
type ResolveJudgedTurnStatement =
    fn(&[GoalGenerationSnapshot], Option<GoalGeneration>) -> ResolvedJudgedTurnStatement;

/// What one resolution of the judged turn's lineage decided.
type ResolvedJudgedTurnStatement = Result<Option<GoalStatement>, ApprovalJudgeCorruption>;

/// Loads the judged turn's lineage and hands it to one resolution.
async fn load_judged_turn_lineage(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    resolve: ResolveJudgedTurnStatement,
) -> Result<Option<GoalStatement>, ApprovalJudgeRepositoryError> {
    let recorded = judged_turn_goal_generation(&mut *connection, session, turn).await?;
    let goal = load_goal_from_connection(&mut *connection, session)
        .await
        .map_err(map_goal_error)?;
    match goal {
        None if recorded.is_some() => {
            Err(ApprovalJudgeCorruption::Missing("judged turn goal").into())
        }
        None => Ok(None),
        Some(goal) => Ok(resolve(goal.generations(), recorded)?),
    }
}

/// Selects the statement still in force for the judged turn, if any.
///
/// A recorded generation supplies its statement only while it remains open; a
/// stopped, achieved, or superseded one supplies nothing, because the authority
/// it stated has been withdrawn or discharged. An unrecorded turn resolves as
/// it does for reading — to no statement — so a judge that read nothing
/// commits its decision unchanged: the comparison pins withdrawal, and a turn
/// no generation authorized has no authority to withdraw.
fn judged_turn_authority_in_force(
    generations: &[GoalGenerationSnapshot],
    recorded: Option<GoalGeneration>,
) -> Result<Option<GoalStatement>, ApprovalJudgeCorruption> {
    match recorded {
        Some(generation) => generations
            .iter()
            .find(|snapshot| snapshot.generation() == generation)
            .map(|snapshot| {
                snapshot
                    .state()
                    .is_open()
                    .then(|| snapshot.statement().clone())
            })
            .ok_or(ApprovalJudgeCorruption::Inconsistent(
                "judged turn goal generation",
            )),
        None => judged_turn_goal_statement(generations, None),
    }
}

/// Whether the authority the judge read is still the authority in force.
///
/// The judge reads a statement while it is prepared, spends a provider
/// round-trip deciding under it, and commits afterwards. A user can stop or
/// supersede the goal in that window, so the statement resolved again under the
/// completion lock is compared against the one the decision was made under.
///
/// Equal statements stand. A statement that resolved before and resolves to
/// nothing now belongs to a generation that closed, and one that resolves to
/// different bytes belongs to a generation that was replaced; neither is the
/// authority the recommendation was formed under. A judge that read no
/// statement decided without one, so a goal appearing since revokes nothing and
/// leaves that decision alone — this pins withdrawal, not novelty.
fn read_authority_still_stands(authority: JudgedTurnAuthority<'_>) -> bool {
    match (authority.read, authority.in_force) {
        (Some(read), Some(in_force)) => read == in_force,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// The two statements a completion compares, named because their roles differ.
///
/// Both are `Option<&GoalStatement>` and the comparison is asymmetric —
/// withdrawn authority is `read` present and `in_force` absent, while the
/// reverse preserves the decision — so transposing them would reverse the commit
/// decision without a type error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JudgedTurnAuthority<'statement> {
    /// What the judge read while it was prepared, and decided under.
    read: Option<&'statement GoalStatement>,
    /// What resolves for the same turn under the completion lock.
    in_force: Option<&'statement GoalStatement>,
}

/// Selects which generation's statement states the judged turn's authority.
///
/// A recorded generation binds exactly, and its absence resolves to no
/// statement, which the judge renders as absent and escalates on. Nothing is
/// inferred from the lineage's shape: a turn no generation recorded is a turn
/// no generation authorized, however few or however open the generations are.
fn judged_turn_goal_statement(
    generations: &[GoalGenerationSnapshot],
    recorded: Option<GoalGeneration>,
) -> Result<Option<GoalStatement>, ApprovalJudgeCorruption> {
    match recorded {
        Some(generation) => generations
            .iter()
            .find(|snapshot| snapshot.generation() == generation)
            .map(|snapshot| Some(snapshot.statement().clone()))
            .ok_or(ApprovalJudgeCorruption::Inconsistent(
                "judged turn goal generation",
            )),
        None => Ok(None),
    }
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
    identities: ApprovalJudgeCompletionIdentities,
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
    // What the completion committed, which is not always what its caller
    // offered: a completion whose authority closed during the provider
    // round-trip stored an escalation in place of the provider's
    // recommendation. A retry after an uncertain response still carries the
    // original value, so the replay is judged against the stored decision.
    //
    // A stored escalation is admitted for a different offered value only while
    // the authority is still withdrawn, which is the condition that produced it
    // and one a closed generation cannot leave. With the authority intact the
    // escalation was the provider's own, so an offered approval or denial is a
    // structurally different call and must be reported rather than replayed.
    let stored = approval_judge_recommendation_from_str(&stored_recommendation);
    let substituted = stored == Some(DelegateApprovalRecommendation::EscalateToHuman)
        && !read_authority_still_stands(JudgedTurnAuthority {
            read: prepared.session_context.goal(),
            in_force: load_judged_turn_authority_in_force(
                connection,
                prepared.request.session(),
                prepared.request.turn(),
            )
            .await?
            .as_ref(),
        });
    let exact = approval_judge_terminal_disposition_from_str(&terminal_disposition)
        == Some(ApprovalJudgeTerminalDispositionStorageKind::Completed)
        && (stored == Some(recommendation) || substituted)
        && required::<String>(&row, "rationale")? == rationale.as_str()
        && row.try_get::<Option<Decimal>, _>("input_tokens")? == encoded.input
        && row.try_get::<Option<Decimal>, _>("output_tokens")? == encoded.output
        && row.try_get::<Option<Decimal>, _>("cache_creation_input_tokens")?
            == encoded.cache_creation
        && row.try_get::<Option<Decimal>, _>("cache_read_input_tokens")? == encoded.cache_read;
    if !exact {
        return Ok(None);
    }
    let continuation_exact = stored == Some(DelegateApprovalRecommendation::EscalateToHuman)
        || exact_completion_continuation(connection, prepared, identities.continuation_attempt())
            .await?;
    if !continuation_exact {
        return Ok(None);
    }
    Ok(Some(match stored {
        Some(DelegateApprovalRecommendation::Approve | DelegateApprovalRecommendation::Deny) => {
            CompleteApprovalJudgeOutcome::Decided
        }
        Some(DelegateApprovalRecommendation::EscalateToHuman) => {
            match headless_escalation_identities(connection, prepared.call).await? {
                // An attended escalation persists no terminal evidence of its
                // own: it parks the request for a human and leaves the turn
                // running, so the caller's identities were never used and there
                // is nothing for a replay to disagree with.
                None => CompleteApprovalJudgeOutcome::EscalatedToHuman,
                // A headless escalation terminalized the turn under all three
                // identities, so a replay offering any other one is a
                // structurally different call rather than the same one twice.
                Some(persisted) if persisted != identities => return Ok(None),
                Some(_) => CompleteApprovalJudgeOutcome::HeadlessEscalationTerminalized,
            }
        }
        None => CompleteApprovalJudgeOutcome::EscalatedToHuman,
    }))
}

/// Reads the identities a headless escalation durably closed the turn under.
///
/// Only the commissioned-dispatch audit family retains this legacy path.
async fn headless_escalation_identities(
    connection: &mut PgConnection,
    call: ModelCallId,
) -> Result<Option<ApprovalJudgeCompletionIdentities>, ApprovalJudgeRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT terminal_attempt_id, failure_entry_id, terminal_frontier_id
           FROM commissioned_dispatch_headless_approval_escalation
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(required(&row, "terminal_attempt_id")?),
        SemanticTranscriptEntryId::from_uuid(required(&row, "failure_entry_id")?),
        ContextFrontierId::from_uuid(required(&row, "terminal_frontier_id")?),
    )))
}

/// Reports whether this completion was the round's final one, so an ambiguous
/// replay must check the persisted continuation identity rather than accept a
/// newly supplied one.
///
/// A later request in the same batch that is still undecided, or decided by
/// anything other than a proposal-time source, is evidence that this completion
/// was not the last: those decisions land after the batch is proposed. The
/// proposal-time sources are the ones the proposing transaction itself records —
/// `policy_auto`, `session_blanket`, and `user_override`, whose one-shot
/// pre-approval is consumed at proposal time from the producing call's frozen
/// inventory. Omitting one of them would make a terminal replay accept any
/// supplied continuation identity and mask an identity mismatch.
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
                 OR decision.decision_source
                     NOT IN ('policy_auto', 'session_blanket', 'user_override')
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
                    | "semantic_transcript_entry_id_global"
                    | "context_frontier_id_global"
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
    use std::num::NonZeroU64;

    use signalbox_domain::{
        DurableCommandId, Goal, GoalGeneration, GoalStatement, GoalUserProvenance, SessionId,
    };
    use sqlx::types::Uuid;

    use super::{
        ApprovalJudgeCorruption, ApprovalJudgeRepositoryError, JudgedTurnAuthority,
        judged_turn_authority_in_force, judged_turn_goal_statement, read_authority_still_stands,
    };

    /// A goal commissioned with the given statement, as dispatch commissions it.
    fn commissioned(statement: &str) -> Goal {
        Goal::commission(
            SessionId::from_uuid(Uuid::from_u128(1)),
            goal_statement(statement),
            GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(2))),
        )
    }

    fn goal_statement(text: &str) -> GoalStatement {
        GoalStatement::try_new(String::from(text)).expect("the fixture statement is admitted")
    }

    fn generation(value: u64) -> GoalGeneration {
        GoalGeneration::new(NonZeroU64::new(value).expect("the fixture generation is positive"))
    }

    /// A single open generation is the most permissive lineage there is, and an
    /// unrecorded turn still reads nothing from it. Dispatch binds the turn it
    /// creates, so no turn needs this inference, and a goal attached after some
    /// other turn already existed must not supply that turn authority.
    #[test]
    fn an_unrecorded_turn_resolves_to_no_statement() {
        let goal = commissioned("Dispatched by rule watch-forward: template merge-forward");

        let resolved = judged_turn_goal_statement(goal.generations(), None);

        assert_eq!(resolved, Ok(None));
    }

    /// A recorded turn keeps reading its own generation even once a broader
    /// successor exists.
    #[test]
    fn a_recorded_turn_reads_its_own_generation_not_the_replacement() {
        let original = "land the reviewer fixes";
        let replacement = "land anything at all";
        let superseded = commissioned(original)
            .supersede(
                goal_statement(replacement),
                GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(3))),
            )
            .expect("a pursuing generation admits supersession");

        let resolved = judged_turn_goal_statement(superseded.generations(), Some(generation(1)));

        assert_eq!(resolved, Ok(Some(goal_statement(original))));
    }

    #[test]
    fn a_recorded_generation_absent_from_the_lineage_is_inconsistent() {
        let goal = commissioned("land the reviewer fixes");

        let resolved = judged_turn_goal_statement(goal.generations(), Some(generation(2)));

        assert_eq!(
            resolved,
            Err(ApprovalJudgeCorruption::Inconsistent(
                "judged turn goal generation"
            ))
        );
    }

    /// A recorded turn reads its own generation's statement whether or not that
    /// generation is still open. The judge is deciding a request the turn made
    /// under that authority; a stop landing afterwards withdraws the authority
    /// for future work rather than retroactively unstating what this turn ran
    /// under, and this pins the read rather than the commit — the commit is
    /// caught by the resolution completion runs under its own lock.
    #[test]
    fn a_recorded_turn_reads_a_closed_generation_it_ran_under() {
        let statement = "land the reviewer fixes";
        let stopped = commissioned(statement)
            .stop(GoalUserProvenance::new(DurableCommandId::from_uuid(
                Uuid::from_u128(4),
            )))
            .expect("a pursuing generation admits stopping");

        let resolved = judged_turn_goal_statement(stopped.generations(), Some(generation(1)));

        assert_eq!(resolved, Ok(Some(goal_statement(statement))));
    }

    /// The hole this resolution exists for. Reading binds a recorded generation
    /// exactly and returns its statement whatever state it reached, so reusing
    /// the reading resolution at commit time would compare a statement against
    /// itself and find the authority intact after the user withdrew it.
    #[test]
    fn a_recorded_generation_that_stopped_is_no_longer_in_force() {
        let stopped = commissioned("land the reviewer fixes")
            .stop(GoalUserProvenance::new(DurableCommandId::from_uuid(
                Uuid::from_u128(4),
            )))
            .expect("a pursuing generation admits stopping");

        let read = judged_turn_goal_statement(stopped.generations(), Some(generation(1)));
        let in_force = judged_turn_authority_in_force(stopped.generations(), Some(generation(1)));

        assert_eq!(read, Ok(Some(stopped.current().statement().clone())));
        assert_eq!(in_force, Ok(None));
    }

    /// A supersession replaces the authority the turn ran under, so the
    /// generation it is bound to states nothing that is still in force even
    /// though the lineage still holds its statement.
    #[test]
    fn a_recorded_generation_that_was_superseded_is_no_longer_in_force() {
        let superseded = commissioned("land the reviewer fixes")
            .supersede(
                goal_statement("land anything at all"),
                GoalUserProvenance::new(DurableCommandId::from_uuid(Uuid::from_u128(3))),
            )
            .expect("a pursuing generation admits supersession");

        let in_force =
            judged_turn_authority_in_force(superseded.generations(), Some(generation(1)));

        assert_eq!(in_force, Ok(None));
    }

    /// An open recorded generation states authority that still stands, so the
    /// decision formed under it commits unchanged.
    #[test]
    fn an_open_recorded_generation_remains_in_force() {
        let goal = commissioned("land the reviewer fixes");

        let in_force = judged_turn_authority_in_force(goal.generations(), Some(generation(1)));

        assert_eq!(in_force, Ok(Some(goal.current().statement().clone())));
    }

    /// The ordinary case: nothing moved while the judge was deciding, so the
    /// recommendation it formed is the one that commits.
    #[test]
    fn an_unchanged_statement_still_stands_at_completion() {
        let statement = goal_statement("land the reviewer fixes");

        let stands = read_authority_still_stands(JudgedTurnAuthority {
            read: Some(&statement),
            in_force: Some(&statement),
        });

        assert!(stands);
    }

    /// The hazard this recheck exists for: the goal was stopped while the
    /// request sat awaiting a decision, so the statement resolves to nothing
    /// and the authority the recommendation was formed under is gone.
    #[test]
    fn a_statement_that_stopped_no_longer_stands() {
        let read = goal_statement("land the reviewer fixes");

        let stands = read_authority_still_stands(JudgedTurnAuthority {
            read: Some(&read),
            in_force: None,
        });

        assert!(!stands);
    }

    /// A supersession is a replacement, not a continuation: the judge decided
    /// under the statement it read, and the one now in force may authorize
    /// something wider than that decision covered.
    #[test]
    fn a_replaced_statement_no_longer_stands() {
        let read = goal_statement("land the reviewer fixes");
        let in_force = goal_statement("land anything at all");

        let stands = read_authority_still_stands(JudgedTurnAuthority {
            read: Some(&read),
            in_force: Some(&in_force),
        });

        assert!(!stands);
    }

    /// A judge that read no statement decided without one. A goal attached
    /// since withdraws nothing, so this pins withdrawal rather than novelty and
    /// leaves such a decision alone.
    #[test]
    fn a_goal_appearing_after_an_absent_read_still_stands() {
        let in_force = goal_statement("land the reviewer fixes");

        let stands = read_authority_still_stands(JudgedTurnAuthority {
            read: None,
            in_force: Some(&in_force),
        });

        assert!(stands);
    }

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
