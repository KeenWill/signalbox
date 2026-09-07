use super::decode::map_tool_loop_error;
use super::scheduling_projection::load_scheduling_projection_with_semantic_frontiers;
use super::{SubmitInputCorruption, SubmitInputRepositoryError, required};
use crate::mapping::{
    ActiveTurnPhaseStorageKind, accepted_input_id_from_uuid, active_turn_phase_from_str,
    durable_command_id_to_uuid, positive_u64_from_numeric, session_id_to_uuid, turn_id_from_uuid,
    turn_id_to_uuid,
};
use crate::model_execution::{
    ModelCallCorruption, ModelCallRepositoryError, load_call_snapshot,
    require_live_execution_for_restart,
};
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::tool_loop::{load_active_batch_from_connection, load_runner_recovery_source_snapshot};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputSchedulingProjection, AcceptedInputTurnSchedulingStatus,
    BlobDigest, ContextFrontierId, ContextFrontierProjection, DurableCommandId,
    PreparedSubmitInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload, SessionId,
    SubmitInput, SubmitInputAppliedResult, SubmitInputResult, ToolAttemptId, TurnAttemptId, TurnId,
    UserContentPart,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn supersede_automatic_reconciliation(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<(), SubmitInputRepositoryError> {
    sqlx::query(
        "UPDATE automatic_reconciliation_attempt AS attempt
            SET outcome_kind = 'superseded', finished_at = statement_timestamp()
           FROM automatic_reconciliation AS recovery
          WHERE recovery.turn_id = $1
            AND recovery.session_id = $2
            AND recovery.state_kind = 'attempting'
            AND attempt.turn_id = recovery.turn_id
            AND attempt.attempt_ordinal = recovery.attempt_count
            AND attempt.outcome_kind = 'attempting'",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE automatic_reconciliation
            SET state_kind = 'superseded', exhausted_at = NULL
          WHERE turn_id = $1
            AND session_id = $2
            AND state_kind IN ('scheduled', 'attempting', 'exhausted')",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(connection)
    .await?;
    Ok(())
}

pub(super) async fn prospective_attachment_frontier_exceeds_bound(
    connection: &mut PgConnection,
    session: SessionId,
    prior_queued_inputs: &[AcceptedInputId],
    result: &SubmitInputResult,
    maximum_bytes: u64,
) -> Result<bool, SubmitInputRepositoryError> {
    let current = match load_session_from_connection(connection, session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(SubmitInputCorruption::Inconsistent(
                "provisionally applied input session disappeared",
            )
            .into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(SubmitInputCorruption::CurrentSession(error).into());
        }
    };
    let delegated_parked_frontier =
        load_delegated_parked_attachment_frontier(connection, session).await?;
    let supplemental_semantic_frontiers = delegated_parked_frontier
        .as_ref()
        .map(|frontier| vec![frontier.snapshot.frontier().snapshot()])
        .unwrap_or_default();
    let scheduling = load_scheduling_projection_with_semantic_frontiers(
        connection,
        current,
        &supplemental_semantic_frontiers,
    )
    .await?;
    let live_execution = match require_live_execution_for_restart(connection, session).await {
        Err(ModelCallRepositoryError::Corruption(ModelCallCorruption::Unsupported {
            field: "delegated turn attempt state",
            value,
        })) if value == "stop_requested" => Err(ModelCallRepositoryError::NoLiveExecution),
        // A turn executing a tool batch keeps the `running` phase against the
        // continuation attempt while the call that produced the batch is
        // already terminal. Model-execution reconstitution then sees the
        // turn's retained provider pin with no current call and reports
        // `PinnedTargetUnexpected`, which is exactly the statement that this
        // turn has no live model call to read a frontier from. The scheduling
        // projection records the batch's yielded frontier, so route the state
        // through the same no-live-execution path the parked phases use rather
        // than rolling back an otherwise-applied submission.
        Err(ModelCallRepositoryError::Corruption(ModelCallCorruption::Execution(
            signalbox_domain::ModelCallExecutionReconstitutionFailure::PinnedTargetUnexpected,
        ))) => Err(ModelCallRepositoryError::NoLiveExecution),
        result => result,
    };
    let (base_origins, check_base) = match live_execution {
        Ok(execution) => {
            let mut distinct = BTreeSet::new();
            let complete_entries = execution.frontier_entries().cloned().collect::<Vec<_>>();
            let projection = ContextFrontierProjection::from_complete_entries(&complete_entries)
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent("prospective attachment context projection")
                })?;
            let entries_by_reference = complete_entries
                .iter()
                .map(|entry| (entry.reference(), entry))
                .collect::<BTreeMap<_, _>>();
            let mut projected_entries = Vec::new();
            for reference in projection.ordered_entries() {
                let Some(entry) = entries_by_reference.get(&reference) else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "prospective attachment projected entry",
                    )
                    .into());
                };
                projected_entries.push(*entry);
            }
            let mut origins = projected_entries
                .into_iter()
                .filter_map(|entry| match entry.payload() {
                    InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                        accepted_input,
                    }
                    | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                        accepted_input,
                        ..
                    } => distinct.insert(*accepted_input).then_some(*accepted_input),
                    _ => None,
                })
                .collect::<Vec<_>>();
            origins.extend(
                execution
                    .active_turn()
                    .pending_steering()
                    .iter()
                    .map(|pending| pending.accepted_input())
                    .filter(|accepted_input| distinct.insert(*accepted_input)),
            );
            (
                origins,
                matches!(
                    result,
                    SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                ),
            )
        }
        Err(ModelCallRepositoryError::NoLiveExecution) => {
            if let Some(active) = scheduling.active_turn_execution() {
                let mut distinct = BTreeSet::new();
                let mut origins = if matches!(
                    active.phase(),
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                ) {
                    let snapshot =
                        load_runner_recovery_source_snapshot(connection, session, active.turn())
                            .await
                            .map_err(map_tool_loop_error)?
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner recovery prospective attachment frontier missing",
                            ))?;
                    let complete_entries = snapshot
                        .ordered_entries()
                        .map(|reference| scheduling.semantic_entry(reference).cloned())
                        .collect::<Option<Vec<_>>>()
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "runner recovery prospective attachment frontier entry missing",
                        ))?;
                    let projection =
                        ContextFrontierProjection::from_complete_entries(&complete_entries)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "runner recovery prospective attachment frontier projection",
                                )
                            })?;
                    let entries_by_reference = complete_entries
                        .iter()
                        .map(|entry| (entry.reference(), entry))
                        .collect::<BTreeMap<_, _>>();
                    projection
                        .ordered_entries()
                        .filter_map(
                            |reference| match entries_by_reference[&reference].payload() {
                                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                                    accepted_input,
                                }
                                | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                                    accepted_input,
                                    ..
                                } => distinct.insert(*accepted_input).then_some(*accepted_input),
                                InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
                                | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
                                | InitialSemanticTranscriptEntryPayload::DelegationMessage {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::DelegationResult {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ContextSummary {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ToolInadmissible {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::RunnerPlacementChanged {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
                                | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
                                | InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ProviderReasoning {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                                    ..
                                }
                                | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
                                | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
                                | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. }
                                | InitialSemanticTranscriptEntryPayload::Imported { .. } => None,
                            },
                        )
                        .collect::<Vec<_>>()
                } else {
                    scheduling.active_rendered_frontier_origins().ok_or(
                        SubmitInputCorruption::Inconsistent(
                            "active prospective attachment frontier missing",
                        ),
                    )?
                };
                distinct.extend(origins.iter().copied());
                origins.extend(
                    active
                        .pending_steering()
                        .iter()
                        .map(|pending| pending.accepted_input())
                        .filter(|accepted_input| distinct.insert(*accepted_input)),
                );
                (
                    origins,
                    matches!(
                        result,
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                    ),
                )
            } else if let Some(frontier) = delegated_parked_frontier.as_ref() {
                let origins = delegated_parked_attachment_frontier_origins(
                    connection,
                    session,
                    &scheduling,
                    frontier,
                )
                .await?;
                (
                    origins,
                    matches!(
                        result,
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                    ),
                )
            } else {
                (
                    scheduling
                        .earliest_queued_rendered_base_origins()
                        .transpose()
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "prospective attachment context projection",
                            )
                        })?
                        .unwrap_or_default(),
                    false,
                )
            }
        }
        Err(error) => return Err(error.into()),
    };
    let queued_inputs = scheduling
        .turns()
        .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
        .map(|turn| turn.accepted_input().id())
        .collect::<Vec<_>>();
    let queued_reset_origins = scheduling
        .turns()
        .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
        .enumerate()
        .filter_map(|(index, turn)| {
            scheduling
                .external_predecessor_rendered_base_origins(turn.turn())
                .map(|origins| (index, origins))
        })
        .collect::<BTreeMap<_, _>>();
    let first_changed_queue = if check_base {
        0
    } else {
        prior_queued_inputs
            .iter()
            .zip(&queued_inputs)
            .take_while(|(before, after)| before == after)
            .count()
    };
    if !check_base && first_changed_queue == queued_inputs.len() {
        return Err(SubmitInputCorruption::Inconsistent(
            "applied turn-origin input left the prospective queue unchanged",
        )
        .into());
    }

    let all_origins = base_origins
        .iter()
        .chain(&queued_inputs)
        .chain(queued_reset_origins.values().flatten())
        .map(|accepted_input| accepted_input.into_uuid())
        .collect::<Vec<_>>();
    if !accepted_inputs_have_attachment_parts(connection, &all_origins).await? {
        return Ok(false);
    }
    let rows = sqlx::query(
        "SELECT part.accepted_input_id, part.blob_digest, blob.byte_length
           FROM accepted_input_content_part AS part
           JOIN blob ON blob.digest = part.blob_digest
          WHERE part.accepted_input_id = ANY($1)
            AND part.part_kind = 'attachment'
          ORDER BY part.accepted_input_id, part.position",
    )
    .bind(&all_origins)
    .fetch_all(&mut *connection)
    .await?;
    let mut attachments = BTreeMap::<AcceptedInputId, Vec<(BlobDigest, u64)>>::new();
    for row in rows {
        let accepted_input =
            accepted_input_id_from_uuid(row.try_get::<Uuid, _>("accepted_input_id")?);
        let digest = BlobDigest::from_bytes(
            row.try_get::<Vec<u8>, _>("blob_digest")?
                .try_into()
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent("prospective attachment digest")
                })?,
        );
        let length = positive_u64_from_numeric(row.try_get("byte_length")?).map_err(|_| {
            SubmitInputCorruption::Inconsistent("prospective attachment byte length")
        })?;
        attachments
            .entry(accepted_input)
            .or_default()
            .push((digest, length));
    }

    let mut digests = BTreeMap::<BlobDigest, u64>::new();
    let mut total = 0_u64;
    for accepted_input in &base_origins {
        add_prospective_attachment_lengths(
            &mut digests,
            &mut total,
            attachments
                .get(accepted_input)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )?;
    }
    if check_base && total > maximum_bytes {
        return Ok(true);
    }
    for (index, accepted_input) in queued_inputs.iter().enumerate() {
        if let Some(reset_origins) = queued_reset_origins.get(&index) {
            digests.clear();
            total = 0;
            for reset_origin in reset_origins {
                add_prospective_attachment_lengths(
                    &mut digests,
                    &mut total,
                    attachments
                        .get(reset_origin)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                )?;
            }
        }
        add_prospective_attachment_lengths(
            &mut digests,
            &mut total,
            attachments
                .get(accepted_input)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )?;
        if index >= first_changed_queue && total > maximum_bytes {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn session_has_attachment_parts(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, SubmitInputRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM accepted_input AS accepted
               JOIN accepted_input_content_part AS part
                 ON part.accepted_input_id = accepted.accepted_input_id
              WHERE accepted.session_id = $1
                AND part.part_kind = 'attachment'
         )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut *connection)
    .await?)
}

