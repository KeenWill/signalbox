use super::credential_pool::{PreparedServingEvidence, persist_call_pool_policy};
use super::persist_disposition::{insert_snapshot, settle_injection};
use super::{
    CredentialPoolRuntimePolicy, ModelCallCorruption, ModelCallRepositoryError,
    ToolContinuationUsageLimit, encode_selection, require_single, required,
};
use crate::mapping::{
    defaults_version_to_numeric, durable_command_id_from_uuid, session_id_to_uuid, turn_id_to_uuid,
};
use crate::outbox;
use crate::outbox::{InjectionOutcomeOutbox, ModelCallOutboxState, OutboxEvent};
use rust_decimal::Decimal;
use signalbox_application::{
    ModelCallCredentialReference, ProviderReasoningProvenance, ResolvedToolConversationEntry,
};
use signalbox_domain::{
    AcceptedInputDisposition, EmptyTurnInstructionManifestEvidence, InstructionDigest, ModelCallId,
    PreparedModelCallRequest, ProviderModelIdentity, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef,
    SessionId, TurnInstructionManifest, TurnInstructionManifestId,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::BTreeSet;

pub(crate) async fn insert_prepared_call(
    connection: &mut PgConnection,
    prepared: &signalbox_domain::PreparedInitialModelCall,
    credential_reference: &ModelCallCredentialReference,
    credential_pool_policy: Option<&CredentialPoolRuntimePolicy>,
    input_includes_cache_tokens: bool,
    serving_evidence: PreparedServingEvidence<'_>,
) -> Result<(), ModelCallRepositoryError> {
    let call = prepared.call();
    let (kind, direct, alias, alias_selected) = encode_selection(call.selection());
    for steering in prepared.consumed_steering() {
        let SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input,
            source_turn,
        } = steering.semantic_entry().payload()
        else {
            return Err(ModelCallCorruption::Inconsistent("steering semantic payload").into());
        };
        if *source_turn != prepared.turn()
            || *accepted_input != steering.accepted_input().id()
            || !matches!(
                steering.accepted_input().disposition(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: consuming_call
                } if *consuming_call == call.id()
            )
        {
            return Err(
                ModelCallCorruption::Inconsistent("steering consumption correlation").into(),
            );
        }
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, steering_source_turn_id)
             VALUES ($1, $2, 'steering_accepted_input', $3, $4)",
        )
        .bind(session_id_to_uuid(
            steering.semantic_entry().source_session(),
        ))
        .bind(steering.semantic_entry().identity().into_uuid())
        .bind(accepted_input.into_uuid())
        .bind(turn_id_to_uuid(*source_turn))
        .execute(&mut *connection)
        .await?;
    }
    if let Some(snapshot) = prepared.steering_snapshot() {
        insert_snapshot(connection, snapshot).await?;
    }
    for steering in prepared.consumed_steering() {
        let command: Option<Option<Uuid>> = sqlx::query_scalar(
            "UPDATE accepted_input
                SET disposition_kind = 'consumed_as_steering',
                    consuming_model_call_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
                AND consuming_model_call_id IS NULL
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $4
            RETURNING accepting_command_id",
        )
        .bind(call.id().into_uuid())
        .bind(steering.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(prepared.session()))
        .bind(turn_id_to_uuid(prepared.turn()))
        .fetch_optional(&mut *connection)
        .await?;
        let command = command.ok_or(ModelCallCorruption::Inconsistent(
            "consumed steering accepted input",
        ))?;
        settle_injection(
            connection,
            prepared.session(),
            command,
            InjectionOutcomeOutbox::Delivered {
                turn: Some(prepared.turn()),
            },
        )
        .await?;
    }
    let pinned_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET pinned_provider_model_identity_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND current_attempt_id = $4
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND (
                pinned_provider_model_identity_id IS NULL
                OR pinned_provider_model_identity_id = $1
            )",
    )
    .bind(call.target().identity().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(pinned_rows, "turn-level provider target pin")?;
    let instruction_manifest = sqlx::query(
        "SELECT m.turn_instruction_manifest_id,
                m.eligibility_hash_algorithm, m.eligibility_hash,
                m.admitted_set_hash_algorithm, m.admitted_set_hash,
                m.manifest_hash_algorithm, m.manifest_hash, d.scan_complete
           FROM turn_instruction_manifest AS m
           JOIN instruction_discovery AS d
             ON d.instruction_discovery_id = m.instruction_discovery_id
          WHERE m.session_id = $1
            AND m.turn_id = $2
            AND m.boundary_kind = 'turn_start'",
    )
    .bind(session_id_to_uuid(prepared.session()))
    .bind(turn_id_to_uuid(prepared.turn()))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("turn instruction manifest"))?;
    if !instruction_manifest.try_get::<bool, _>("scan_complete")? {
        return Err(ModelCallCorruption::Inconsistent("instruction discovery completeness").into());
    }
    if instruction_manifest.try_get::<String, _>("eligibility_hash_algorithm")? != "sha256_v1"
        || instruction_manifest.try_get::<String, _>("admitted_set_hash_algorithm")? != "sha256_v1"
        || instruction_manifest.try_get::<String, _>("manifest_hash_algorithm")? != "sha256_v1"
    {
        return Err(
            ModelCallCorruption::Inconsistent("turn instruction manifest hash algorithm").into(),
        );
    }
    let instruction_manifest_id = TurnInstructionManifestId::from_uuid(
        instruction_manifest.try_get("turn_instruction_manifest_id")?,
    );
    let eligibility_hash: Vec<u8> = instruction_manifest.try_get("eligibility_hash")?;
    let admitted_set_hash: Vec<u8> = instruction_manifest.try_get("admitted_set_hash")?;
    let manifest_hash: Vec<u8> = instruction_manifest.try_get("manifest_hash")?;
    let eligibility_hash: [u8; 32] = eligibility_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction eligibility hash"))?;
    let admitted_set_hash: [u8; 32] = admitted_set_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction admitted-set hash"))?;
    let manifest_hash: [u8; 32] = manifest_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction manifest hash"))?;
    TurnInstructionManifest::reconstitute_empty_turn_start(
        instruction_manifest_id,
        prepared.session(),
        prepared.turn(),
        EmptyTurnInstructionManifestEvidence {
            eligibility_hash: InstructionDigest::from_sha256(eligibility_hash),
            admitted_set_hash: InstructionDigest::from_sha256(admitted_set_hash),
            manifest_hash: InstructionDigest::from_sha256(manifest_hash),
        },
    )
    .ok_or(ModelCallCorruption::Inconsistent(
        "turn instruction manifest authentication",
    ))?;
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, resolved_provider_model_identity_id,
             effective_provider_model_identity_id, prepared_credential_model_family,
             prepared_max_output_tokens, prepared_context_window_tokens,
             prepared_provider_compaction_replay, context_frontier_id, credential_reference,
             usage_input_includes_cache_tokens, turn_instruction_manifest_id, state_kind,
             terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 $15, $16, $17, $18, 'prepared', NULL)",
    )
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .bind(kind)
    .bind(direct)
    .bind(alias)
    .bind(alias_selected)
    .bind(call.target().identity().into_uuid())
    .bind(serving_evidence.effective_target.identity().into_uuid())
    .bind(serving_evidence.credential_model_family)
    .bind(
        serving_evidence
            .limit
            .map(|limit| Decimal::from(limit.max_output_tokens())),
    )
    .bind(
        serving_evidence
            .limit
            .map(|limit| Decimal::from(limit.context_window_tokens())),
    )
    .bind(
        serving_evidence
            .limit
            .map(ToolContinuationUsageLimit::replays_provider_compaction),
    )
    .bind(call.frontier().snapshot().into_uuid())
    .bind(credential_reference.as_str())
    .bind(input_includes_cache_tokens)
    .bind(instruction_manifest_id.into_uuid())
    .execute(&mut *connection)
    .await?;
    freeze_recorded_user_overrides(connection, prepared.session(), call.id()).await?;
    if let Some(policy) = credential_pool_policy {
        persist_call_pool_policy(connection, call.id(), policy).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: prepared.session(),
            turn: prepared.turn(),
            call: call.id(),
            state: ModelCallOutboxState::Prepared,
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn resolve_runner_placement_entries(
    connection: &mut PgConnection,
    request: &mut PreparedModelCallRequest,
) -> Result<(), ModelCallRepositoryError> {
    let references = request
        .frontier_entries()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::RunnerPlacementChanged { placement_revision } => {
                Some((entry.reference(), *placement_revision))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for (source, revision) in references {
        let row = sqlx::query(
            "SELECT record.placement_revision, record.requested_sandbox_profile FROM runner_placement_boundary AS boundary
             JOIN runner_session_placement_record AS record USING (session_id, event_ordinal)
             WHERE boundary.session_id = $1 AND boundary.semantic_entry_id = $2 AND boundary.placement_revision = $3
               AND record.placement_revision = boundary.placement_revision AND record.event_kind = 'runner_replaced'",
        ).bind(source.source_session().into_uuid()).bind(source.entry().into_uuid()).bind(Decimal::from(revision.get()))
            .fetch_optional(&mut *connection).await?.ok_or(ModelCallCorruption::Missing("placement boundary record"))?;
        let sandbox: String = required(&row, "requested_sandbox_profile")?;
        let sandbox = crate::mapping::runner_sandbox_from_str(&sandbox).ok_or(
            ModelCallCorruption::Inconsistent("placement boundary sandbox"),
        )?;
        request
            .resolve_runner_placement(source, revision, sandbox)
            .map_err(|_| ModelCallCorruption::Inconsistent("placement boundary correlation"))?;
    }
    Ok(())
}

pub(super) async fn load_provider_reasoning_provenance(
    connection: &mut PgConnection,
    request: &PreparedModelCallRequest,
) -> Result<Box<[ProviderReasoningProvenance]>, ModelCallRepositoryError> {
    let entries = request
        .frontier_entries()
        .filter_map(|entry| {
            if let SemanticTranscriptEntryPayload::ProviderReasoning { producing_call, .. } =
                entry.payload()
            {
                Some((entry.reference(), *producing_call))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(Box::new([]));
    }
    let sessions = entries
        .iter()
        .map(|(source, _)| source.source_session().into_uuid())
        .collect::<Vec<_>>();
    let identifiers = entries
        .iter()
        .map(|(source, _)| source.entry().into_uuid())
        .collect::<Vec<_>>();
    let calls = entries
        .iter()
        .map(|(_, call)| call.into_uuid())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT retained.source_session_id, retained.semantic_entry_id,
                call.model_call_id, call.effective_provider_model_identity_id,
                call.credential_reference
           FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[]) WITH ORDINALITY
                AS retained(source_session_id, semantic_entry_id, producing_call_id, ordinal)
           JOIN model_call AS call
             ON call.session_id = retained.source_session_id
            AND call.model_call_id = retained.producing_call_id
          ORDER BY retained.ordinal",
    )
    .bind(sessions)
    .bind(identifiers)
    .bind(calls)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != entries.len() {
        return Err(ModelCallCorruption::Missing("provider reasoning producing call").into());
    }
    rows.iter()
        .map(|row| {
            Ok(ProviderReasoningProvenance {
                source: SemanticTranscriptEntryRef::from_source(
                    SessionId::from_uuid(row.try_get("source_session_id")?),
                    SemanticTranscriptEntryId::from_uuid(row.try_get("semantic_entry_id")?),
                ),
                producing_call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
                producing_target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    row.try_get("effective_provider_model_identity_id")?,
                )),
                producing_credential: ModelCallCredentialReference::new(
                    row.try_get::<String, _>("credential_reference")?,
                ),
            })
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()
        .map(Vec::into_boxed_slice)
}

pub(super) async fn load_tool_conversation_entries(
    connection: &mut PgConnection,
    request: &PreparedModelCallRequest,
) -> Result<Option<Box<[ResolvedToolConversationEntry]>>, ModelCallRepositoryError> {
    let projection = signalbox_domain::ContextFrontierProjection::from_complete_entries(
        request.frontier_entry_slice(),
    )
    .map_err(|_| ModelCallCorruption::Inconsistent("prepared frontier projection"))?;
    let container_bytes = projection
        .ordered_entries()
        .count()
        .saturating_mul(std::mem::size_of::<ResolvedToolConversationEntry>());
    let limit_bytes = signalbox_application::MAX_RETAINED_FRONTIER_CONTENT_BYTES;
    if container_bytes > limit_bytes {
        return Ok(None);
    }
    let projected = projection.ordered_entries().collect::<BTreeSet<_>>();
    let mut request_ids = BTreeSet::new();
    let mut attempt_ids = BTreeSet::new();
    let mut approval_ids = BTreeSet::new();
    for entry in request
        .frontier_entries()
        .filter(|entry| projected.contains(&entry.reference()))
    {
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. }
            | SemanticTranscriptEntryPayload::ToolInadmissible { request }
            | SemanticTranscriptEntryPayload::ToolClosed { request } => {
                request_ids.insert(*request);
            }
            SemanticTranscriptEntryPayload::ToolDenied { request } => {
                request_ids.insert(*request);
                approval_ids.insert(*request);
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                attempt_ids.insert(*attempt);
            }
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::ProviderReasoning { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    if request_ids.is_empty() && attempt_ids.is_empty() {
        return Ok(Some(Box::new([])));
    }
    if !tool_evidence_fits_before_loading(
        connection,
        &request_ids
            .iter()
            .map(|id: &signalbox_domain::ToolRequestId| id.into_uuid())
            .collect::<Vec<_>>(),
        &attempt_ids
            .iter()
            .map(|id: &signalbox_domain::ToolAttemptId| id.into_uuid())
            .collect::<Vec<_>>(),
        &approval_ids
            .iter()
            .map(|id: &signalbox_domain::ToolRequestId| id.into_uuid())
            .collect::<Vec<_>>(),
        container_bytes,
        limit_bytes,
    )
    .await?
    {
        return Ok(None);
    }
    let attempts = crate::tool_loop::load_attempts_by_id(
        connection,
        &attempt_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;
    for attempt in attempts.values() {
        let request = match attempt {
            signalbox_domain::ReconstitutedToolAttempt::Current(current) => current.request(),
            signalbox_domain::ReconstitutedToolAttempt::Ended(ended) => ended.request(),
        };
        request_ids.insert(request);
    }
    let requests = crate::tool_loop::load_requests_by_id(
        connection,
        &request_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;
    let approvals = crate::tool_loop::load_approvals_by_request(
        connection,
        &approval_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(map_tool_evidence_error)?;

    let mut resolved = Vec::new();
    for entry in request
        .frontier_entries()
        .filter(|entry| projected.contains(&entry.reference()))
    {
        let source = entry.reference();
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse {
                request: request_id,
                ..
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::AssistantToolUse { source, request });
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let attempt = attempts
                    .get(attempt)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool attempt evidence"))?;
                let signalbox_domain::ReconstitutedToolAttempt::Ended(attempt) = attempt else {
                    return Err(
                        ModelCallCorruption::Inconsistent("tool result attempt is live").into(),
                    );
                };
                let request = requests
                    .get(&attempt.request())
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool result request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::ExecutionResult {
                    source,
                    request,
                    attempt,
                });
            }
            SemanticTranscriptEntryPayload::ToolDenied {
                request: request_id,
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("denied tool request evidence"))?;
                let approval = approvals
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("tool denial evidence"))?;
                resolved.push(ResolvedToolConversationEntry::Denied {
                    source,
                    request,
                    approval,
                });
            }
            SemanticTranscriptEntryPayload::ToolInadmissible {
                request: request_id,
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("closed tool request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::Inadmissible { source, request });
            }
            SemanticTranscriptEntryPayload::ToolClosed {
                request: request_id,
            } => {
                let request = requests
                    .get(request_id)
                    .cloned()
                    .ok_or(ModelCallCorruption::Missing("closed tool request evidence"))?;
                resolved.push(ResolvedToolConversationEntry::Closed { source, request });
            }
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::ProviderReasoning { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    Ok(Some(resolved.into_boxed_slice()))
}

async fn tool_evidence_fits_before_loading(
    connection: &mut PgConnection,
    requests: &[Uuid],
    attempts: &[Uuid],
    approvals: &[Uuid],
    container_bytes: usize,
    limit_bytes: usize,
) -> Result<bool, ModelCallRepositoryError> {
    let content_bytes: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes), 0) FROM (
            SELECT octet_length(tool_name)::bigint + octet_length(arguments_text)
                   + COALESCE(octet_length(inadmissible_reason), 0) AS bytes
              FROM tool_request
             WHERE request_id = ANY($1)
                OR request_id IN (SELECT request_id FROM tool_attempt WHERE attempt_id = ANY($2))
            UNION ALL
            SELECT COALESCE(octet_length(result_text), 0)::bigint
                   + COALESCE(octet_length(error_detail), 0)
              FROM tool_attempt WHERE attempt_id = ANY($2)
            UNION ALL
            SELECT COALESCE(octet_length(denial_reason), 0)::bigint
                   + COALESCE(octet_length(rationale), 0)
              FROM tool_approval_decision WHERE request_id = ANY($3)
        ) AS retained",
    )
    .bind(requests)
    .bind(attempts)
    .bind(approvals)
    .fetch_one(connection)
    .await?;
    let content_bytes = usize::try_from(content_bytes).unwrap_or(usize::MAX);
    Ok(container_bytes.saturating_add(content_bytes) <= limit_bytes)
}

pub(super) fn map_tool_evidence_error(
    error: crate::tool_loop::ToolLoopRepositoryError,
) -> ModelCallRepositoryError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ModelCallRepositoryError::from_database(source, commit_ambiguous),
        crate::tool_loop::ToolLoopRepositoryError::IdentityCollision
        | crate::tool_loop::ToolLoopRepositoryError::Corruption(_)
        | crate::tool_loop::ToolLoopRepositoryError::DifferentCommandKind
        | crate::tool_loop::ToolLoopRepositoryError::ConflictingCommandReuse
        | crate::tool_loop::ToolLoopRepositoryError::InvalidTransition(_) => {
            ModelCallCorruption::Inconsistent("tool conversation evidence").into()
        }
    }
}

