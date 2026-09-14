//! Checked replacement of a runner retained by an unresolved tool round.

use super::*;
use signalbox_domain::{DurableCommandId, ReconstitutedToolAttempt};

pub(super) struct RecoveryTakeover {
    turn: TurnId,
    yielded: TurnAttemptId,
    producing_call: Option<Uuid>,
    interrupted: Option<ToolAttemptId>,
    source_lease: Option<(RunnerLeaseId, RunnerGeneration)>,
    retained_loss_ordinal: Decimal,
}

impl RunnerProtocolStore {
    pub(super) async fn recovery_takeover_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        session: SessionId,
    ) -> Result<Option<RecoveryTakeover>, RunnerProtocolStoreError> {
        let Some(row) = sqlx::query(crate::lock_inventory::RUNNER_TAKEOVER_WAIT)
            .bind(session.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?
        else {
            return Ok(None);
        };
        let turn = TurnId::from_uuid(row.decode_column("turn_id")?);
        let yielded = sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_attempt_id FROM turn_attempt
             WHERE session_id = $1 AND turn_id = $2 AND state_kind = 'ended'
               AND end_variant = 'without_stop' AND end_disposition = 'yielded_to_durable_wait'
               AND NOT EXISTS (SELECT 1 FROM turn_attempt AS successor
                   WHERE successor.continued_from_attempt_id = turn_attempt.turn_attempt_id)",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        let interrupted = row
            .decode_column::<Option<Uuid>>("runner_recovery_tool_attempt_id")?
            .map(tool_attempt_id);
        let producing_call: Option<Uuid> = row.decode_column("active_tool_round_call_id")?;
        let mut source_lease = None;
        if let Some(attempt) = interrupted {
            let locked = sqlx::query(crate::lock_inventory::RUNNER_TAKEOVER_ATTEMPT)
                .bind(attempt.into_uuid())
                .fetch_one(&mut **transaction)
                .await?;
            let issuing =
                TurnAttemptId::from_uuid(locked.decode_column("issuing_turn_attempt_id")?);
            let batch = crate::tool_loop::load_runner_recovery_cancellation_batch(
                transaction.as_mut(),
                session,
                turn,
                issuing,
                Some(attempt),
            )
            .await
            .map_err(dispatch::tool_error)?
            .ok_or_else(dispatch::invalid)?;
            let request = ToolRequestId::from_uuid(locked.decode_column("request_id")?);
            let lease = self
                .attempt_lease_in(transaction, attempt)
                .await?
                .ok_or_else(dispatch::invalid)?;
            let eligible = match batch.attempt(request) {
                Some(ReconstitutedToolAttempt::Current(current))
                    if current.attempt() == attempt =>
                {
                    lease.state() == RunnerLeaseState::LostUnclaimed
                        || (matches!(
                            lease.state(),
                            RunnerLeaseState::LostClaimed | RunnerLeaseState::LostExecutionPossible
                        ) && matches!(
                            lease.effect(),
                            RunnerToolEffectClass::Pure | RunnerToolEffectClass::Idempotent
                        ))
                }
                Some(ReconstitutedToolAttempt::Ended(ended)) if ended.attempt() == attempt => {
                    lease.state() == RunnerLeaseState::Refused
                        && matches!(ended.end(), ToolAttemptEnd::KnownFailed { error }
                        if error.kind() == signalbox_domain::ToolExecutionErrorKind::ExecutionFailed)
                }
                _ => false,
            };
            if !eligible {
                return Err(dispatch::invalid());
            }
            source_lease = Some((lease.correlation().lease, lease.generation()));
        }
        if let Some(call) = producing_call {
            sqlx::query(crate::lock_inventory::RUNNER_TAKEOVER_BATCH)
                .bind(call)
                .fetch_one(&mut **transaction)
                .await?;
        }
        let retained_loss_ordinal = sqlx::query_scalar::<_, Decimal>(
            "SELECT runner_recovery_loss_ordinal(session_id, $2, event_ordinal) FROM runner_current_session_placement WHERE session_id = $1",
        ).bind(session.into_uuid()).bind(turn.into_uuid()).fetch_one(&mut **transaction).await?;
        Ok(Some(RecoveryTakeover {
            turn,
            yielded: TurnAttemptId::from_uuid(yielded),
            producing_call,
            interrupted,
            source_lease,
            retained_loss_ordinal,
        }))
    }
}

