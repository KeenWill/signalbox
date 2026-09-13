//! Atomic local runner admission, claim, and terminal tool-attempt recording.

use super::*;
use signalbox_domain::{
    ReconstitutedToolAttempt, RunnerLeaseOfferRequest, RunnerToolPermissionOverride,
    ToolApprovalPosture, ToolAttemptObservation, ToolDispatchAuthority,
};

impl RunnerProtocolStore {
    /// Reads the attached runner's approval posture without issuing authority.
    pub async fn runner_tool_posture(
        &self,
        session: SessionId,
        tool: &ToolName,
    ) -> Result<Option<ToolApprovalPosture>, RunnerProtocolStoreError> {
        let Some(stored) = self.load_placement(session).await? else {
            return Ok(None);
        };
        let mut transaction = self.pool.begin().await?;
        let registration = connected_registration(&mut transaction, &self.catalog).await?;
        let posture = registration.as_ref().and_then(|(_, registration)| {
            let request = stored.placement().request();
            (request
                .validate_registration(registration.registration())
                .is_ok()
                && registration.registration().tool(tool).is_some()
                && request.sandbox == RunnerSandboxProfile::Ambient
                && request.workspace == WorkspaceRequirement::None)
                .then(|| requested_posture(request, tool))
        });
        transaction.rollback().await?;
        Ok(posture)
    }

    /// Atomically pins and offers, or offers from an existing pin, for an issued attempt.
    /// `None` preserves daemon-local execution without issuing runner authority.
    pub async fn offer_tool_dispatch(
        &self,
        authority: &ToolDispatchAuthority,
        lease_id: RunnerLeaseId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let correlation = authority.correlation();
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.session())
            .await
            .map_err(tool_error)?;
        if let Some(lease) = self
            .attempt_lease_in(&mut transaction, correlation.attempt())
            .await?
        {
            if lease.correlation().dispatch != correlation
                || lease.arguments() != authority.request().arguments()
            {
                return Err(invalid());
            }
            transaction.rollback().await?;
            return Ok(Some(lease));
        }
        let registration = connected_registration(&mut transaction, &self.catalog).await?;
        let row = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(correlation.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored = self
            .decode_stored_placement_in(&mut transaction, &row)
            .await?;
        let (_, placement, _, grant, interrupted) = stored.into_parts();
        let Some((enrollment, registration)) = registration else {
            return if placement.state() == &SessionRunnerPlacementState::Unpinned {
                Ok(None)
            } else {
                Err(invalid())
            };
        };
        if interrupted.is_some() {
            return Err(invalid());
        }
        let tool = authority.request().name();
        if registration.registration().tool(tool).is_none() {
            return Ok(None);
        }
        if let Err(error) = placement
            .request()
            .validate_registration(registration.registration())
        {
            return if placement.state() == &SessionRunnerPlacementState::Unpinned {
                Ok(None)
            } else {
                Err(RunnerProtocolStoreError::Domain(error))
            };
        }
        if placement.request().sandbox != RunnerSandboxProfile::Ambient
            || placement.request().workspace != WorkspaceRequirement::None
        {
            return if placement.state() == &SessionRunnerPlacementState::Unpinned {
                Ok(None)
            } else {
                Err(invalid())
            };
        }
        if requested_posture(placement.request(), tool) == ToolApprovalPosture::Human
            && authority.request().approval_posture() != ToolApprovalPosture::Human
        {
            return Ok(None);
        }
        let batch = crate::tool_loop::load_active_batch_from_connection(
            transaction.as_mut(),
            correlation.session(),
            correlation.turn(),
        )
        .await
        .map_err(tool_error)?
        .ok_or_else(invalid)?;
        let authorization = batch
            .resume_runner_attempt(correlation.attempt())
            .map_err(|_| invalid())?;
        let offer = RunnerLeaseOfferRequest {
            lease: lease_id,
            tool: tool.clone(),
        };
        let lease = match placement.state() {
            SessionRunnerPlacementState::Unpinned => {
                let directory = match &placement.request().working_directory {
                    WorkingDirectorySelection::Exact(directory) => directory.clone(),
                    WorkingDirectorySelection::RunnerDefault => registration
                        .registration()
                        .default_working_directory()
                        .cloned()
                        .ok_or_else(invalid)?,
                };
                let pin = placement
                    .pin_and_offer_lease(
                        &enrollment,
                        registration.registration(),
                        directory,
                        None,
                        authorization,
                        offer,
                    )
                    .map_err(RunnerProtocolStoreError::Domain)?;
                self.store_pin_in(&mut transaction, &pin, &registration)
                    .await?;
                pin.lease
            }
            SessionRunnerPlacementState::Pinned(_) => {
                let lease = placement
                    .offer_lease(
                        &enrollment,
                        registration.registration(),
                        grant.as_ref(),
                        authorization,
                        offer,
                    )
                    .map_err(RunnerProtocolStoreError::Domain)?;
                append_lease_event_in(&mut transaction, &lease).await?;
                lease
            }
            _ => return Err(invalid()),
        };
        commit_mutation(transaction).await?;
        Ok(Some(lease))
    }

