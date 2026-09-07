use super::credential_pool::{
    SelectedRuntimePoolCredential, consume_pool_member_actions, prepared_serving_evidence,
    select_runtime_pool_credential, serving_pool_target,
};
use super::live_turn::require_live_execution_with_targets;
use super::persist_terminal::persist_failed_with_delegated_child_result;
use super::persist_tool_round::{
    persist_credential_pool_exhaustion, persist_tool_continuation_headroom_exhaustion,
};
use super::prepared::insert_prepared_call;
use super::reread::{pending_reclassification_candidates, record_reclassified_turn_candidate};
use super::{
    CredentialPoolRuntimeCatalog, ModelCallCorruption, ModelCallIdentityCollision,
    ModelCallOutboxOrderGuard, ModelCallRepositoryError, ToolContinuationUsageLimit,
    ToolContinuationUsageLimitCatalog,
};
use crate::mapping::{session_id_to_uuid, turn_id_to_uuid};
use crate::outbox;
use rust_decimal::Decimal;
use signalbox_application::{ModelCallCredentialReference, PrepareToolContinuationOutcome};
use signalbox_domain::{
    AcceptedInputId, FailedModelCallTurn, FailedModelCallTurnIdentities, FastMode,
    ModelCallExecution, ModelCallId, ModelCallPreparationFailure, ModelTargetCatalog,
    PendingSteeringReclassificationIdentity, PreparedToolResultProjection, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget,
    SessionId, TurnId, TurnTerminalCause,
};
use sqlx::{PgConnection, Row};
use std::collections::{BTreeSet, HashSet};

/// Reconstitutes one continuation against caller-owned, transaction-local tool
/// results before the shared model-call/outbox ordering guard is acquired.
pub(crate) async fn load_tool_continuation_execution(
    connection: &mut PgConnection,
    session: SessionId,
    targets: &ModelTargetCatalog,
    projection: &PreparedToolResultProjection,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    let continuation_snapshot = projection.snapshot();
    let continuation = ResolvedContextFrontierReconstitutionInput::new(
        session,
        continuation_snapshot.frontier().snapshot(),
        continuation_snapshot.ordered_entries().collect(),
    );
    require_live_execution_with_targets(
        connection,
        session,
        Some(targets),
        Some(continuation),
        Some(projection.clone()),
    )
    .await
}