pub(super) async fn retain_takeover(
    transaction: &mut Transaction<'_, Postgres>,
    command: DurableCommandId,
    session: SessionId,
    source_ordinal: u64,
    successor_ordinal: u64,
    takeover: RecoveryTakeover,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_recovery_takeover
         (command_id, session_id, turn_id, source_event_ordinal, successor_event_ordinal,
          yielded_turn_attempt_id, producing_model_call_id, interrupted_tool_attempt_id,
          source_lease_id, source_generation, retained_loss_event_ordinal)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(command.into_uuid())
    .bind(session.into_uuid())
    .bind(takeover.turn.into_uuid())
    .bind(Decimal::from(source_ordinal))
    .bind(Decimal::from(successor_ordinal))
    .bind(takeover.yielded.into_uuid())
    .bind(takeover.producing_call)
    .bind(takeover.interrupted.map(ToolAttemptId::into_uuid))
    .bind(takeover.source_lease.map(|(lease, _)| lease.into_uuid()))
    .bind(
        takeover
            .source_lease
            .map(|(_, generation)| Decimal::from(generation.get())),
    )
    .bind(takeover.retained_loss_ordinal)
    .execute(&mut **transaction)
    .await?;
    let refusal_recorded: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runner_current_lease_event AS head
         JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
         WHERE head.lease_id = $1 AND head.generation = $2 AND event.state_kind = 'refused')",
    )
    .bind(takeover.source_lease.map(|(lease, _)| lease.into_uuid()))
    .bind(
        takeover
            .source_lease
            .map(|(_, generation)| Decimal::from(generation.get())),
    )
    .fetch_one(&mut **transaction)
    .await?;
    if takeover.interrupted.is_none() || refusal_recorded {
        if takeover.producing_call.is_none() {
            let prior = crate::tool_loop::load_runner_recovery_source_snapshot(
                transaction.as_mut(),
                session,
                takeover.turn,
            )
            .await
            .map_err(dispatch::tool_error)?
            .ok_or_else(dispatch::invalid)?;
            append_takeover_boundaries(transaction.as_mut(), session, &prior).await?;
        }
        let row = sqlx::query("SELECT * FROM runner_recovery_takeover WHERE command_id = $1")
            .bind(command.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
        resume_takeover_in(transaction, &row).await?;
    }
    Ok(())
}

