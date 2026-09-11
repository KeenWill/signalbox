use std::collections::BTreeMap;

use signalbox_domain::{
    ContextFrontierId, PreparedToolResultProjection, ReconstitutedToolAttempt,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntryId, SessionId, ToolAttemptEnd,
    ToolBatch, TurnId,
};
use sqlx::{PgConnection, types::Uuid};

use super::{
    ToolLoopCorruption, ToolLoopRepositoryError, load_foreground_delegation_outcome, load_snapshot,
    map_model_call_error,
};

pub(crate) async fn load(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<ResolvedContextFrontierSnapshot>, ToolLoopRepositoryError> {
    let frontier: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(compaction.result_frontier_id, lifecycle.compaction_frontier_id)
           FROM turn_lifecycle AS lifecycle
           LEFT JOIN context_compaction AS compaction
             ON compaction.session_id = lifecycle.session_id
            AND compaction.source_frontier_id = lifecycle.compaction_frontier_id
          WHERE lifecycle.session_id = $1 AND lifecycle.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    match frontier {
        Some(frontier) => {
            load_snapshot(connection, session, ContextFrontierId::from_uuid(frontier))
                .await
                .map(Some)
        }
        None => Ok(None),
    }
}

pub(super) async fn prepare_results(
    connection: &mut PgConnection,
    batch: &ToolBatch,
    entries: Vec<SemanticTranscriptEntryId>,
    frontier: ContextFrontierId,
) -> Result<PreparedToolResultProjection, ToolLoopRepositoryError> {
    let mut child_outcomes = BTreeMap::new();
    for request in batch.requests() {
        if let Some(ReconstitutedToolAttempt::Ended(attempt)) = batch.attempt(request.id())
            && let ToolAttemptEnd::AwaitingChild {
                spawning_request,
                child,
            } = attempt.end()
        {
            child_outcomes.insert(
                request.id(),
                load_foreground_delegation_outcome(
                    connection,
                    batch.session(),
                    request.id(),
                    *spawning_request,
                    *child,
                )
                .await?,
            );
        }
    }
    batch
        .prepare_delegation_result_projection(entries, frontier, child_outcomes)
        .map_err(|_| {
            ToolLoopRepositoryError::InvalidTransition("tool batch is not ready for continuation")
        })
}

pub(crate) async fn load_result_projection(
    connection: &mut PgConnection,
    batch: &ToolBatch,
    mut frontier: ContextFrontierId,
    retained_scheduling: Option<&signalbox_domain::AcceptedInputSchedulingProjection>,
) -> Result<PreparedToolResultProjection, ToolLoopRepositoryError> {
    let session = batch.session();
    let result_count = batch.yielded_snapshot().entry_count() + batch.requests().len();
    let mut snapshot = load_snapshot(connection, session, frontier).await?;
    let mut boundaries = Vec::new();
    while snapshot.entry_count() > result_count {
        boundaries.push(snapshot);
        let prefix: Uuid = sqlx::query_scalar(
            "SELECT prefix_context_frontier_id FROM context_frontier WHERE context_frontier_id = $1",
        ).bind(frontier.into_uuid()).fetch_one(&mut *connection).await?;
        frontier = ContextFrontierId::from_uuid(prefix);
        snapshot = load_snapshot(connection, session, frontier).await?;
    }
    let entries = snapshot
        .ordered_entries()
        .skip(batch.yielded_snapshot().entry_count())
        .map(|entry| entry.entry())
        .collect();
    let mut projection = prepare_results(connection, batch, entries, frontier).await?;
    boundaries.reverse();
    if !boundaries.is_empty() {
        let owned_scheduling;
        let scheduling = if let Some(scheduling) = retained_scheduling {
            scheduling
        } else {
            let loaded = crate::session::load_session_from_connection(connection, session)
                .await
                .map_err(|error| match error {
                    crate::session::SessionRepositoryError::Database(error) => {
                        ToolLoopRepositoryError::from(error)
                    }
                    crate::session::SessionRepositoryError::Corruption(_) => {
                        ToolLoopCorruption::Inconsistent("compaction session").into()
                    }
                })?
                .ok_or(ToolLoopCorruption::Missing("compaction session"))?;
            let loaded_scheduling = Box::pin(crate::submit_input::load_scheduling_projection(
                connection, loaded,
            ))
            .await
            .map_err(crate::model_execution::map_scheduling_error)
            .map_err(map_model_call_error)?;
            owned_scheduling = loaded_scheduling;
            &owned_scheduling
        };
        for snapshot in boundaries {
            let reference = snapshot
                .ordered_entries()
                .last()
                .ok_or(ToolLoopCorruption::Missing("checkpoint boundary entry"))?;
            let entry = scheduling
                .semantic_entry(reference)
                .cloned()
                .ok_or(ToolLoopCorruption::Missing("checkpoint boundary entry"))?;
            projection = projection
                .with_context_boundary(entry, snapshot)
                .map_err(|_| ToolLoopCorruption::Inconsistent("checkpoint boundary projection"))?;
        }
    }
    Ok(projection)
}