/// Prepares one continuation call inside a caller-owned tool-result
/// transaction. The caller must hold the session scheduler lock and the shared
/// model-call/outbox ordering guard before projecting any result outbox event,
/// and commits or rolls back this function's writes together with that result.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_tool_continuation_call<NextSteeringIdentities>(
    connection: &mut PgConnection,
    _outbox_order_guard: ModelCallOutboxOrderGuard,
    execution: ModelCallExecution,
    session: SessionId,
    turn: TurnId,
    targets: &ModelTargetCatalog,
    credential_reference: &ModelCallCredentialReference,
    credential_families: Option<&crate::ModelCredentialFamilyCatalog>,
    credential_pools: &CredentialPoolRuntimeCatalog,
    cache_inclusive_input_targets: &HashSet<ResolvedProviderTarget>,
    continuation_usage_limits: &ToolContinuationUsageLimitCatalog,
    projection: &PreparedToolResultProjection,
    producing_call: ModelCallId,
    call: ModelCallId,
    failure_identities: FailedModelCallTurnIdentities,
    steering_frontier: signalbox_domain::ContextFrontierId,
    mut next_steering_identities: NextSteeringIdentities,
) -> Result<PrepareToolContinuationOutcome, ModelCallRepositoryError>
where
    NextSteeringIdentities:
        FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
{
    let continuation_snapshot = projection.snapshot();
    if execution.turn() != turn || execution.current_call().is_some() {
        return Ok(PrepareToolContinuationOutcome::NoWork);
    }
    let mut reserved_entries = execution
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    let mut steering_identities =
        Vec::with_capacity(execution.active_turn().pending_steering().len());
    for pending in execution.active_turn().pending_steering() {
        let accepted_input = pending.accepted_input();
        let (entry, successor_turn) = next_steering_identities(accepted_input);
        if !reserved_entries.insert(entry) {
            return Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::SemanticEntry,
            ));
        }
        steering_identities.push((
            entry,
            PendingSteeringReclassificationIdentity::new(accepted_input, successor_turn),
        ));
    }
    let steering_entries = steering_identities
        .iter()
        .map(|(entry, _)| *entry)
        .collect::<Vec<_>>();
    if !steering_entries.is_empty()
        && steering_frontier == continuation_snapshot.frontier().snapshot()
    {
        return Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::TerminalFrontier,
        ));
    }
    let steering_snapshot = (!steering_entries.is_empty()).then_some(steering_frontier);
    let fast_mode = execution
        .configuration()
        .effective()
        .model_settings()
        .effective()
        .fast_mode();
    let resolved_target = targets.resolve(*execution.configuration().effective().model());
    if let Ok(resolved) = resolved_target
        && let Some(limit) = continuation_usage_limits.get(&(resolved.target(), fast_mode))
        && let Some(evidence) = load_tool_continuation_headroom_evidence(
            connection,
            session,
            turn,
            producing_call,
            serving_pool_target(credential_families, resolved.target(), fast_mode),
            *limit,
        )
        .await?
    {
        let source_turn = execution.turn();
        let reclassifications = steering_identities
            .iter()
            .map(|(_, reclassification)| *reclassification)
            .collect::<Vec<_>>();
        let mut proposed_turns = BTreeSet::new();
        for reclassification in &reclassifications {
            record_reclassified_turn_candidate(
                source_turn,
                reclassification.turn(),
                &mut proposed_turns,
            )?;
        }
        let required = execution
            .require_context_compaction_after_tool_results(
                producing_call,
                failure_identities
                    .clone()
                    .with_pending_steering_reclassifications(reclassifications),
            )
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "context headroom exhaustion could not close tool continuation",
                )
            })?;
        persist_failed_with_delegated_child_result(
            connection,
            required.failed(),
            TurnTerminalCause::ContextHeadroomExhausted,
            ProviderReportedTokenUsage::unreported(),
            None,
            None,
        )
        .await?;
        persist_tool_continuation_headroom_exhaustion(connection, &required, evidence).await?;
        return Ok(PrepareToolContinuationOutcome::ContextCompactionRequired(
            Box::new(required),
        ));
    }
    let selected = if let Ok(resolved) = resolved_target {
        let default_reference = resolve_session_credential(
            connection,
            session,
            resolved.target(),
            fast_mode,
            credential_reference,
            credential_families,
        )
        .await?;
        let serving_evidence = prepared_serving_evidence(
            credential_families,
            continuation_usage_limits,
            resolved.target(),
            fast_mode,
        );
        let selected = Some(
            select_runtime_pool_credential(
                connection,
                session,
                turn,
                execution.current_attempt().id(),
                serving_evidence,
                default_reference,
                credential_pools,
            )
            .await?,
        );
        outbox::lock_sequence_allocator(connection).await?;
        selected
    } else {
        None
    };
    if let Some(SelectedRuntimePoolCredential {
        reference: None,
        policy: Some(policy),
        ..
    }) = selected.as_ref()
    {
        let source_turn = execution.turn();
        let reclassifications = steering_identities
            .iter()
            .map(|(_, reclassification)| *reclassification)
            .collect::<Vec<_>>();
        let mut proposed_turns = BTreeSet::new();
        for reclassification in &reclassifications {
            record_reclassified_turn_candidate(
                source_turn,
                reclassification.turn(),
                &mut proposed_turns,
            )?;
        }
        let exhausted = execution
            .fail_credential_pool_exhausted(
                policy.name().to_owned(),
                failure_identities
                    .clone()
                    .with_pending_steering_reclassifications(reclassifications),
            )
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "credential-pool exhaustion could not close tool continuation",
                )
            })?;
        persist_credential_pool_exhaustion(connection, &exhausted).await?;
        return Ok(PrepareToolContinuationOutcome::PoolExhausted(Box::new(
            exhausted,
        )));
    }
    let prepared = match execution.prepare_initial_call_consuming_steering(
        call,
        steering_entries,
        steering_snapshot,
    ) {
        Ok(prepared) => prepared,
        Err(error) if error.failure() == ModelCallPreparationFailure::TargetUnavailable => {
            let resolution = error.target_resolution_error().ok_or(
                ModelCallRepositoryError::InvalidTransition(
                    "continuation target failure omitted its resolution proof",
                ),
            )?;
            let source_turn = error.execution().turn();
            let reclassifications = steering_identities
                .into_iter()
                .map(|(_, reclassification)| reclassification)
                .collect::<Vec<_>>();
            let mut proposed_turns = BTreeSet::new();
            for reclassification in &reclassifications {
                record_reclassified_turn_candidate(
                    source_turn,
                    reclassification.turn(),
                    &mut proposed_turns,
                )?;
            }
            let failed = error
                .execution()
                .clone()
                .fail_target_resolution(
                    resolution,
                    failure_identities.with_pending_steering_reclassifications(reclassifications),
                )
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "continuation target failure could not close execution",
                    )
                })?;
            persist_failed_with_delegated_child_result(
                connection,
                &failed,
                TurnTerminalCause::ModelTargetUnavailable,
                ProviderReportedTokenUsage::unreported(),
                None,
                None,
            )
            .await?;
            return Ok(PrepareToolContinuationOutcome::TargetUnavailable(Box::new(
                failed,
            )));
        }
        Err(_) => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "continuation call cannot be prepared",
            ));
        }
    };
    let selected = selected.ok_or(ModelCallRepositoryError::InvalidTransition(
        "resolved continuation omitted credential selection",
    ))?;
    let credential_reference =
        selected
            .reference
            .ok_or(ModelCallRepositoryError::InvalidTransition(
                "available continuation selection omitted a credential reference",
            ))?;
    let serving_evidence = prepared_serving_evidence(
        credential_families,
        continuation_usage_limits,
        prepared.call().target(),
        fast_mode,
    );
    insert_prepared_call(
        connection,
        &prepared,
        &credential_reference,
        selected.policy.as_ref(),
        cache_inclusive_input_targets.contains(&prepared.call().target()),
        serving_evidence,
    )
    .await?;
    consume_pool_member_actions(
        connection,
        prepared.turn(),
        &selected.pending_consumed_actions,
    )
    .await?;
    Ok(PrepareToolContinuationOutcome::Checkpointed(call))
}