pub(super) async fn load_call_credential_reference(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
    let reference = sqlx::query_scalar::<_, String>(
        "SELECT credential_reference
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("prepared model call"))?;
    Ok(ModelCallCredentialReference::new(reference))
}

/// Freezes the session's recorded, still-effective user overrides for one newly
/// checkpointed model call.
///
/// Two things retire a recorded override. The first is the consuming
/// `user_override` decision that names it through its UNIQUE column — the
/// durable one-shot boundary. The second is an approval of the identical
/// command recorded by any other authority after the denial: the judge
/// approving the re-proposal it previously denied, a user decision after
/// escalation, or a policy approval. Retiring on the second matters because the
/// first call after a denial can never carry that denial's override (it is
/// checkpointed by the transaction that materializes the denied result), so its
/// re-proposal is decided without the override; leaving the override standing
/// would let a later call pre-approve a repeat of a side-effecting command the
/// session has already let through once.
///
/// "After the denial" is a structural ordering, not a clock — none of these
/// append-only tables carries one. Across turns it is `acceptance_position`,
/// the per-session position of the input that opened each turn. Within the
/// denial's own turn it is the attempt chain: each tool round continues into a
/// fresh `turn_attempt` through `continued_from_attempt_id`, so walking that
/// chain forward from the attempt that produced the denied proposal names the
/// later proposals of the same turn. Both are needed — the re-proposal this
/// override exists for is normally made in the denial's own turn, while a
/// later turn's proposal is ordered only by acceptance.
///
/// That scoping is load-bearing rather than decoration. The same command is
/// routinely approved and executed earlier in a session, long before a later
/// proposal of it is denied; retiring on an approval anywhere in the session
/// would retire most overrides at the instant they were recorded and leave the
/// command with nothing to authorize.
async fn freeze_recorded_user_overrides(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(
        "WITH RECURSIVE effective AS (
            SELECT recorded.denied_request_id,
                   denied_turn.acceptance_position AS denied_turn_position,
                   producing.turn_attempt_id AS denied_attempt_id,
                   denied.tool_name, denied.arguments_kind, denied.arguments_text
              FROM tool_approval_user_override AS recorded
              JOIN tool_request AS denied
                ON denied.request_id = recorded.denied_request_id
              JOIN turn_lifecycle AS denied_turn
                ON denied_turn.turn_id = denied.turn_id
              JOIN model_call AS producing
                ON producing.model_call_id = denied.producing_model_call_id
             WHERE recorded.session_id = $1
               AND NOT EXISTS (
                   SELECT 1
                     FROM tool_approval_decision AS consumed
                    WHERE consumed.override_denied_request_id
                          = recorded.denied_request_id
               )
         ),
         -- The attempts the denial's own turn ran after the denied proposal's.
         later_attempt AS (
            SELECT effective.denied_request_id, successor.turn_attempt_id
              FROM effective
              JOIN turn_attempt AS successor
                ON successor.continued_from_attempt_id
                   = effective.denied_attempt_id
             UNION
            SELECT walked.denied_request_id, successor.turn_attempt_id
              FROM later_attempt AS walked
              JOIN turn_attempt AS successor
                ON successor.continued_from_attempt_id = walked.turn_attempt_id
         )
         INSERT INTO model_call_user_override
            (model_call_id, denied_request_id)
         SELECT $2, effective.denied_request_id
           FROM effective
          WHERE NOT EXISTS (
              SELECT 1
                FROM tool_request AS matching
                JOIN tool_approval_decision AS decision
                  ON decision.request_id = matching.request_id
                 AND decision.decision_kind = 'approve'
                JOIN turn_lifecycle AS matching_turn
                  ON matching_turn.turn_id = matching.turn_id
                JOIN model_call AS proposing
                  ON proposing.model_call_id = matching.producing_model_call_id
               WHERE matching.session_id = $1
                 AND matching.tool_name = effective.tool_name
                 AND matching.arguments_kind = effective.arguments_kind
                 AND matching.arguments_text = effective.arguments_text
                 AND (
                     matching_turn.acceptance_position
                         > effective.denied_turn_position
                     OR EXISTS (
                         SELECT 1
                           FROM later_attempt
                          WHERE later_attempt.denied_request_id
                                = effective.denied_request_id
                            AND later_attempt.turn_attempt_id
                                = proposing.turn_attempt_id
                     )
                 )
          )",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

/// Reloads exactly the override inventory frozen when this call was
/// checkpointed, irrespective of overrides recorded or consumed afterward.
pub(super) async fn load_call_user_overrides(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<Box<[signalbox_domain::RecordedUserOverride]>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT recorded.command_id, recorded.denied_request_id, recorded.judge_model_call_id,
                request.tool_name, request.arguments_kind, request.arguments_text
           FROM model_call_user_override AS frozen
           JOIN tool_approval_user_override AS recorded
             ON recorded.denied_request_id = frozen.denied_request_id
           JOIN tool_request AS request
             ON request.request_id = recorded.denied_request_id
          WHERE recorded.session_id = $1
            AND frozen.model_call_id = $2
          ORDER BY recorded.denied_request_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let command: Uuid = row.try_get("command_id")?;
            let denied_request: Uuid = row.try_get("denied_request_id")?;
            let judge_call: Uuid = row.try_get("judge_model_call_id")?;
            let tool = signalbox_domain::ToolName::try_new(row.try_get("tool_name")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("recorded override tool name"))?;
            let arguments_kind = match row.try_get::<String, _>("arguments_kind")?.as_str() {
                "json" => signalbox_domain::ToolArgumentsKind::Json,
                "undecodable" => signalbox_domain::ToolArgumentsKind::Undecodable,
                _ => {
                    return Err(ModelCallCorruption::Inconsistent(
                        "recorded override arguments kind",
                    )
                    .into());
                }
            };
            let arguments = signalbox_domain::NormalizedToolArguments::try_from_stored(
                arguments_kind,
                row.try_get("arguments_text")?,
            )
            .map_err(|_| ModelCallCorruption::Inconsistent("recorded override arguments"))?;
            Ok(signalbox_domain::RecordedUserOverride::new(
                durable_command_id_from_uuid(command)
                    .map_err(|_| ModelCallCorruption::Inconsistent("recorded override command"))?,
                session,
                signalbox_domain::ToolRequestId::from_uuid(denied_request),
                ModelCallId::from_uuid(judge_call),
                tool,
                arguments,
            ))
        })
        .collect::<Result<Box<[_]>, ModelCallRepositoryError>>()
}

