use super::credential_pool::{
    SelectedRuntimePoolCredential, consume_pool_member_actions, prepared_serving_evidence,
    select_runtime_pool_credential, serving_pool_target,
};
use super::live_turn::require_live_execution_with_targets;
use super::persist_terminal::persist_failed_with_delegated_child_result;
use super::persist_tool_round::persist_credential_pool_exhaustion;
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
use std::collections::{BTreeMap, BTreeSet, HashSet};

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
pub(crate) async fn prepare_tool_continuation_call(
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
    compaction_failed: bool,
    call: ModelCallId,
    failure_identities: FailedModelCallTurnIdentities,
    steering_frontier: signalbox_domain::ContextFrontierId,
    mut steering_candidates: BTreeMap<
        AcceptedInputId,
        (signalbox_domain::SemanticTranscriptEntryId, TurnId),
    >,
) -> Result<PrepareToolContinuationOutcome, ModelCallRepositoryError> {
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
        let (entry, successor_turn) = steering_candidates
            .remove(&accepted_input)
            .ok_or(ModelCallCorruption::Missing("reserved steering identities"))?;
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
    let usage_limit = resolved_target
        .as_ref()
        .ok()
        .and_then(|resolved| continuation_usage_limits.get(&(resolved.target(), fast_mode)));
    let compacted_input_bytes =
        projection
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry.payload() {
                signalbox_domain::SemanticTranscriptEntryPayload::ContextSummary {
                    value, ..
                } => Some(value.as_str()),
                _ => None,
            });
    let compacted_request_content_bytes = if !compaction_failed
        && let Some(summary) = compacted_input_bytes
    {
        let system = super::prepared::load_frozen_epoch_system_prompt(
            connection,
            session,
            execution
                .active_turn()
                .configuration()
                .session_defaults_version(),
        )
        .await?;
        // Adapter envelopes are reserved separately; content includes JSON escaping.
        let encoded_bytes = |text: &str| -> Result<u64, ModelCallRepositoryError> {
            serde_json::to_vec(text)
                .map(|bytes| bytes.len().saturating_sub(2) as u64)
                .map_err(|_| ModelCallCorruption::Inconsistent("continuation text encoding").into())
        };
        let mut bytes = encoded_bytes(summary)?.saturating_add(encoded_bytes(
            system.as_ref().map_or("", |prompt| prompt.as_str()),
        )?);
        if let Some(measurement) = usage_limit.and_then(|limit| limit.entry_measurement.as_ref()) {
            let mut request = execution
                .preview_initial_call_consuming_steering(
                    call,
                    steering_entries.clone(),
                    steering_snapshot,
                )
                .map_err(|_| ModelCallCorruption::Inconsistent("compacted continuation preview"))?;
            super::prepared::resolve_runner_placement_entries(connection, &mut request).await?;
            let tool_entries =
                super::prepared::load_tool_conversation_entries(connection, &request)
                    .await?
                    .ok_or(ModelCallCorruption::Missing(
                        "compacted continuation tool entries",
                    ))?;
            let provenance =
                super::prepared::load_provider_reasoning_provenance(connection, &request).await?;
            let operation = signalbox_application::PreparedModelOperation::render(
                request,
                credential_reference.clone(),
                system,
                Box::new([]),
                &tool_entries,
                &provenance,
            )
            .map_err(|_| ModelCallCorruption::Inconsistent("compacted continuation rendering"))?;
            bytes = bytes.saturating_add(measurement.additional_entry_bytes(&operation).ok_or(
                ModelCallCorruption::Inconsistent("compacted continuation measurement"),
            )?);
        }
        Some(bytes)
    } else {
        None
    };
    let headroom_exhausted = if !compaction_failed
        && let Ok(resolved) = resolved_target
        && let Some(limit) = continuation_usage_limits.get(&(resolved.target(), fast_mode))
    {
        load_tool_continuation_headroom_evidence(
            connection,
            session,
            turn,
            producing_call,
            execution
                .active_turn()
                .pending_steering()
                .iter()
                // The rendered projection already includes pending steering when measured.
                .filter(|_| {
                    compacted_request_content_bytes.is_none() || limit.entry_measurement.is_none()
                })
                .map(|pending| pending.accepted_input().into_uuid())
                .collect(),
            serving_pool_target(credential_families, resolved.target(), fast_mode),
            limit.clone(),
            compacted_request_content_bytes,
        )
        .await?
    } else {
        false
    };
    if compaction_failed
        || (headroom_exhausted && compacted_input_bytes.is_some_and(|summary| summary.len() == 1))
    {
        let reclassifications = steering_identities
            .into_iter()
            .map(|(_, identity)| identity)
            .collect::<Vec<_>>();
        let mut proposed_turns = BTreeSet::new();
        for identity in &reclassifications {
            record_reclassified_turn_candidate(turn, identity.turn(), &mut proposed_turns)?;
        }
        let failed = execution
            .fail_automatic_context_compaction(
                failure_identities.with_pending_steering_reclassifications(reclassifications),
            )
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "automatic compaction failure could not close tool continuation",
                )
            })?;
        persist_failed_with_delegated_child_result(
            connection,
            &failed,
            TurnTerminalCause::ContextCompactionFailed,
            ProviderReportedTokenUsage::unreported(),
            None,
            None,
        )
        .await?;
        return Ok(PrepareToolContinuationOutcome::ContextCompactionFailed(
            Box::new(failed),
        ));
    }
    if headroom_exhausted {
        sqlx::query(
            "UPDATE turn_lifecycle SET compaction_frontier_id = $3
                      WHERE session_id = $1 AND turn_id = $2 AND state_kind = 'active'",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .bind(projection.snapshot().frontier().snapshot().into_uuid())
        .execute(&mut *connection)
        .await?;
        return Ok(PrepareToolContinuationOutcome::ContextCompactionRequired(
            turn,
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
    if let Some(wait) =
        super::credential_wait::park_initial(connection, &execution, selected.as_ref()).await?
    {
        return Ok(PrepareToolContinuationOutcome::CredentialWait(wait));
    }
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

#[allow(clippy::too_many_arguments)]
async fn load_tool_continuation_headroom_evidence(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    pending_steering: Vec<sqlx::types::Uuid>,
    current_effective_target: ResolvedProviderTarget,
    limit: ToolContinuationUsageLimit,
    compacted_input_bytes: Option<u64>,
) -> Result<bool, ModelCallRepositoryError> {
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
                            SELECT CASE WHEN attempt.error_kind IS NULL THEN
                                        COALESCE(octet_length(attempt.context_result_text), 0)
                                        + COALESCE(octet_length(attempt.result_media_reference::text), 0)
                                   ELSE octet_length(jsonb_build_object('error',
                                        jsonb_build_object('kind', attempt.error_kind,
                                                          'detail', attempt.context_error_detail))::text)
                                   END
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

                            SELECT octet_length(jsonb_build_object('error', jsonb_build_object(
                                        'kind', 'denied', 'detail', decision.denial_reason))::text)
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

                            SELECT octet_length(jsonb_build_object('error', tool_inadmissible_error(request.inadmissible_reason, request.inadmissible_limit, request.inadmissible_argument_bytes))::text) AS content_bytes
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
                ) AS projected_result_content_bytes,
                (SELECT COALESCE(SUM(CASE part.part_kind
                     WHEN 'attachment' THEN $5::bigint
                     ELSE COALESCE(octet_length(part.text_value), 0)
                 END), 0)::numeric
                   FROM accepted_input_content_part AS part
                  WHERE part.accepted_input_id = ANY($4)) AS pending_steering_content_bytes,
                (SELECT COALESCE(SUM(CASE part.part_kind
                     WHEN 'attachment' THEN $5::bigint
                     ELSE COALESCE(octet_length(to_json(part.text_value)::text), 2) - 2
                 END + $6::bigint), 0)::numeric
                   FROM accepted_input_content_part AS part
                  WHERE part.accepted_input_id = ANY($4)) AS pending_steering_rendered_bytes
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
    .bind(&pending_steering)
    .bind(i64::try_from(signalbox_application::MAX_RENDERED_ATTACHMENT_STUB_BYTES)
        .unwrap_or(i64::MAX))
    .bind(i64::try_from(limit.steering_part_framing_bytes).unwrap_or(i64::MAX))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Err(ModelCallCorruption::Missing("completed tool-producing call").into());
    };
    let producing_effective_target = ResolvedProviderTarget::naming(
        ProviderModelIdentity::from_uuid(row.try_get("effective_provider_model_identity_id")?),
    );
    if compacted_input_bytes.is_none() && producing_effective_target != current_effective_target {
        return Ok(false);
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
    let pending_steering_content_bytes = decode("pending_steering_content_bytes")?.ok_or(
        ModelCallCorruption::Missing("pending steering content byte count"),
    )?;
    if let Some(compacted_input_bytes) = compacted_input_bytes {
        let steering_bytes = decode("pending_steering_rendered_bytes")?.ok_or(
            ModelCallCorruption::Missing("pending steering rendered byte count"),
        )?;
        return Ok(compacted_input_bytes
            .saturating_add(limit.request_overhead_bytes)
            .saturating_add(steering_bytes)
            .saturating_add(limit.max_output_tokens())
            > limit.context_window_tokens());
    }
    let Some(input_tokens) = usage.input_tokens() else {
        return Ok(false);
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
        .saturating_add(pending_steering_content_bytes)
        .saturating_add(limit.max_output_tokens())
        > limit.context_window_tokens();
    Ok(exhausted)
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