async fn accepted_inputs_have_attachment_parts(
    connection: &mut PgConnection,
    accepted_inputs: &[Uuid],
) -> Result<bool, SubmitInputRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM accepted_input_content_part
              WHERE accepted_input_id = ANY($1)
                AND part_kind = 'attachment'
         )",
    )
    .bind(accepted_inputs)
    .fetch_one(&mut *connection)
    .await?)
}

struct DelegatedParkedAttachmentFrontier {
    turn: TurnId,
    snapshot: ResolvedContextFrontierSnapshot,
}

async fn load_delegated_parked_attachment_frontier(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<DelegatedParkedAttachmentFrontier>, SubmitInputRepositoryError> {
    let row = sqlx::query(
        "SELECT turn_id, active_phase_kind, recovery_model_call_id,
                (
                    SELECT call.model_call_id
                      FROM model_call AS call
                     WHERE call.session_id = turn_lifecycle.session_id
                       AND call.turn_id = turn_lifecycle.turn_id
                       AND call.state_kind = 'cancellation_requested'
                ) AS stop_requested_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND origin_kind = 'delegation'
            AND state_kind = 'active'
            AND NOT delegation_runtime_terminal
            AND goal_turn_is_runtime_relevant(session_id, turn_id)",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let turn = turn_id_from_uuid(required(&row, "turn_id")?);
    let phase_spelling: String = required(&row, "active_phase_kind")?;
    let phase = active_turn_phase_from_str(&phase_spelling).ok_or({
        SubmitInputCorruption::Unsupported {
            field: "delegated parked active phase",
            value: phase_spelling,
        }
    })?;
    let snapshot = match phase {
        ActiveTurnPhaseStorageKind::AwaitingToolApproval
        | ActiveTurnPhaseStorageKind::AwaitingChild
        | ActiveTurnPhaseStorageKind::AwaitingToolRecovery => {
            load_active_batch_from_connection(connection, session, turn)
                .await
                .map_err(map_tool_loop_error)?
                .map(|batch| batch.yielded_snapshot().clone())
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated parked tool frontier missing",
                ))?
        }
        ActiveTurnPhaseStorageKind::AwaitingModelCallRecovery => {
            let recovery_call: Uuid = required(&row, "recovery_model_call_id")?;
            let frontier = sqlx::query_scalar::<_, Uuid>(
                "SELECT context_frontier_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND session_id = $2
                    AND turn_id = $3",
            )
            .bind(recovery_call)
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegated model recovery frontier missing",
            ))?;
            load_call_snapshot(connection, session, ContextFrontierId::from_uuid(frontier))
                .await
                .map_err(|error| SubmitInputRepositoryError::ModelExecution(Box::new(error)))?
                .reconstitute()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated model recovery snapshot invalid",
                ))?
        }
        ActiveTurnPhaseStorageKind::AwaitingRunnerRecovery => {
            load_runner_recovery_source_snapshot(connection, session, turn)
                .await
                .map_err(map_tool_loop_error)?
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated runner recovery prospective attachment frontier missing",
                ))?
        }
        ActiveTurnPhaseStorageKind::Running => {
            let stop_requested_call: Option<Uuid> = row.try_get("stop_requested_model_call_id")?;
            let Some(stop_requested_call) = stop_requested_call else {
                // A turn executing a tool batch keeps the `running` phase
                // while the call that produced the batch is already terminal,
                // so this is the delegated spelling of the state the
                // accepted-input path reads through
                // `active_rendered_frontier_origins`. The batch's yielded
                // frontier is the delegated turn's retained context; without
                // it, accounting would fall back to the earliest queued base
                // and omit both that frontier and its pending steering.
                // A delegated turn with neither a cancellation-requested call
                // nor an active batch has a live model call, which the
                // live-execution path reads instead.
                return Ok(load_active_batch_from_connection(connection, session, turn)
                    .await
                    .map_err(map_tool_loop_error)?
                    .map(|batch| DelegatedParkedAttachmentFrontier {
                        turn,
                        snapshot: batch.yielded_snapshot().clone(),
                    }));
            };
            let frontier = sqlx::query_scalar::<_, Uuid>(
                "SELECT context_frontier_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND session_id = $2
                    AND turn_id = $3
                    AND state_kind = 'cancellation_requested'",
            )
            .bind(stop_requested_call)
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegated stop-requested frontier missing",
            ))?;
            load_call_snapshot(connection, session, ContextFrontierId::from_uuid(frontier))
                .await
                .map_err(|error| SubmitInputRepositoryError::ModelExecution(Box::new(error)))?
                .reconstitute()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated stop-requested snapshot invalid",
                ))?
        }
    };
    Ok(Some(DelegatedParkedAttachmentFrontier { turn, snapshot }))
}