impl RunnerProtocolStore {
    /// Loads the retained source lease whose installed successor can accept retry work.
    pub async fn pending_runner_recovery_source(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT takeover.source_lease_id, takeover.source_generation FROM runner_recovery_takeover AS takeover
             JOIN turn_lifecycle AS turn USING (session_id, turn_id)
             JOIN runner_current_session_placement AS head USING (session_id)
             JOIN runner_session_placement_record AS placement
               ON placement.session_id = head.session_id AND placement.event_ordinal = head.event_ordinal
             WHERE turn.state_kind = 'active' AND turn.active_phase_kind = 'awaiting_runner_recovery'
               AND NOT turn.delegation_runtime_terminal AND takeover.successor_event_ordinal = head.event_ordinal
               AND placement.registration_enrollment_id = $1 AND takeover.source_lease_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM runner_lease_generation AS retry WHERE retry.lease_id = takeover.source_lease_id AND retry.generation > takeover.source_generation)
             ORDER BY takeover.command_id LIMIT 1",
        ).bind(enrollment.into_uuid()).fetch_optional(&mut *transaction).await?;
        let source = match row {
            Some(row) => {
                self.load_lease_in(
                    &mut transaction,
                    runner_lease_id(row.decode_column("source_lease_id")?),
                    decode_generation(row.decode_column("source_generation")?)?,
                )
                .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(source)
    }

    /// Offers the retry authorized by an installed takeover on this connection.
    /// The caller holds the same serial dispatch permit as ordinary tool offers.
    pub async fn offer_runner_recovery_retry(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        turn: TurnId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let session = sqlx::query_scalar::<_, Uuid>(
            "SELECT takeover.session_id FROM runner_recovery_takeover AS takeover
             JOIN turn_lifecycle AS turn USING (session_id, turn_id)
             JOIN runner_current_session_placement AS head USING (session_id)
             JOIN runner_session_placement_record AS placement
               ON placement.session_id = head.session_id AND placement.event_ordinal = head.event_ordinal
             WHERE turn.state_kind = 'active' AND turn.active_phase_kind = 'awaiting_runner_recovery'
               AND NOT turn.delegation_runtime_terminal
               AND takeover.successor_event_ordinal = head.event_ordinal
               AND placement.registration_enrollment_id = $1
               AND takeover.source_lease_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM runner_lease_generation AS retry WHERE retry.lease_id = takeover.source_lease_id AND retry.generation > takeover.source_generation) AND takeover.turn_id = $2
             ORDER BY takeover.command_id LIMIT 1",
        ).bind(enrollment.into_uuid()).bind(turn.into_uuid()).fetch_optional(&mut *transaction).await?;
        let Some(session) = session else {
            return Ok(None);
        };
        let session = session_id(session);
        crate::tool_loop::lock_tool_session(transaction.as_mut(), session)
            .await
            .map_err(dispatch::tool_error)?;
        let row = sqlx::query(
            "SELECT takeover.*, source.issuing_turn_attempt_id FROM runner_recovery_takeover AS takeover
             JOIN turn_lifecycle AS turn USING (session_id, turn_id)
             JOIN runner_current_session_placement AS head USING (session_id)
             JOIN tool_attempt AS source ON source.attempt_id = takeover.interrupted_tool_attempt_id
             WHERE takeover.session_id = $1 AND takeover.successor_event_ordinal = head.event_ordinal
               AND turn.state_kind = 'active' AND turn.active_phase_kind = 'awaiting_runner_recovery'
               AND NOT turn.delegation_runtime_terminal
               AND NOT EXISTS (SELECT 1 FROM runner_lease_generation AS retry WHERE retry.lease_id = takeover.source_lease_id AND retry.generation > takeover.source_generation)",
        ).bind(session.into_uuid()).fetch_optional(&mut *transaction).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let turn = TurnId::from_uuid(row.decode_column("turn_id")?);
        let issuing = TurnAttemptId::from_uuid(row.decode_column("issuing_turn_attempt_id")?);
        let source = tool_attempt_id(row.decode_column("interrupted_tool_attempt_id")?);
        let loss = self
            .load_lease_loss_in(
                &mut transaction,
                runner_lease_id(row.decode_column("source_lease_id")?),
                decode_generation(row.decode_column("source_generation")?)?,
            )
            .await?
            .ok_or_else(dispatch::invalid)?;
        let batch = crate::tool_loop::load_runner_recovery_cancellation_batch(
            transaction.as_mut(),
            session,
            turn,
            issuing,
            Some(source),
        )
        .await
        .map_err(dispatch::tool_error)?
        .ok_or_else(dispatch::invalid)?;
        let retry_authority = loss.retry().ok_or_else(dispatch::invalid)?;
        let (retired, authorization) = if loss.lost().state() == RunnerLeaseState::LostUnclaimed {
            let (_, authorization) = retry_authority
                .prepare_unclaimed_attempt(batch)
                .map_err(RunnerProtocolStoreError::Domain)?
                .into_parts();
            (None, authorization)
        } else {
            let prepared = retry_authority
                .prepare_claimed_attempt(batch, tool_attempt_id(Uuid::now_v7()))
                .map_err(RunnerProtocolStoreError::Domain)?;
            self.store_claimed_retry_attempt_authority_in(&mut transaction, &loss, &prepared)
                .await?;
            let (_, retired, authorization) = prepared.into_parts();
            (Some(retired), authorization)
        };
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = load_connection_head_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or_else(dispatch::invalid)?;
        if current.epoch() != epoch || current.state() != RunnerConnectionState::Connected {
            return Err(dispatch::invalid());
        }
        let enrolled = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or_else(dispatch::invalid)?;
        let revision = sqlx::query_scalar::<_, Decimal>(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment,
            decode_registration_revision(revision)?,
            Some(&enrolled),
            &self.catalog,
        )
        .await?
        .ok_or_else(dispatch::invalid)?;
        let placement = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        if placement.decode_column::<Decimal>("event_ordinal")?
            != row.decode_column::<Decimal>("successor_event_ordinal")?
        {
            return Err(dispatch::invalid());
        }
        let (_, placement, _, grant, _) = self
            .decode_stored_placement_in(&mut transaction, &placement)
            .await?
            .into_parts();
        let retry = placement
            .offer_retry(
                &enrolled,
                registration.registration(),
                grant.as_ref(),
                loss,
                authorization,
            )
            .map_err(RunnerProtocolStoreError::Domain)?;
        if let Some(retired) = retired {
            self.store_claimed_retry_replacement_in(&mut transaction, &retired, &retry)
                .await?;
        } else {
            append_lease_event_in(&mut transaction, &retry).await?;
        }
        commit_mutation(transaction).await?;
        Ok(Some(retry))
    }
}