#[derive(Clone, Copy)]
pub(super) struct ToolContinuationHeadroomEvidence {
    pub(super) usage: ProviderReportedTokenUsage,
    pub(super) input_includes_cache_tokens: bool,
    pub(super) projected_result_content_bytes: u64,
    pub(super) limit: ToolContinuationUsageLimit,
}

async fn load_tool_continuation_headroom_evidence(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    current_effective_target: ResolvedProviderTarget,
    limit: ToolContinuationUsageLimit,
) -> Result<Option<ToolContinuationHeadroomEvidence>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT effective_provider_model_identity_id,
                usage_input_includes_cache_tokens,
                usage_input_tokens, usage_output_tokens,
                usage_cache_creation_input_tokens,
                usage_cache_read_input_tokens,
                retained_input_tokens,
                retained_output_tokens,
                EXISTS (
                    SELECT 1
                      FROM semantic_transcript_entry AS compacted
                     WHERE compacted.source_session_id = model_call.session_id
                       AND compacted.producing_model_call_id = model_call.model_call_id
                       AND compacted.payload_kind = 'provider_compaction'
                ) AS has_provider_compaction,
                NOT EXISTS (
                    SELECT 1
                      FROM semantic_transcript_entry AS compacted
                     WHERE compacted.source_session_id = model_call.session_id
                       AND compacted.producing_model_call_id = model_call.model_call_id
                       AND compacted.payload_kind = 'provider_compaction'
                       AND compacted.assistant_text_value::jsonb ->> 'content' IS NOT NULL
                ) AS input_is_retained,
                (
                    SELECT COALESCE(SUM(projected.content_bytes), 0)::numeric
                      FROM (
                            SELECT COALESCE(octet_length(attempt.result_text), 0)
                                   + COALESCE(octet_length(attempt.error_detail), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_attempt AS attempt
                                ON attempt.attempt_id = entry.tool_result_attempt_id
                               AND attempt.session_id = entry.source_session_id
                              JOIN tool_request AS request
                                ON request.request_id = attempt.request_id
                               AND request.session_id = attempt.session_id
                               AND request.turn_id = attempt.turn_id
                             WHERE request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id

                            UNION ALL

                            SELECT COALESCE(octet_length(decision.denial_reason), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_request AS request
                                ON request.request_id = entry.tool_result_request_id
                               AND request.session_id = entry.source_session_id
                              JOIN tool_approval_decision AS decision
                                ON decision.request_id = request.request_id
                             WHERE entry.payload_kind = 'tool_denied'
                               AND request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id

                            UNION ALL

                            SELECT octet_length(request.inadmissible_reason) AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_request AS request ON request.request_id = entry.tool_result_request_id
                                AND request.session_id = entry.source_session_id
                             WHERE entry.payload_kind = 'tool_inadmissible'
                               AND request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id

                            UNION ALL

                            -- A returning foreground await renders the child's
                            -- delivered result as this round's tool result, so
                            -- its content joins the round through the awaiting
                            -- request this call issued.
                            SELECT COALESCE(octet_length(child_result.content_text), 0)
                                       AS content_bytes
                              FROM semantic_transcript_entry AS entry
                              JOIN tool_request AS request
                                ON request.request_id =
                                   entry.delegation_result_awaiting_tool_request_id
                               AND request.session_id = entry.source_session_id
                              JOIN session_child_result AS child_result
                                ON child_result.spawning_tool_request_id =
                                   entry.delegation_result_spawning_tool_request_id
                             WHERE entry.payload_kind = 'delegation_result'
                               AND request.producing_model_call_id = model_call.model_call_id
                               AND request.session_id = model_call.session_id
                               AND request.turn_id = model_call.turn_id
                      ) AS projected
                ) AS projected_result_content_bytes
           FROM model_call
          WHERE model_call_id = $1
            AND session_id = $2
            AND turn_id = $3
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'completed'",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Err(ModelCallCorruption::Missing("completed tool-producing call").into());
    };
    let producing_effective_target = ResolvedProviderTarget::naming(
        ProviderModelIdentity::from_uuid(row.try_get("effective_provider_model_identity_id")?),
    );
    if producing_effective_target != current_effective_target {
        return Ok(None);
    }
    let decode = |field: &'static str| -> Result<Option<u64>, ModelCallRepositoryError> {
        row.try_get::<Option<Decimal>, _>(field)?
            .map(|value| {
                if !value.fract().is_zero() || value.is_sign_negative() {
                    return Err(ModelCallCorruption::Inconsistent(
                        "tool-producing model-call token usage",
                    )
                    .into());
                }
                u64::try_from(value).map_err(|_| {
                    ModelCallCorruption::Inconsistent("tool-producing model-call token usage")
                        .into()
                })
            })
            .transpose()
    };
    let usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(decode("usage_input_tokens")?)
        .with_output_tokens(decode("usage_output_tokens")?)
        .with_cache_creation_input_tokens(decode("usage_cache_creation_input_tokens")?)
        .with_cache_read_input_tokens(decode("usage_cache_read_input_tokens")?);
    let input_includes_cache_tokens = row.try_get("usage_input_includes_cache_tokens")?;
    let projected_result_content_bytes = decode("projected_result_content_bytes")?.ok_or(
        ModelCallCorruption::Missing("projected tool-result content byte count"),
    )?;
    let Some(input_tokens) = usage.input_tokens() else {
        return Ok(None);
    };
    let mut retained_input_tokens = decode("retained_input_tokens")?;
    let mut retained_output_tokens = decode("retained_output_tokens")?;
    let has_provider_compaction = row.try_get::<bool, _>("has_provider_compaction")?;
    let mut input_is_retained: bool = row.try_get("input_is_retained")?;
    if has_provider_compaction
        && (retained_input_tokens.is_none() || retained_output_tokens.is_none())
    {
        return Err(ModelCallCorruption::Missing(
            "provider-compaction retained iteration token counts",
        )
        .into());
    }
    if has_provider_compaction && !limit.replays_provider_compaction() {
        // The disabled projection omits the opaque block and replays the
        // preserved history that aggregate usage measured.
        retained_input_tokens = None;
        retained_output_tokens = None;
        input_is_retained = true;
    }
    let input_tokens = if let Some(retained_input_tokens) = retained_input_tokens {
        retained_input_tokens
    } else if !input_is_retained {
        0
    } else if input_includes_cache_tokens {
        input_tokens
    } else {
        input_tokens
            .saturating_add(usage.cache_creation_input_tokens().unwrap_or(0))
            .saturating_add(usage.cache_read_input_tokens().unwrap_or(0))
    };
    let exhausted = input_tokens
        .saturating_add(
            retained_output_tokens
                .or(usage.output_tokens())
                .unwrap_or(0),
        )
        // Provider-neutral CLI adapters expose no tokenizer-only operation.
        // UTF-8 payload bytes therefore reserve a deliberately conservative
        // allowance for result material appended after the reported input.
        .saturating_add(projected_result_content_bytes)
        .saturating_add(limit.max_output_tokens())
        > limit.context_window_tokens();
    Ok(exhausted.then_some(ToolContinuationHeadroomEvidence {
        usage,
        input_includes_cache_tokens,
        projected_result_content_bytes,
        limit,
    }))
}