async fn delegated_parked_attachment_frontier_origins(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &AcceptedInputSchedulingProjection,
    frontier: &DelegatedParkedAttachmentFrontier,
) -> Result<Vec<AcceptedInputId>, SubmitInputRepositoryError> {
    let complete_entries = frontier
        .snapshot
        .ordered_entries()
        .map(|reference| scheduling.semantic_entry(reference).cloned())
        .collect::<Option<Vec<_>>>()
        .ok_or(SubmitInputCorruption::Inconsistent(
            "delegated parked prospective attachment frontier entry missing",
        ))?;
    let projection =
        ContextFrontierProjection::from_complete_entries(&complete_entries).map_err(|_| {
            SubmitInputCorruption::Inconsistent(
                "delegated parked prospective attachment frontier projection",
            )
        })?;
    let entries_by_reference = complete_entries
        .iter()
        .map(|entry| (entry.reference(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut distinct = BTreeSet::new();
    let mut origins = projection
        .ordered_entries()
        .filter_map(
            |reference| match entries_by_reference[&reference].payload() {
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input,
                    ..
                } => distinct.insert(*accepted_input).then_some(*accepted_input),
                InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
                | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
                | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
                | InitialSemanticTranscriptEntryPayload::DelegationResult { .. }
                | InitialSemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                | InitialSemanticTranscriptEntryPayload::ContextSummary { .. }
                | InitialSemanticTranscriptEntryPayload::ToolInadmissible { .. }
                | InitialSemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
                | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
                | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
                | InitialSemanticTranscriptEntryPayload::ProviderCompaction { .. }
                | InitialSemanticTranscriptEntryPayload::ProviderReasoning { .. }
                | InitialSemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
                | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
                | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. }
                | InitialSemanticTranscriptEntryPayload::Imported { .. } => None,
            },
        )
        .collect::<Vec<_>>();
    let pending = sqlx::query_scalar::<_, Uuid>(
        "SELECT accepted_input_id
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(frontier.turn))
    .fetch_all(&mut *connection)
    .await?;
    origins.extend(
        pending
            .into_iter()
            .map(accepted_input_id_from_uuid)
            .filter(|accepted_input| distinct.insert(*accepted_input)),
    );
    Ok(origins)
}

fn add_prospective_attachment_lengths(
    digests: &mut BTreeMap<BlobDigest, u64>,
    total: &mut u64,
    attachments: &[(BlobDigest, u64)],
) -> Result<(), SubmitInputRepositoryError> {
    for (digest, length) in attachments {
        match digests.get(digest) {
            Some(recorded) if recorded != length => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "prospective attachment length disagreement",
                )
                .into());
            }
            Some(_) => {}
            None => {
                *total = total
                    .checked_add(*length)
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "prospective attachment byte length sum",
                    ))?;
                digests.insert(*digest, *length);
            }
        }
    }
    Ok(())
}