pub(super) async fn finish_recovery_retry_in(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let row = sqlx::query(
        "SELECT takeover.* FROM runner_recovery_takeover AS takeover
         JOIN turn_lifecycle AS turn USING (session_id, turn_id)
         JOIN runner_lease_generation AS retry ON retry.lease_id = takeover.source_lease_id
           AND ((retry.predecessor_generation = takeover.source_generation
                 AND retry.placement_event_ordinal = takeover.successor_event_ordinal)
             OR (retry.generation = takeover.source_generation AND NOT EXISTS (
                 SELECT 1 FROM runner_lease_generation AS later WHERE later.lease_id = retry.lease_id AND later.generation > retry.generation)))
         JOIN runner_current_session_placement AS head ON head.session_id = takeover.session_id
         WHERE retry.lease_id = $1 AND retry.generation = $2
           AND takeover.successor_event_ordinal = head.event_ordinal
           AND turn.active_phase_kind = 'awaiting_runner_recovery'
           AND turn.runner_recovery_tool_attempt_id = takeover.interrupted_tool_attempt_id",
    ).bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get()))
        .fetch_optional(&mut **transaction).await?;
    if let Some(row) = row {
        let ambiguous: bool = sqlx::query_scalar(
            "SELECT terminal_disposition_kind = 'ambiguous' FROM tool_attempt WHERE attempt_id = $1",
        ).bind(lease.attempt().into_uuid()).fetch_one(&mut **transaction).await?;
        if ambiguous {
            sqlx::query(
                "UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_tool_recovery',
                current_attempt_id = $1, recovery_tool_attempt_id = $2,
                runner_recovery_runner_id = NULL, runner_recovery_placement_revision = NULL,
                runner_recovery_tool_attempt_id = NULL WHERE session_id = $3 AND turn_id = $4",
            )
            .bind(row.decode_column::<Uuid>("yielded_turn_attempt_id")?)
            .bind(lease.attempt().into_uuid())
            .bind(row.decode_column::<Uuid>("session_id")?)
            .bind(row.decode_column::<Uuid>("turn_id")?)
            .execute(&mut **transaction)
            .await?;
        } else {
            resume_takeover_in(transaction, &row).await?;
        }
    }
    Ok(())
}