    /// Reloads the exact durable lease associated with a physical attempt.
    pub async fn load_attempt_lease(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let session = sqlx::query_scalar::<_, Uuid>(
            "SELECT session_id FROM tool_attempt WHERE attempt_id = $1",
        )
        .bind(attempt.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(invalid)?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), session_id(session))
            .await
            .map_err(tool_error)?;
        let lease = self.attempt_lease_in(&mut transaction, attempt).await?;
        transaction.commit().await?;
        Ok(lease)
    }

    async fn attempt_lease_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        attempt: ToolAttemptId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let row = sqlx::query("SELECT lease_id, generation FROM runner_lease_generation WHERE attempt_id = $1 ORDER BY generation DESC LIMIT 1")
            .bind(attempt.into_uuid()).fetch_optional(&mut **transaction).await?;
        match row {
            Some(row) => {
                self.load_lease_in(
                    transaction,
                    runner_lease_id(row.decode_column("lease_id")?),
                    decode_generation(row.decode_column("generation")?)?,
                )
                .await
            }
            None => Ok(None),
        }
    }

    /// Loads one offered lease belonging to this exact physical connection.
    pub async fn pending_tool_lease(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query("SELECT generation.lease_id, generation.generation
            FROM runner_lease_generation AS generation
            JOIN runner_current_lease_event AS head USING (lease_id, generation)
            JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
            WHERE generation.registration_enrollment_id = $1 AND generation.offer_connection_epoch = $2 AND event.state_kind = 'offered'
            ORDER BY generation.lease_id LIMIT 1")
            .bind(enrollment.into_uuid()).bind(Decimal::from(epoch.get())).fetch_optional(&mut *transaction).await?;
        let lease = match row {
            Some(row) => {
                self.load_lease_in(
                    &mut transaction,
                    runner_lease_id(row.decode_column("lease_id")?),
                    decode_generation(row.decode_column("generation")?)?,
                )
                .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(lease)
    }

    /// Commits the exact claim before the caller issues execution capability.
    pub async fn claim_tool_lease(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        correlation: RunnerLeaseCorrelation,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.dispatch.session())
            .await
            .map_err(tool_error)?;
        validate_connection(&mut transaction, enrollment, epoch, &correlation).await?;
        sqlx::query(RUNNER_LEASE_HEAD)
            .bind(correlation.lease.into_uuid())
            .bind(Decimal::from(correlation.generation.get()))
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(invalid)?;
        let lease = self
            .load_lease_in(&mut transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or_else(invalid)?;
        if lease.correlation() != correlation {
            return Err(invalid());
        }
        let lease = match lease.state() {
            RunnerLeaseState::Offered => {
                let lease = lease
                    .claim(correlation)
                    .map_err(RunnerProtocolStoreError::Domain)?;
                append_lease_event_in(&mut transaction, &lease).await?;
                lease
            }
            RunnerLeaseState::Claimed => lease,
            _ => return Err(invalid()),
        };
        commit_mutation(transaction).await?;
        Ok(lease)
    }

    /// Commits terminal attempt evidence and the completed lease in one transaction.
    pub async fn record_tool_lease_result(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        correlation: RunnerLeaseCorrelation,
        observation: ToolAttemptObservation,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.dispatch.session())
            .await
            .map_err(tool_error)?;
        validate_connection(&mut transaction, enrollment, epoch, &correlation).await?;
        sqlx::query(RUNNER_LEASE_HEAD)
            .bind(correlation.lease.into_uuid())
            .bind(Decimal::from(correlation.generation.get()))
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(invalid)?;
        let lease = self
            .load_lease_in(&mut transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or_else(invalid)?;
        if lease.correlation() != correlation {
            return Err(invalid());
        }
        if lease.state() == RunnerLeaseState::Completed {
            let attempt = crate::tool_loop::load_attempts_by_id(
                transaction.as_mut(),
                &[correlation.dispatch.attempt()],
            )
            .await
            .map_err(tool_error)?
            .remove(&correlation.dispatch.attempt())
            .ok_or_else(invalid)?;
            let ReconstitutedToolAttempt::Ended(ended) = attempt else {
                return Err(invalid());
            };
            let matches = match (ended.end(), &observation) {
                (
                    ToolAttemptEnd::Completed { result: prior },
                    ToolAttemptObservation::Completed { result },
                ) => prior == result,
                (
                    ToolAttemptEnd::KnownFailed { error: prior },
                    ToolAttemptObservation::KnownFailed { error },
                ) => prior == error,
                (ToolAttemptEnd::Ambiguous, ToolAttemptObservation::Ambiguous) => true,
                _ => false,
            };
            if !matches {
                return Err(invalid());
            }
            transaction.rollback().await?;
            return Ok(lease);
        }
        let batch = crate::tool_loop::load_active_batch_from_connection(
            transaction.as_mut(),
            correlation.dispatch.session(),
            correlation.dispatch.turn(),
        )
        .await
        .map_err(tool_error)?
        .ok_or_else(invalid)?;
        let authorized = batch
            .resume_in_flight_attempt(correlation.dispatch.attempt())
            .map_err(|_| invalid())?;
        if authorized.correlation() != correlation.dispatch {
            return Err(invalid());
        }
        let observed = authorized.executor_fence().bind(observation);
        let (current, _) = authorized.into_parts();
        let ended = current
            .apply_terminal_observation(observed)
            .map_err(|_| invalid())?;
        let completed = lease
            .complete(correlation)
            .map_err(RunnerProtocolStoreError::Domain)?;
        crate::tool_loop::persist_ended_attempt(transaction.as_mut(), &ended)
            .await
            .map_err(tool_error)?;
        append_lease_event_in(&mut transaction, &completed).await?;
        commit_mutation(transaction).await?;
        Ok(completed)
    }
}

fn requested_posture(
    request: &SessionRunnerPlacementRequest,
    tool: &ToolName,
) -> ToolApprovalPosture {
    match request.permission_overrides.get(tool) {
        Some(RunnerToolPermissionOverride::Confirm) => ToolApprovalPosture::Human,
        Some(RunnerToolPermissionOverride::Auto) | None => ToolApprovalPosture::Auto,
    }
}

async fn connected_registration(
    transaction: &mut Transaction<'_, Postgres>,
    catalog: &RunnerCatalog,
) -> Result<Option<(RunnerEnrollment, StoredValidatedRunnerRegistration)>, RunnerProtocolStoreError>
{
    let enrollment =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::RUNNER_CREATION_ENROLLMENT)
            .fetch_optional(&mut **transaction)
            .await?;
    let Some(enrollment) = enrollment else {
        return Ok(None);
    };
    let enrollment = runner_enrollment_id(enrollment);
    let current = load_connection_head_in(transaction.as_mut(), enrollment).await?;
    if !current.is_some_and(|connection| connection.state() == RunnerConnectionState::Connected) {
        return Ok(None);
    }
    let revision = sqlx::query_scalar::<_, Decimal>(RUNNER_REGISTRATION_HEAD)
        .bind(enrollment.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(invalid)?;
    let enrollment = load_enrollment_in(transaction.as_mut(), enrollment)
        .await?
        .ok_or_else(invalid)?;
    let registration = load_registration_in(
        transaction.as_mut(),
        enrollment.enrollment(),
        decode_registration_revision(revision)?,
        Some(&enrollment),
        catalog,
    )
    .await?
    .ok_or_else(invalid)?;
    Ok(Some((enrollment, registration)))
}

async fn validate_connection(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: RunnerEnrollmentId,
    epoch: RunnerConnectionEpoch,
    correlation: &RunnerLeaseCorrelation,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(RUNNER_ENROLLMENT)
        .bind(enrollment.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(invalid)?;
    let enrolled = load_enrollment_in(transaction.as_mut(), enrollment)
        .await?
        .ok_or_else(invalid)?;
    if enrolled.state() != RunnerEnrollmentState::Active || enrolled.runner() != correlation.runner
    {
        return Err(invalid());
    }
    let connection = load_connection_head_in(transaction.as_mut(), enrollment)
        .await?
        .ok_or_else(invalid)?;
    if connection.epoch() != epoch
        || !matches!(
            connection.state(),
            RunnerConnectionState::Connected | RunnerConnectionState::Suspect
        )
    {
        return Err(invalid());
    }
    let row = sqlx::query("SELECT registration_enrollment_id, offer_connection_epoch FROM runner_lease_generation WHERE lease_id = $1 AND generation = $2")
        .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get())).fetch_optional(&mut **transaction).await?.ok_or_else(invalid)?;
    if row.decode_column::<Uuid>("registration_enrollment_id")? != enrollment.into_uuid()
        || row.decode_column::<Option<Decimal>>("offer_connection_epoch")?
            != Some(Decimal::from(epoch.get()))
    {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> RunnerProtocolStoreError {
    RunnerProtocolStoreError::Domain(RunnerDomainError::CorrelationMismatch)
}

fn tool_error(error: crate::tool_loop::ToolLoopRepositoryError) -> RunnerProtocolStoreError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous: false,
        } => RunnerProtocolStoreError::Database(source),
        crate::tool_loop::ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous: true,
        } => RunnerProtocolStoreError::CommitAmbiguous(source),
        _ => RunnerProtocolCorruption::CrossWiredReference.into(),
    }
}
