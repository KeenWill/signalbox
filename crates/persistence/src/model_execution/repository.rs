use super::continuation::resolve_session_credential;
use super::credential_pool::{
    consume_pool_member_actions, prepared_serving_evidence, select_runtime_pool_credential,
    serving_pool_target,
};
use super::live_turn::{
    load_tool_denial_correlations, load_tool_inadmissible_correlations,
    load_tool_result_correlations, require_live_execution,
};
use super::load::{load_attachment_blob_facts, load_origin_contents, require_exact_call};
use super::persist_terminal::persist_failed_with_delegated_child_result;
use super::persist_tool_round::persist_credential_pool_exhaustion;
use super::prepared::{
    insert_prepared_call, load_frozen_epoch_system_prompt, load_provider_reasoning_provenance,
    load_tool_conversation_entries, resolve_runner_placement_entries,
};
use super::{
    CountedActivationCheckpointOutcome, CredentialPoolRuntimeCatalog, ModelCallCorruption,
    ModelCallOutboxOrderGuard, ModelCallRepositoryError, PostgresModelCallRepository,
    ProspectiveModelCall, ProspectiveModelInput, ReportedModelCallUsage,
    ToolContinuationUsageLimit, map_projected_membership_error, map_scheduling_error,
    preview_entry_content_bytes,
};
use crate::mapping::session_id_to_uuid;
use crate::outbox;
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::submit_input::load_scheduling_projection;
use rust_decimal::Decimal;
use signalbox_application::{AttachmentPreparationFailure, ModelCallCredentialReference};
use signalbox_domain::{
    ContextFrontierId, FailedModelCallTurn, FailedModelCallTurnIdentities, FastMode,
    ModelCallExecutionReconstitutionInput, ModelCallId, ModelTargetCatalog,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SemanticTranscriptEntryRef, SessionId,
    TurnTerminalCause,
};
use sqlx::{PgConnection, PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;

impl PostgresModelCallRepository {
    /// Uses the shared pool, immutable target catalog, and current non-secret
    /// credential reference for calls first pinned by this repository.
    pub fn new(
        pool: PgPool,
        targets: ModelTargetCatalog,
        credential_reference: ModelCallCredentialReference,
    ) -> Self {
        Self {
            pool,
            targets,
            credential_reference,
            runner_recovery: None,
            credential_families: None,
            credential_pools: HashMap::new(),
            same_credential_attempt_bound: Some(NonZeroUsize::MIN),
            cache_inclusive_input_targets: HashSet::new(),
            continuation_usage_limits: HashMap::new(),
        }
    }

    /// Observes whether the session's park suspends further scheduler operations.
    pub async fn session_is_parked(
        &self,
        session: SessionId,
    ) -> Result<bool, ModelCallRepositoryError> {
        Ok(sqlx::query_scalar(
            "SELECT state_kind = 'parked' FROM session_lifecycle WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Shares the runner authority used at model and tool continuation boundaries.
    pub fn with_runner_recovery(
        mut self,
        runner: crate::runner_protocol::RunnerProtocolStore,
    ) -> Self {
        self.runner_recovery = Some(runner);
        self
    }

    pub(super) async fn settle_runner_replacement_after_observation(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session: SessionId,
        observation_frontier: Option<ContextFrontierId>,
    ) -> Result<bool, ModelCallRepositoryError> {
        if let Some(runner) = &self.runner_recovery {
            let boundary = match observation_frontier {
                Some(frontier) => Some(
                    super::live_turn::load_call_snapshot(transaction.as_mut(), session, frontier)
                        .await?
                        .reconstitute()
                        .ok_or(ModelCallCorruption::Inconsistent(
                            "runner replacement observation snapshot",
                        ))?,
                ),
                None => None,
            };
            let (settled, _) = runner
                .settle_replacement_at_boundary(transaction, session, boundary.as_ref())
                .await
                .map_err(|error| match error {
                    crate::runner_protocol::RunnerProtocolStoreError::Database(source) => {
                        ModelCallRepositoryError::from(source)
                    }
                    _ => {
                        ModelCallCorruption::Inconsistent("runner replacement observation boundary")
                            .into()
                    }
                })?;
            return Ok(settled);
        }
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT NOT EXISTS (SELECT 1 FROM runner_replacement_stage WHERE session_id = $1)",
        )
        .bind(session.into_uuid())
        .fetch_one(&mut **transaction)
        .await?)
    }

    /// Selects credentials from each session's latest append-only snapshot.
    pub fn with_session_credentials(
        mut self,
        credential_families: crate::ModelCredentialFamilyCatalog,
    ) -> Self {
        self.credential_families = Some(credential_families);
        self
    }

    /// Enables per-call credential-pool selection and trigger observation.
    pub fn with_credential_pools(mut self, credential_pools: CredentialPoolRuntimeCatalog) -> Self {
        self.credential_pools = credential_pools;
        self
    }

    /// Bounds recorded calls for transient-failure retry on one credential; `None` is unbounded.
    pub fn with_same_credential_attempt_bound(mut self, bound: Option<NonZeroUsize>) -> Self {
        self.same_credential_attempt_bound = bound;
        self
    }

    /// Pins which configured targets report input totals inclusive of cache.
    pub fn with_cache_inclusive_input_targets(
        mut self,
        targets: HashSet<ResolvedProviderTarget>,
    ) -> Self {
        self.cache_inclusive_input_targets = targets;
        self
    }

    /// Pins configured usage headroom for same-turn tool continuations.
    pub fn with_continuation_usage_limits(
        mut self,
        limits: impl IntoIterator<Item = ToolContinuationUsageLimit>,
    ) -> Self {
        self.continuation_usage_limits = limits
            .into_iter()
            .map(|limit| ((limit.target, limit.fast_mode), limit))
            .collect();
        self
    }

    /// Reads the newest ordinary or dedicated-compaction call with reported input
    /// usage for one exact target and effective fast mode.
    ///
    /// A later failed call with no usage does not erase the last provider-confirmed
    /// context size. Callers may use this only as a lower bound: later transcript
    /// entries can make the next request larger, never smaller absent compaction.
    ///
    /// The prospective input names the model-visible entries the next request
    /// would carry. Membership is compared against the reported call's own
    /// frontier, so the allowance covers exactly the content appended after the
    /// provider counted its input. `replays_provider_compaction` states whether
    /// the effective target includes opaque provider-compaction members in that
    /// request projection and therefore whether final-iteration retained counts
    /// describe the next request.
    pub async fn latest_reported_usage<'a>(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
        fast_mode: FastMode,
        replays_provider_compaction: bool,
        prospective: impl Into<ProspectiveModelInput<'a>>,
    ) -> Result<Option<ReportedModelCallUsage>, ModelCallRepositoryError> {
        let effective_target =
            serving_pool_target(self.credential_families.as_ref(), target, fast_mode);
        let (projected_members, uncommitted_content_bytes, rendered_bytes) =
            match prospective.into() {
                ProspectiveModelInput::Rendered(entries) => (
                    entries.keys().copied().collect::<Vec<_>>(),
                    0,
                    Some(
                        entries
                            .values()
                            .copied()
                            .map(Decimal::from)
                            .collect::<Vec<_>>(),
                    ),
                ),
                ProspectiveModelInput::Committed(frontier) => {
                    let mut connection = self.pool.acquire().await?;
                    let members = crate::context_compaction::projected_frontier_membership(
                        &mut connection,
                        session,
                        frontier,
                    )
                    .await
                    .map_err(map_projected_membership_error)?;
                    (members, 0, None)
                }
                ProspectiveModelInput::Preview {
                    projected_members,
                    uncommitted_content_bytes,
                } => (projected_members.to_vec(), uncommitted_content_bytes, None),
            };
        let member_sessions = projected_members
            .iter()
            .map(|member| session_id_to_uuid(member.source_session()))
            .collect::<Vec<_>>();
        let member_entries = projected_members
            .iter()
            .map(|member| member.entry().into_uuid())
            .collect::<Vec<_>>();
        // Calls no newer than the latest compaction cannot win the final call-ID
        // ordering, so discard them before the exact summary-membership probe.
        let row = sqlx::query(
            "WITH latest_compaction AS MATERIALIZED (
                SELECT compaction.source_frontier_id,
                       compaction.summary_entry_id,
                       call.model_call_id,
                       call.resolved_provider_model_identity_id,
                       call.state_kind,
                       call.terminal_disposition_kind,
                       COALESCE(call.usage_input_includes_cache_tokens, false) AS
                           usage_input_includes_cache_tokens,
                       call.input_tokens AS usage_input_tokens,
                       call.output_tokens AS usage_output_tokens,
                       call.cache_creation_input_tokens AS
                           usage_cache_creation_input_tokens,
                       call.cache_read_input_tokens AS usage_cache_read_input_tokens
                  FROM context_compaction AS compaction
                  JOIN context_compaction_model_call AS call
                    ON call.session_id = compaction.session_id
                   AND call.model_call_id = compaction.producing_call_id
                 WHERE compaction.session_id = $1
                   AND NOT EXISTS (
                       SELECT 1
                         FROM context_compaction AS successor
                        WHERE successor.session_id = compaction.session_id
                          AND successor.predecessor_compaction_id =
                              compaction.context_compaction_id
                   )
             ), ordinary_candidate AS (
                SELECT 'ordinary'::text AS call_kind,
                       model_call.model_call_id,
                       model_call.context_frontier_id,
                       model_call.usage_input_includes_cache_tokens,
                       true AS input_is_retained,
                       model_call.retained_input_tokens,
                       model_call.retained_output_tokens,
                       (model_call.terminal_disposition_kind = 'completed') AS output_is_retained,
                       model_call.usage_input_tokens,
                       model_call.usage_output_tokens,
                       model_call.usage_cache_creation_input_tokens,
                       model_call.usage_cache_read_input_tokens,
                       NULL::uuid AS reported_summary_entry_id,
                       EXISTS (
                           SELECT 1
                             FROM semantic_transcript_entry AS compacted
                            WHERE compacted.source_session_id = model_call.session_id
                              AND compacted.producing_model_call_id = model_call.model_call_id
                              AND compacted.payload_kind = 'provider_compaction'
                       ) AS has_provider_compaction,
                       headroom.projected_result_content_bytes AS
                           proven_unreported_content_bytes
                  FROM model_call
                  LEFT JOIN tool_continuation_context_headroom AS headroom
                    ON headroom.session_id = model_call.session_id
                   AND headroom.producing_model_call_id = model_call.model_call_id
                 WHERE model_call.session_id = $1
                   AND model_call.effective_provider_model_identity_id = $6
                   AND model_call.state_kind = 'terminal'
                   AND model_call.usage_input_tokens IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1
                         FROM latest_compaction AS latest
                        WHERE model_call.model_call_id <= latest.model_call_id
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM latest_compaction AS latest
                        WHERE NOT EXISTS (
                            SELECT 1
                              FROM context_frontier_member AS member
                             WHERE member.owning_session_id = model_call.session_id
                               AND member.context_frontier_id =
                                   model_call.context_frontier_id
                               AND member.source_session_id = model_call.session_id
                               AND member.semantic_entry_id = latest.summary_entry_id
                        )
                   )
             ), compaction_candidate AS (
                -- A dedicated compaction call's reported input is the source
                -- text its summary replaced: the summary removed exactly that
                -- material from model visibility, so none of it bounds the next
                -- request. Its reported output is the retained summary, and the
                -- content the compaction did not summarize stays in the
                -- projected membership below.
                SELECT 'context_compaction'::text AS call_kind,
                       latest.model_call_id,
                       latest.source_frontier_id AS context_frontier_id,
                       latest.usage_input_includes_cache_tokens,
                       false AS input_is_retained,
                       NULL::numeric AS retained_input_tokens,
                       NULL::numeric AS retained_output_tokens,
                       true AS output_is_retained,
                       latest.usage_input_tokens,
                       latest.usage_output_tokens,
                       latest.usage_cache_creation_input_tokens,
                       latest.usage_cache_read_input_tokens,
                       latest.summary_entry_id AS reported_summary_entry_id,
                       false AS has_provider_compaction,
                       NULL::numeric AS proven_unreported_content_bytes
                  FROM latest_compaction AS latest
                 WHERE latest.resolved_provider_model_identity_id = $6
                   AND latest.state_kind = 'terminal'
                   AND latest.terminal_disposition_kind = 'completed'
                   AND latest.usage_input_tokens IS NOT NULL
             ), latest_call AS (
                SELECT *
                  FROM (
                      SELECT * FROM ordinary_candidate
                      UNION ALL
                      SELECT * FROM compaction_candidate
                  ) AS candidate
                 ORDER BY model_call_id DESC
                 LIMIT 1
             ), unreported_member AS MATERIALIZED (
                -- An ordinary call's reported input is its own frontier, so
                -- only projected members outside that membership are new. A
                -- compaction call reports no retained input at all: every
                -- projected member except its summary is content the next
                -- request adds to that summary.
                SELECT prospective.source_session_id, prospective.semantic_entry_id
                  FROM UNNEST($2::uuid[], $3::uuid[])
                       AS prospective(source_session_id, semantic_entry_id)
                EXCEPT
                SELECT reported.source_session_id, reported.semantic_entry_id
                  FROM latest_call
                  JOIN context_frontier_member AS reported
                    ON reported.owning_session_id = $1
                   AND reported.context_frontier_id =
                       latest_call.context_frontier_id
                 WHERE latest_call.call_kind = 'ordinary'
             )
             SELECT usage_input_includes_cache_tokens, input_is_retained,
                    retained_input_tokens, retained_output_tokens,
                    has_provider_compaction,
                    output_is_retained,
                    usage_input_tokens, usage_output_tokens,
                    usage_cache_creation_input_tokens,
                    usage_cache_read_input_tokens,
                    CASE WHEN $7::numeric[] IS NULL THEN
                        COALESCE(latest_call.proven_unreported_content_bytes, 0)
                    ELSE 0 END
                    -- Entries an uncommitted preview minted have no durable row
                    -- to score; the preview measured their content itself.
                    + $4::numeric
                    + (
                        SELECT COALESCE(SUM(
                            CASE
                                -- Aggregated provider output usage already
                                -- includes every response part from the call
                                -- that performed server-side compaction.
                                WHEN latest_call.has_provider_compaction
                                     AND entry.producing_model_call_id =
                                         latest_call.model_call_id
                                     AND entry.payload_kind IN (
                                         'assistant_text',
                                         'provider_compaction',
                                         'provider_reasoning',
                                         'assistant_tool_use'
                                     )
                                THEN 0
                                WHEN measured.content_bytes IS NOT NULL
                                THEN measured.content_bytes
                                -- The durable proof already measured every
                                -- result the producing call's round projected,
                                -- including a returning foreground delegation's
                                -- child result. Each correlates to that call
                                -- through the request that produced it.
                                WHEN latest_call.proven_unreported_content_bytes IS NOT NULL
                                     AND (
                                         (
                                             entry.payload_kind IN (
                                                 'tool_execution_result',
                                                 'tool_denied',
                                                 'tool_inadmissible'
                                             )
                                             AND result_request.producing_model_call_id =
                                                 latest_call.model_call_id
                                         )
                                         OR (
                                             entry.payload_kind = 'delegation_result'
                                             AND awaiting_request.producing_model_call_id =
                                                 latest_call.model_call_id
                                         )
                                     )
                                THEN 0
                                ELSE CASE entry.payload_kind
                                    WHEN 'imported_entry' THEN
                                        COALESCE(octet_length(imported.content_encoding), 0)
                                    WHEN 'origin_accepted_input' THEN
                                        COALESCE((
                                            SELECT SUM(CASE part.part_kind
                                                WHEN 'attachment' THEN $8::bigint
                                                ELSE COALESCE(octet_length(part.text_value), 0)
                                            END)
                                              FROM accepted_input_content_part AS part
                                             WHERE part.accepted_input_id =
                                                   input.accepted_input_id
                                        ), 0)
                                    WHEN 'steering_accepted_input' THEN
                                        COALESCE((
                                            SELECT SUM(CASE part.part_kind
                                                WHEN 'attachment' THEN $8::bigint
                                                ELSE COALESCE(octet_length(part.text_value), 0)
                                            END)
                                              FROM accepted_input_content_part AS part
                                             WHERE part.accepted_input_id =
                                                   input.accepted_input_id
                                        ), 0)
                                    WHEN 'context_summary' THEN
                                        COALESCE(octet_length(entry.context_summary_value), 0)
                                    WHEN 'assistant_text' THEN
                                        COALESCE(octet_length(entry.assistant_text_value), 0)
                                    WHEN 'provider_reasoning' THEN
                                        COALESCE(octet_length(entry.assistant_text_value), 0)
                                    WHEN 'provider_compaction' THEN
                                        CASE WHEN $5::boolean THEN
                                            COALESCE(octet_length(entry.assistant_text_value), 0)
                                        ELSE 0 END
                                    WHEN 'assistant_tool_use' THEN
                                        COALESCE(octet_length(request.tool_name), 0)
                                        + COALESCE(octet_length(request.arguments_text), 0)
                                    WHEN 'tool_execution_result' THEN
                                        CASE WHEN attempt.error_kind IS NULL THEN
                                        COALESCE(octet_length(attempt.context_result_text), 0)
                                   ELSE octet_length(jsonb_build_object('error',
                                        jsonb_build_object('kind', attempt.error_kind,
                                                          'detail', attempt.context_error_detail))::text)
                                   END
                                    WHEN 'tool_denied' THEN
                                        octet_length(jsonb_build_object('error', jsonb_build_object(
                                        'kind', 'denied', 'detail', decision.denial_reason))::text)
                                    WHEN 'tool_inadmissible' THEN
                                        octet_length(jsonb_build_object('error', jsonb_build_object(
                                            'kind', 'execution_failed',
                                            'detail', result_request.inadmissible_reason))::text)
                                    WHEN 'delegated_task' THEN
                                        COALESCE(octet_length(task.task_content), 0)
                                    WHEN 'delegation_message' THEN
                                        COALESCE(octet_length(message.content_text), 0)
                                    WHEN 'delegation_result' THEN
                                        COALESCE(octet_length(child_result.content_text), 0)
                                    ELSE 0
                                END
                            END
                        ), 0)::numeric
                          FROM unreported_member AS prospective
                          LEFT JOIN UNNEST($2::uuid[], $3::uuid[], $7::numeric[])
                               AS measured(source_session_id, semantic_entry_id, content_bytes)
                            ON measured.source_session_id = prospective.source_session_id
                           AND measured.semantic_entry_id = prospective.semantic_entry_id
                          LEFT JOIN semantic_transcript_entry AS entry
                            ON entry.source_session_id = prospective.source_session_id
                           AND entry.semantic_entry_id = prospective.semantic_entry_id
                          LEFT JOIN accepted_input AS input
                            ON input.accepted_input_id = entry.origin_accepted_input_id
                           AND input.session_id = entry.source_session_id
                          LEFT JOIN imported_transcript_entry AS imported
                            ON imported.imported_conversation_id = entry.imported_conversation_id
                           AND imported.imported_transcript_entry_id =
                               entry.imported_transcript_entry_id
                          LEFT JOIN tool_request AS request
                            ON request.request_id = entry.assistant_tool_request_id
                           AND request.session_id = entry.source_session_id
                          LEFT JOIN tool_attempt AS attempt
                            ON attempt.attempt_id = entry.tool_result_attempt_id
                           AND attempt.session_id = entry.source_session_id
                          LEFT JOIN tool_request AS result_request
                            ON result_request.request_id = COALESCE(
                                   attempt.request_id,
                                   entry.tool_result_request_id
                               )
                           AND result_request.session_id = entry.source_session_id
                          LEFT JOIN tool_request AS awaiting_request
                            ON awaiting_request.request_id =
                               entry.delegation_result_awaiting_tool_request_id
                           AND awaiting_request.session_id = entry.source_session_id
                          LEFT JOIN tool_approval_decision AS decision
                            ON decision.request_id = entry.tool_result_request_id
                          LEFT JOIN session_delegation_initial_task AS task
                            ON task.child_session_id = entry.source_session_id
                           AND task.semantic_entry_id = entry.semantic_entry_id
                          LEFT JOIN session_message AS message
                            ON message.message_id = entry.delegation_message_id
                          LEFT JOIN session_child_result AS child_result
                            ON child_result.spawning_tool_request_id =
                               entry.delegation_result_spawning_tool_request_id
                         WHERE entry.semantic_entry_id IS NULL OR NOT (
                                   latest_call.usage_output_tokens IS NOT NULL
                               AND (
                                      (
                                          latest_call.call_kind = 'ordinary'
                                          AND entry.payload_kind IN (
                                              'assistant_text',
                                              'provider_reasoning',
                                              'assistant_tool_use'
                                          )
                                          AND entry.producing_model_call_id =
                                              latest_call.model_call_id
                                      )
                                      OR (
                                          latest_call.call_kind = 'context_compaction'
                                          AND entry.source_session_id = $1
                                          AND entry.semantic_entry_id =
                                              latest_call.reported_summary_entry_id
                                      )
                               )
                           )
                    ) AS projected_unreported_content_bytes
               FROM latest_call",
        )
        .bind(session_id_to_uuid(session))
        .bind(&member_sessions)
        .bind(&member_entries)
        .bind(Decimal::from(uncommitted_content_bytes))
        .bind(replays_provider_compaction)
        .bind(effective_target.identity().into_uuid())
        .bind(rendered_bytes)
        .bind(
            i64::try_from(signalbox_application::MAX_RENDERED_ATTACHMENT_STUB_BYTES)
                .unwrap_or(i64::MAX),
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let decode = |field: &'static str| -> Result<Option<u64>, ModelCallRepositoryError> {
            row.try_get::<Option<Decimal>, _>(field)?
                .map(|value| {
                    if !value.fract().is_zero() || value.is_sign_negative() {
                        return Err(ModelCallCorruption::Inconsistent(
                            "completed model-call token usage",
                        )
                        .into());
                    }
                    u64::try_from(value).map_err(|_| {
                        ModelCallCorruption::Inconsistent("completed model-call token usage").into()
                    })
                })
                .transpose()
        };
        let mut retained_input_tokens = decode("retained_input_tokens")?;
        let mut retained_output_tokens = decode("retained_output_tokens")?;
        let has_provider_compaction = row.try_get::<bool, _>("has_provider_compaction")?;
        if has_provider_compaction
            && (retained_input_tokens.is_none() || retained_output_tokens.is_none())
        {
            return Err(ModelCallCorruption::Missing(
                "provider-compaction retained iteration token counts",
            )
            .into());
        }
        if has_provider_compaction && !replays_provider_compaction {
            // Without the durable block, the next request replays the preserved
            // pre-compaction history. Final-iteration retained counts describe
            // a projection that request will not carry; fall back to the
            // conservative aggregate usage evidence instead.
            retained_input_tokens = None;
            retained_output_tokens = None;
        }
        Ok(Some(ReportedModelCallUsage {
            usage: ProviderReportedTokenUsage::unreported()
                .with_input_tokens(decode("usage_input_tokens")?)
                .with_output_tokens(decode("usage_output_tokens")?)
                .with_cache_creation_input_tokens(decode("usage_cache_creation_input_tokens")?)
                .with_cache_read_input_tokens(decode("usage_cache_read_input_tokens")?),
            input_includes_cache_tokens: row.try_get("usage_input_includes_cache_tokens")?,
            input_is_retained: row.try_get("input_is_retained")?,
            retained_input_tokens,
            retained_output_tokens,
            output_is_retained: row.try_get("output_is_retained")?,
            projected_unreported_content_bytes: decode("projected_unreported_content_bytes")?
                .ok_or(ModelCallCorruption::Missing(
                    "projected unreported transcript content byte count",
                ))?,
        }))
    }

    /// Whether a preserved request-size failure still lacks later evidence that
    /// the same target accepted a call after it or that compaction replaced it.
    ///
    /// The provider may reject an oversized call without reporting token usage.
    /// A successor can use that typed terminal evidence to compact once, but a
    /// later provider-accepted ordinary call or completed compaction on the
    /// prospective lineage supersedes the failure so it cannot trigger forever.
    /// The supplied frontier is the durable immediate prefix of an uncommitted
    /// activation preview, so lineage checks never depend on a speculative ID.
    pub async fn request_too_large_requires_compaction(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
        persisted_prospective_prefix: ContextFrontierId,
    ) -> Result<bool, ModelCallRepositoryError> {
        let requires_compaction = sqlx::query_scalar(
            "WITH latest_failure AS MATERIALIZED (
                SELECT failed.model_call_id
                  FROM model_call AS failed
                 WHERE failed.session_id = $1
                   AND failed.resolved_provider_model_identity_id = $2
                   AND failed.state_kind = 'terminal'
                   AND failed.terminal_provider_failure_cause =
                       'request_too_large'
                   AND context_frontier_preserves_prefix(
                           $1,
                           failed.context_frontier_id,
                           $3
                       )
                 ORDER BY failed.model_call_id DESC
                 LIMIT 1
             )
             SELECT EXISTS (
                 SELECT 1
                   FROM latest_failure AS failed
                  WHERE NOT EXISTS (
                      SELECT 1
                        FROM model_call AS accepted
                       WHERE accepted.session_id = $1
                         AND accepted.resolved_provider_model_identity_id = $2
                         AND accepted.model_call_id > failed.model_call_id
                         AND accepted.state_kind = 'terminal'
                         AND (
                                accepted.terminal_disposition_kind = 'completed'
                             OR accepted.usage_input_tokens IS NOT NULL
                         )
                         AND context_frontier_preserves_prefix(
                                 $1,
                                 accepted.context_frontier_id,
                                 $3
                             )
                      UNION ALL
                      SELECT 1
                        FROM context_compaction AS compaction
                        JOIN context_compaction_model_call AS accepted
                          ON accepted.session_id = compaction.session_id
                         AND accepted.model_call_id =
                             compaction.producing_call_id
                       WHERE compaction.session_id = $1
                         AND accepted.resolved_provider_model_identity_id = $2
                         AND accepted.model_call_id > failed.model_call_id
                         AND accepted.state_kind = 'terminal'
                         AND accepted.terminal_disposition_kind = 'completed'
                         AND context_frontier_preserves_prefix(
                                 $1,
                                 compaction.result_frontier_id,
                                 $3
                             )
                  )
             )",
        )
        .bind(session_id_to_uuid(session))
        .bind(target.identity().into_uuid())
        .bind(persisted_prospective_prefix.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(requires_compaction)
    }

    /// Resolves the credential currently pinned for one session and exact
    /// provider target through the same family catalog used by model calls.
    pub async fn resolve_session_credential_reference(
        &self,
        session: SessionId,
        target: ResolvedProviderTarget,
    ) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        resolve_session_credential(
            &mut connection,
            session,
            target,
            FastMode::Disabled,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await
    }

    /// Derives tool-loop storage from this repository's exact database and
    /// continuation configuration.
    pub fn tool_loop_repository(&self) -> crate::tool_loop::PostgresToolLoopRepository {
        crate::tool_loop::PostgresToolLoopRepository::with_model_calls(
            self.pool.clone(),
            self.targets.clone(),
            self.credential_reference.clone(),
        )
        .with_cache_inclusive_input_targets(self.cache_inclusive_input_targets.clone())
        .with_continuation_usage_limits(self.continuation_usage_limits.clone())
        .with_session_credentials(self.credential_families.clone())
        .with_credential_pools(self.credential_pools.clone())
        .with_runner_recovery(self.runner_recovery.clone())
    }

    /// Derives approval-judge storage from this repository's exact database
    /// and model configuration.
    pub fn approval_judge_repository(
        &self,
    ) -> crate::approval_judge::PostgresApprovalJudgeRepository {
        crate::approval_judge::PostgresApprovalJudgeRepository::new(
            self.pool.clone(),
            self.targets.clone(),
            self.credential_reference.clone(),
            self.credential_families.clone(),
            self.cache_inclusive_input_targets.clone(),
        )
    }

    /// Reconstitutes the exact first-call operation for one read-only activation preview.
    pub async fn preview_activation_operation(
        &self,
        preview: &signalbox_domain::PreparedTurnActivation,
        call: ModelCallId,
    ) -> Result<Option<ProspectiveModelCall>, ModelCallRepositoryError> {
        let session_id = preview.turn().session();
        let mut transaction = self.pool.begin().await?;
        let session = match load_session_from_connection(&mut transaction, session_id).await {
            Ok(Some(session)) => session,
            Ok(None) => return Err(ModelCallRepositoryError::NoLiveExecution),
            Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
            Err(SessionRepositoryError::Corruption(error)) => {
                return Err(ModelCallCorruption::CurrentSession(error).into());
            }
        };
        let scheduling = load_scheduling_projection(&mut transaction, session)
            .await
            .map_err(map_scheduling_error)?;
        let starting_entries = preview
            .starting_entries()
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<BTreeMap<_, _>>();
        let frontier_entries = preview
            .starting_snapshot()
            .ordered_entries()
            .map(|reference| {
                starting_entries
                    .get(&reference)
                    .copied()
                    .or_else(|| scheduling.semantic_entry(reference))
                    .cloned()
                    .ok_or_else(|| {
                        ModelCallCorruption::Missing("preview frontier semantic entry").into()
                    })
            })
            .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
        let origin_contents =
            load_origin_contents(&mut transaction, &frontier_entries, &[], &[]).await?;
        let attachment_blob_facts =
            load_attachment_blob_facts(&mut transaction, &origin_contents, &frontier_entries, &[])
                .await?;
        let tool_result_correlations =
            load_tool_result_correlations(&mut transaction, &frontier_entries).await?;
        let tool_inadmissible_correlations =
            load_tool_inadmissible_correlations(&mut transaction, &frontier_entries).await?;
        let tool_denial_correlations =
            load_tool_denial_correlations(&mut transaction, &frontier_entries).await?;
        // The canonical projection the renderer sends. A preview never commits
        // its starting frontier, so a later usage read cannot resolve that
        // identity to membership and takes this instead.
        let projected_members =
            signalbox_domain::ContextFrontierProjection::from_complete_entries(&frontier_entries)
                .map_err(|_| ModelCallCorruption::Inconsistent("preview frontier projection"))?
                .ordered_entries()
                .collect::<Box<[SemanticTranscriptEntryRef]>>();
        // The entries this preview minted are equally uncommitted, so the
        // durable payload-kind accounting a usage read applies to committed
        // members cannot see them. Every other projected member resolved
        // through the scheduling projection and is scored durably there.
        let uncommitted_content_bytes = projected_members
            .iter()
            .filter(|reference| scheduling.semantic_entry(**reference).is_none())
            .filter_map(|reference| starting_entries.get(reference))
            .fold(0_u64, |total, entry| {
                total.saturating_add(preview_entry_content_bytes(entry, &origin_contents))
            });
        let execution = ModelCallExecutionReconstitutionInput::new(
            preview.turn(),
            self.targets.clone(),
            preview.starting_snapshot().clone(),
            frontier_entries,
            origin_contents,
            None,
            Vec::new(),
        )
        .with_attachment_blob_facts(attachment_blob_facts)
        .with_tool_result_correlations(tool_result_correlations)
        .with_tool_denial_correlations(tool_denial_correlations)
        .with_tool_inadmissible_correlations(tool_inadmissible_correlations)
        .reconstitute()
        .map_err(|error| {
            let (_, failure) = error.into_parts();
            ModelCallCorruption::Execution(failure)
        })?;
        let prepared = execution
            .clone()
            .prepare_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let mut request = execution
            .preview_initial_call(call)
            .map_err(|_| ModelCallRepositoryError::InvalidTransition("preview initial call"))?;
        let system_prompt = load_frozen_epoch_system_prompt(
            &mut transaction,
            session_id,
            preview.turn().configuration().session_defaults_version(),
        )
        .await?;
        resolve_runner_placement_entries(transaction.as_mut(), &mut request).await?;
        let Some(tool_entries) = load_tool_conversation_entries(&mut transaction, &request).await?
        else {
            return Ok(None);
        };
        let reasoning_provenance =
            load_provider_reasoning_provenance(&mut transaction, &request).await?;
        let fast_mode = request.model_settings().effective().fast_mode();
        let credential_reference = resolve_session_credential(
            &mut transaction,
            session_id,
            request.call().target(),
            fast_mode,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await?;
        // Preview the member preparation will actually select. The caller
        // spends an authenticated input-token count on this reference before
        // any call exists, so previewing the session default would count
        // against a member a pending displacement or quarantine has already
        // excluded — and an account-wide rate limit reaches the count endpoint
        // too, so activation would abort before selection could reach the
        // admissible member. Selection consumes no displacement row and this
        // transaction rolls back, so nothing durable moves.
        let serving_evidence = prepared_serving_evidence(
            self.credential_families.as_ref(),
            &self.continuation_usage_limits,
            request.call().target(),
            fast_mode,
        );
        let selected = select_runtime_pool_credential(
            &mut transaction,
            session_id,
            execution.turn(),
            execution.current_attempt().id(),
            serving_evidence,
            credential_reference.clone(),
            &self.credential_pools,
        )
        .await?;
        // An exhausted pool has no member to preview at all. Falling back to
        // the session default would spend an authenticated token count against
        // a quarantined or rejected account, and that count fails, aborting
        // activation before preparation could record the typed exhaustion. The
        // caller activates the turn call-free instead and lets ordinary
        // preparation own the closure.
        let Some(credential_reference) = selected.reference else {
            transaction.rollback().await?;
            return Ok(None);
        };
        transaction.rollback().await?;
        Ok(Some(ProspectiveModelCall {
            prepared,
            request,
            credential_reference,
            system_prompt,
            tool_entries,
            reasoning_provenance,
            projected_members,
            uncommitted_content_bytes,
        }))
    }

    /// Checkpoints the exact no-steering initial call in the transaction that
    /// just committed its counted activation.
    pub(crate) async fn checkpoint_counted_activation_in_transaction(
        &self,
        connection: &mut PgConnection,
        activated: &signalbox_domain::ActivatedTurn,
        prospective: &ProspectiveModelCall,
        _outbox_order_guard: ModelCallOutboxOrderGuard,
    ) -> Result<CountedActivationCheckpointOutcome, ModelCallRepositoryError> {
        let prepared = prospective.prepared();
        let signalbox_domain::ActiveTurnPhase::Running { current_attempt } = activated.phase()
        else {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "counted activation is not running",
            ));
        };
        if prepared.session() != activated.session()
            || prepared.turn() != activated.turn()
            || prepared.attempt() != current_attempt.id()
            || current_attempt.state() != &signalbox_domain::CurrentTurnAttemptState::Prepared
            || !prepared.consumed_steering().is_empty()
            || prepared.steering_snapshot().is_some()
        {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "counted preparation does not match activated turn",
            ));
        }
        let fast_mode = activated
            .configuration()
            .effective()
            .model_settings()
            .effective()
            .fast_mode();
        let credential_reference = resolve_session_credential(
            connection,
            activated.session(),
            prepared.call().target(),
            fast_mode,
            &self.credential_reference,
            self.credential_families.as_ref(),
        )
        .await?;
        let serving_evidence = prepared_serving_evidence(
            self.credential_families.as_ref(),
            &self.continuation_usage_limits,
            prepared.call().target(),
            fast_mode,
        );
        let selected = select_runtime_pool_credential(
            connection,
            prepared.session(),
            prepared.turn(),
            prepared.attempt(),
            serving_evidence,
            credential_reference,
            &self.credential_pools,
        )
        .await?;
        if selected.wait.is_some() {
            let execution =
                require_live_execution(connection, activated.session(), &self.targets).await?;
            super::credential_wait::park_initial(connection, &execution, Some(&selected)).await?;
            return Ok(CountedActivationCheckpointOutcome::CredentialWait);
        }
        outbox::lock_sequence_allocator(connection).await?;
        let Some(credential_reference) = selected.reference.as_ref() else {
            // The caller must close this call-free exhaustion in the same
            // transaction as its retained member evidence.
            let policy = selected
                .policy
                .ok_or(ModelCallRepositoryError::InvalidTransition(
                    "credential-pool exhaustion is missing its frozen policy",
                ))?;
            return Ok(CountedActivationCheckpointOutcome::PoolExhausted(policy));
        };
        insert_prepared_call(
            connection,
            prepared,
            credential_reference,
            selected.policy.as_ref(),
            self.cache_inclusive_input_targets
                .contains(&prepared.call().target()),
            serving_evidence,
        )
        .await?;
        consume_pool_member_actions(
            connection,
            prepared.turn(),
            &selected.pending_consumed_actions,
        )
        .await?;
        Ok(CountedActivationCheckpointOutcome::Prepared)
    }

    pub(crate) async fn fail_counted_pool_exhaustion_in_transaction(
        &self,
        connection: &mut PgConnection,
        activated: &signalbox_domain::ActivatedTurn,
        policy: &super::CredentialPoolRuntimePolicy,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError> {
        let execution =
            require_live_execution(connection, activated.session(), &self.targets).await?;
        let exhausted = execution
            .fail_credential_pool_exhausted(policy.name().to_owned(), identities)
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "credential-pool exhaustion could not close counted activation",
                )
            })?;
        persist_credential_pool_exhaustion(connection, &exhausted).await?;
        Ok(exhausted.into_failed())
    }

    /// Preserves a credential wait or closes the prospective call after
    /// definitive attachment failure during provider-native counting.
    pub(crate) async fn fail_counted_attachment_in_transaction(
        &self,
        connection: &mut PgConnection,
        activated: &signalbox_domain::ActivatedTurn,
        prospective: &ProspectiveModelCall,
        failure: AttachmentPreparationFailure,
        identities: FailedModelCallTurnIdentities,
        outbox_order_guard: ModelCallOutboxOrderGuard,
    ) -> Result<Option<FailedModelCallTurn>, ModelCallRepositoryError> {
        let checkpoint = self
            .checkpoint_counted_activation_in_transaction(
                connection,
                activated,
                prospective,
                outbox_order_guard,
            )
            .await?;
        if matches!(
            checkpoint,
            CountedActivationCheckpointOutcome::CredentialWait
        ) {
            return Ok(None);
        }
        if let CountedActivationCheckpointOutcome::PoolExhausted(policy) = checkpoint {
            return self
                .fail_counted_pool_exhaustion_in_transaction(
                    connection, activated, &policy, identities,
                )
                .await
                .map(Some);
        }
        let call = prospective.prepared().call().id();
        let execution = require_exact_call(
            require_live_execution(connection, activated.session(), &self.targets).await?,
            call,
        )?;
        let failed = execution.fail_prepared_call(identities).map_err(|_| {
            ModelCallRepositoryError::InvalidTransition(
                "counted attachment failure requires the exact Prepared call",
            )
        })?;
        persist_failed_with_delegated_child_result(
            connection,
            &failed,
            TurnTerminalCause::AttachmentPreparationFailed,
            ProviderReportedTokenUsage::unreported(),
            None,
            Some(failure),
        )
        .await?;
        Ok(Some(failed))
    }
}