/// Loads the optional session system prompt from the exact immutable defaults
/// epoch the calling turn froze at origin acceptance.
///
/// The epoch row must exist for a live execution; its absence or an
/// inadmissible stored prompt fails closed as corruption rather than sending
/// a call without the instructions the epoch records.
pub(super) async fn load_frozen_epoch_system_prompt(
    connection: &mut PgConnection,
    session: SessionId,
    defaults_version: signalbox_domain::SessionConfigurationDefaultsVersion,
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ModelCallRepositoryError> {
    let row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT system_prompt
           FROM session_defaults_version
          WHERE session_id = $1
            AND version = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(defaults_version_to_numeric(defaults_version))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("frozen defaults epoch"))?;
    row.map(|value| {
        signalbox_domain::SessionSystemPrompt::try_new(value)
            .map_err(|_| ModelCallCorruption::Inconsistent("system prompt admission").into())
    })
    .transpose()
}

#[cfg(all(test, feature = "postgres-integration"))]
#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;

#[cfg(all(test, feature = "postgres-integration"))]
mod preflight_tests {
    use super::*;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ImageExt, runners::AsyncRunner},
    };

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn tool_evidence_preflight_counts_utf8_and_indirect_requests_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let container = Postgres::default()
            .with_tag(super::postgres_test_image::POSTGRES_IMAGE_TAG)
            .with_cmd(crate::disposable_postgres_server_args())
            .with_mount(crate::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(crate::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(crate::local_test_connection_options(&url)?)
            .await?;
        let mut connection = pool.acquire().await?;
        sqlx::raw_sql(
            "CREATE TEMP TABLE tool_request (request_id uuid, tool_name text, arguments_text text, inadmissible_reason text);
             CREATE TEMP TABLE tool_attempt (attempt_id uuid, request_id uuid, result_text text, error_detail text);
             CREATE TEMP TABLE tool_approval_decision (request_id uuid, denial_reason text, rationale text);",
        ).execute(&mut *connection).await?;
        let request = Uuid::from_u128(1);
        let unrelated = Uuid::from_u128(2);
        let attempt = Uuid::from_u128(3);
        sqlx::query(r#"INSERT INTO tool_request VALUES ($1, 't', '{"x":"☃"}', NULL), ($2, 'unrelated', '{"x":"' || repeat('x', 1000) || '"}', NULL)"#)
            .bind(request).bind(unrelated).execute(&mut *connection).await?;
        sqlx::query("INSERT INTO tool_attempt VALUES ($1, $2, '🦀', NULL)")
            .bind(attempt)
            .bind(request)
            .execute(&mut *connection)
            .await?;
        sqlx::query("INSERT INTO tool_approval_decision VALUES ($1, 'é', 'a')")
            .bind(request)
            .execute(&mut *connection)
            .await?;
        // One-byte name, eleven-byte JSON argument, four-byte result and three-byte approval.
        for direct_requests in [Vec::new(), vec![request]] {
            assert!(
                tool_evidence_fits_before_loading(
                    &mut connection,
                    &direct_requests,
                    &[attempt],
                    &[request],
                    0,
                    19
                )
                .await?
            );
            assert!(
                !tool_evidence_fits_before_loading(
                    &mut connection,
                    &direct_requests,
                    &[attempt],
                    &[request],
                    0,
                    18
                )
                .await?
            );
            assert!(
                !tool_evidence_fits_before_loading(
                    &mut connection,
                    &direct_requests,
                    &[attempt],
                    &[request],
                    1,
                    19
                )
                .await?
            );
        }
        drop(connection);
        pool.close().await;
        drop(container);
        Ok(())
    }
}