pub(crate) async fn resolve_session_credential(
    connection: &mut PgConnection,
    session: SessionId,
    target: ResolvedProviderTarget,
    fast_mode: FastMode,
    fallback: &ModelCallCredentialReference,
    families: Option<&crate::ModelCredentialFamilyCatalog>,
) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
    let Some(families) = families else {
        return Ok(fallback.clone());
    };
    let family = families
        .family_for_call(target, fast_mode)
        .ok_or(ModelCallCorruption::Missing("model credential family"))?;
    match crate::session_credentials::load_current_session_credential(
        connection,
        session_id_to_uuid(session),
        family,
    )
    .await
    {
        Ok(reference) => Ok(reference),
        Err(sqlx::Error::RowNotFound) => match families
            .migration_fallback_family_for_call(target, fast_mode)
        {
            Some(fallback_family) => crate::session_credentials::load_migrated_session_credential(
                connection,
                session_id_to_uuid(session),
                fallback_family,
            )
            .await
            .map_err(|error| match error {
                sqlx::Error::RowNotFound => {
                    ModelCallCorruption::Missing("current session model credential").into()
                }
                error => error.into(),
            }),
            None => Err(ModelCallCorruption::Missing("current session model credential").into()),
        },
        Err(error) => Err(error.into()),
    }
}