pub(super) async fn prepare_attachment_authority_rejection(
    connection: &mut PgConnection,
    command: &SubmitInput,
    maximum_bytes: Option<u64>,
) -> Result<Option<PreparedSubmitInput>, SubmitInputRepositoryError> {
    let digests = command
        .content()
        .parts()
        .iter()
        .filter_map(|part| match part {
            UserContentPart::Attachment { digest, .. } => Some(*digest),
            UserContentPart::Text { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    if digests.is_empty() {
        return Ok(None);
    }

    let mut total_bytes = 0_u64;
    for digest in digests {
        let row = sqlx::query(
            "SELECT blob.byte_length,
                    EXISTS (
                        SELECT 1
                          FROM blob_replica
                         WHERE blob_replica.digest = blob.digest
                    ) AS has_verified_replica
               FROM blob
              WHERE blob.digest = $1",
        )
        .bind(digest.as_bytes().as_slice())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            return Ok(Some(
                command.clone().prepare_attachment_blob_not_found(digest),
            ));
        };
        let has_verified_replica: bool = required(&row, "has_verified_replica")?;
        if !has_verified_replica {
            return Ok(Some(
                command.clone().prepare_attachment_blob_not_found(digest),
            ));
        }
        let byte_length = positive_u64_from_numeric(required(&row, "byte_length")?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("attachment blob byte length"))?;
        let Some(next_total) = total_bytes.checked_add(byte_length) else {
            let maximum_bytes = maximum_bytes.ok_or(SubmitInputCorruption::Inconsistent(
                "attachment authority maximum is unavailable",
            ))?;
            return Ok(Some(
                command
                    .clone()
                    .prepare_attachment_byte_budget_exceeded(maximum_bytes),
            ));
        };
        total_bytes = next_total;
    }

    let maximum_bytes = maximum_bytes.ok_or(SubmitInputCorruption::Inconsistent(
        "attachment authority maximum is unavailable",
    ))?;
    if total_bytes > maximum_bytes {
        return Ok(Some(
            command
                .clone()
                .prepare_attachment_byte_budget_exceeded(maximum_bytes),
        ));
    }
    Ok(None)
}

pub(super) async fn terminalize_retryable_runner_recovery_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
) -> Result<bool, SubmitInputRepositoryError> {
    let row = sqlx::query(crate::lock_inventory::SUBMIT_INPUT_RUNNER_RECOVERY_ATTEMPT)
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .bind(attempt.into_uuid())
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SubmitInputCorruption::Missing(
            "runner recovery interrupted attempt authority",
        ))?;
    let attempt_state: String = required(&row, "state_kind")?;
    let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
    if attempt_state == "terminal" && terminal_disposition.as_deref() == Some("ambiguous") {
        return Ok(true);
    }
    let lease_effect: String = required(&row, "lease_effect_class")?;
    let lease_state: String = required(&row, "lease_state_kind")?;
    if attempt_state != "in_flight"
        || !matches!(
            (lease_state.as_str(), lease_effect.as_str()),
            ("lost_unclaimed", "pure" | "idempotent" | "side_effecting")
                | (
                    "lost_execution_possible" | "lost_claimed",
                    "pure" | "idempotent"
                )
        )
    {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupted attempt stop state",
        )
        .into());
    }
    let preserves_ambiguity = lease_state != "lost_unclaimed" && lease_effect == "idempotent";
    let terminal_disposition = if preserves_ambiguity {
        "ambiguous"
    } else {
        "known_failed"
    };
    let error_kind = if preserves_ambiguity {
        None
    } else {
        Some("crash_lost")
    };
    let changed = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal', terminal_disposition_kind = $1,
                error_kind = $2
          WHERE attempt_id = $3 AND session_id = $4 AND turn_id = $5
            AND state_kind = 'in_flight'",
    )
    .bind(terminal_disposition)
    .bind(error_kind)
    .bind(attempt.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupted attempt retirement",
        )
        .into());
    }
    Ok(preserves_ambiguity)
}