async fn resume_takeover_in(
    transaction: &mut Transaction<'_, Postgres>,
    takeover: &PgRow,
) -> Result<(), RunnerProtocolStoreError> {
    let continuation = Uuid::now_v7();
    let session: Uuid = takeover.decode_column("session_id")?;
    let turn: Uuid = takeover.decode_column("turn_id")?;
    sqlx::query("INSERT INTO turn_attempt (turn_attempt_id, session_id, turn_id, continued_from_attempt_id, state_kind)
        VALUES ($1, $2, $3, $4, 'prepared')")
        .bind(continuation).bind(session).bind(turn).bind(takeover.decode_column::<Uuid>("yielded_turn_attempt_id")?)
        .execute(&mut **transaction).await?;
    sqlx::query(
        "UPDATE turn_lifecycle SET active_phase_kind = 'running', current_attempt_id = $1,
        runner_recovery_runner_id = NULL, runner_recovery_placement_revision = NULL,
        runner_recovery_tool_attempt_id = NULL
        WHERE session_id = $2 AND turn_id = $3 AND active_phase_kind = 'awaiting_runner_recovery'",
    )
    .bind(continuation)
    .bind(session)
    .bind(turn)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn append_takeover_boundaries(
    connection: &mut PgConnection,
    session: SessionId,
    prior: &signalbox_domain::ResolvedContextFrontierSnapshot,
) -> Result<Vec<signalbox_domain::RunnerPlacementBoundary>, RunnerProtocolStoreError> {
    let rows = sqlx::query(
        "SELECT takeover.command_id, takeover.successor_event_ordinal,
            source.placement_revision AS prior_revision, successor.placement_revision
         FROM runner_recovery_takeover AS takeover
         JOIN runner_session_placement_record AS source ON source.session_id = takeover.session_id
            AND source.event_ordinal = takeover.source_event_ordinal
         JOIN runner_session_placement_record AS successor ON successor.session_id = takeover.session_id
            AND successor.event_ordinal = takeover.successor_event_ordinal
         WHERE takeover.session_id = $1 AND successor.event_kind = 'runner_replaced' AND NOT EXISTS (
            SELECT 1 FROM runner_placement_boundary AS boundary WHERE boundary.command_id = takeover.command_id)
         ORDER BY takeover.successor_event_ordinal",
    ).bind(session.into_uuid()).fetch_all(&mut *connection).await?;
    let mut boundaries: Vec<signalbox_domain::RunnerPlacementBoundary> = Vec::new();
    for row in rows {
        let revision = decode_generation(row.decode_column("placement_revision")?)?;
        let boundary = signalbox_domain::RunnerPlacementBoundary::prepare_revision(
            session,
            decode_generation(row.decode_column("prior_revision")?)?,
            revision,
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            signalbox_domain::ContextFrontierId::from_uuid(Uuid::now_v7()),
            Some(
                boundaries
                    .last()
                    .map_or(prior, |boundary| boundary.frontier()),
            ),
        )
        .map_err(RunnerProtocolStoreError::Domain)?;
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, runner_placement_revision)
            VALUES ($1, $2, 'runner_placement_changed', $3)",
        )
        .bind(session.into_uuid())
        .bind(boundary.entry().identity().into_uuid())
        .bind(Decimal::from(revision.get()))
        .execute(&mut *connection)
        .await?;
        crate::model_execution::insert_snapshot(connection, boundary.frontier())
            .await
            .map_err(recovery::map_frontier_error)?;
        sqlx::query("INSERT INTO runner_placement_boundary
            (session_id, placement_revision, event_ordinal, command_id, semantic_entry_id, context_frontier_id)
            VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(session.into_uuid()).bind(Decimal::from(revision.get()))
            .bind(row.decode_column::<Decimal>("successor_event_ordinal")?)
            .bind(row.decode_column::<Uuid>("command_id")?).bind(boundary.entry().identity().into_uuid())
            .bind(boundary.frontier().frontier().snapshot().into_uuid()).execute(&mut *connection).await?;
        sqlx::query("INSERT INTO runner_session_placement_frontier (session_id, placement_revision)
            VALUES ($1, $2) ON CONFLICT (session_id) DO UPDATE SET placement_revision = EXCLUDED.placement_revision")
            .bind(session.into_uuid()).bind(Decimal::from(revision.get())).execute(&mut *connection).await?;
        boundaries.push(boundary);
    }
    Ok(boundaries)
}