/// Closes a turn after a prepared or effect-free tool attempt was lost across
/// process restart. The caller owns the delegated endpoint and scheduler locks
/// and commits this closure with the attempt's `CrashLost` evidence.
pub(crate) async fn fail_tool_crash_in_transaction<NextTurn>(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    projection: &PreparedToolResultProjection,
    identities: FailedModelCallTurnIdentities,
    mut next_turn: NextTurn,
) -> Result<FailedModelCallTurn, ModelCallRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId,
{
    let current_snapshot = projection.snapshot();
    let continuation = ResolvedContextFrontierReconstitutionInput::new(
        session,
        current_snapshot.frontier().snapshot(),
        current_snapshot.ordered_entries().collect(),
    );
    let execution = require_live_execution_with_targets(
        connection,
        session,
        None,
        Some(continuation),
        Some(projection.clone()),
    )
    .await?;
    if execution.turn() != turn || execution.current_call().is_some() {
        return Err(ModelCallRepositoryError::InvalidTransition(
            "tool crash closure does not match live execution",
        ));
    }
    let reclassifications = pending_reclassification_candidates(&execution, &mut next_turn)?;
    let failed = execution
        .recover_tool_crash_after_restart(
            identities.with_pending_steering_reclassifications(reclassifications),
        )
        .map_err(|_| {
            ModelCallRepositoryError::InvalidTransition(
                "tool crash could not close evidence-free execution",
            )
        })?;
    persist_failed_with_delegated_child_result(
        connection,
        &failed,
        TurnTerminalCause::ToolAttemptLost,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
    )
    .await?;
    Ok(failed)
}