pub(super) async fn persist_runner_recovery_interrupt_effect(
    connection: &mut PgConnection,
    command: DurableCommandId,
    session: SessionId,
    turn: TurnId,
    source_frontier: ContextFrontierId,
) -> Result<(), SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "INSERT INTO turn_runner_recovery_interrupt_effect
            (command_id, session_id, turn_id, placement_event_ordinal,
             runner_id, placement_revision, yielded_turn_attempt_id,
             interrupted_tool_attempt_id, source_frontier_id)
         SELECT $1, lifecycle.session_id, lifecycle.turn_id,
                head.event_ordinal, lifecycle.runner_recovery_runner_id,
                lifecycle.runner_recovery_placement_revision,
                yielded_attempt.turn_attempt_id,
                lifecycle.runner_recovery_tool_attempt_id, $4
           FROM turn_lifecycle AS lifecycle
           JOIN runner_current_session_placement AS head
             ON head.session_id = lifecycle.session_id
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = head.session_id
            AND placement.event_ordinal = head.event_ordinal
           JOIN turn_attempt AS yielded_attempt
             ON yielded_attempt.turn_id = lifecycle.turn_id
            AND yielded_attempt.session_id = lifecycle.session_id
            AND yielded_attempt.state_kind = 'ended'
            AND yielded_attempt.end_variant = 'without_stop'
            AND yielded_attempt.end_disposition = 'yielded_to_durable_wait'
            AND yielded_attempt.interrupt_command_id IS NULL
            AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_attempt AS continuation
                 WHERE continuation.continued_from_attempt_id =
                        yielded_attempt.turn_attempt_id
            )
          WHERE lifecycle.session_id = $2
            AND lifecycle.turn_id = $3
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'
            AND placement.state_kind IN ('runner_lost', 'runner_lost_before_pin')
            AND placement.lost_runner_id = lifecycle.runner_recovery_runner_id
            AND placement.placement_revision =
                lifecycle.runner_recovery_placement_revision
            AND placement.interrupted_tool_attempt_id IS NOT DISTINCT FROM
                lifecycle.runner_recovery_tool_attempt_id
            AND (
                (
                    lifecycle.active_tool_round_call_id IS NULL
                    AND $4 = lifecycle.starting_frontier_id
                )
                OR EXISTS (
                    SELECT 1
                      FROM tool_round AS round
                     WHERE round.producing_model_call_id =
                            lifecycle.active_tool_round_call_id
                       AND round.turn_id = lifecycle.turn_id
                       AND round.session_id = lifecycle.session_id
                       AND round.boundary_kind = 'continuing'
                       AND round.boundary_frontier_id = $4
                )
            )",
    )
    .bind(durable_command_id_to_uuid(command))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(source_frontier.into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if rows != 1 {
        return Err(SubmitInputCorruption::Inconsistent("runner recovery interrupt effect").into());
    }
    Ok(())
}

pub(super) async fn load_runner_recovery_yielded_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<TurnAttemptId, SubmitInputRepositoryError> {
    let attempt = sqlx::query_scalar::<_, Uuid>(
        "SELECT attempt.turn_attempt_id
           FROM turn_lifecycle AS lifecycle
           JOIN turn_attempt AS attempt
             ON attempt.turn_id = lifecycle.turn_id
            AND attempt.session_id = lifecycle.session_id
            AND attempt.state_kind = 'ended'
            AND attempt.end_variant = 'without_stop'
            AND attempt.end_disposition = 'yielded_to_durable_wait'
            AND attempt.interrupt_command_id IS NULL
            AND attempt.interrupt_predecessor_turn_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_attempt AS continuation
                 WHERE continuation.continued_from_attempt_id =
                        attempt.turn_attempt_id
            )
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(SubmitInputCorruption::Missing(
        "runner recovery yielded attempt",
    ))?;
    Ok(TurnAttemptId::from_uuid(attempt))
}
