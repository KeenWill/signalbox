//! Authenticated reconnect evidence, resolved before changing registration.

use super::dispatch::{invalid, tool_error};
use super::*;
use signalbox_domain::ToolAttemptObservation;

/// Durable execution evidence supplied by the reconnecting runner.
#[derive(Clone, Debug)]
pub enum RunnerLeaseResumeEvidence {
    /// The runner has not crossed its execution-start boundary.
    AwaitingDispatch(RunnerLeaseCorrelation),
    /// Execution may have started, without a retained terminal result.
    ExecutionPossible(RunnerLeaseCorrelation),
    /// Exact terminal evidence retained until acknowledgement.
    Result {
        /// Full original lease authority.
        correlation: RunnerLeaseCorrelation,
        /// Canonical tool-attempt observation.
        observation: ToolAttemptObservation,
    },
}

impl RunnerLeaseResumeEvidence {
    fn correlation(&self) -> &RunnerLeaseCorrelation {
        match self {
            Self::AwaitingDispatch(correlation)
            | Self::ExecutionPossible(correlation)
            | Self::Result { correlation, .. } => correlation,
        }
    }
}

/// Canonical action for one authenticated reconnect inventory.
#[derive(Clone, Debug)]
pub enum RunnerLeaseResumeOutcome {
    /// No retained or outstanding execution authority exists.
    Empty,
    /// The exact claimed lease remains eligible for dispatch replay.
    AwaitingDispatch,
    /// Terminal evidence is durably recorded.
    Recorded,
    /// The canonical lease has already lost authority.
    Lost,
    /// The current connection must be durably lost before reconnect admission.
    LoseConnection(RunnerConnectionSnapshot),
}

impl RunnerProtocolStore {
    /// Authenticates and reconciles execution evidence before registration changes.
    pub async fn reconcile_tool_resume(
        &self,
        request: RunnerEnrollmentRequestId,
        observed: IssuedRunnerEnrollmentIdentities,
        prior_revision: RunnerRegistrationRevision,
        advertisement: &RunnerAdvertisement,
        evidence: Option<RunnerLeaseResumeEvidence>,
    ) -> Result<RunnerLeaseResumeOutcome, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        if let Some(evidence) = &evidence {
            crate::tool_loop::lock_tool_session(
                transaction.as_mut(),
                evidence.correlation().dispatch.session(),
            )
            .await
            .map_err(tool_error)?;
        }
        let stored = load_enrollment_request_facts(transaction.as_mut(), request)
            .await?
            .ok_or(RunnerEnrollmentRequestFailure::UnknownRequest { request })?;
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(stored.identities.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(invalid)?;
        if stored.identities != observed {
            return Err(RunnerEnrollmentRequestFailure::ResumeIdentityMismatch {
                request,
                expected: stored.identities,
                observed,
            }
            .into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), observed.enrollment())
            .await?
            .ok_or_else(invalid)?;
        if enrollment.state() == RunnerEnrollmentState::Revoked {
            return Err(RunnerEnrollmentRequestFailure::EnrollmentRevoked {
                request,
                enrollment: observed.enrollment(),
            }
            .into());
        }
        let current: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(observed.enrollment().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = decode_registration_revision(current)?;
        let registration = load_registration_in(
            transaction.as_mut(),
            observed.enrollment(),
            current,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or_else(invalid)?;
        let receipt = RunnerEnrollmentReceipt {
            request,
            enrollment,
            registration,
        };
        if prior_revision > current
            || ((prior_revision < current
                || receipt.enrollment().state() == RunnerEnrollmentState::Pending)
                && receipt.advertisement() != *advertisement)
        {
            return Err(invalid());
        }
        let connection =
            load_connection_head_in(transaction.as_mut(), observed.enrollment()).await?;
        let Some(evidence) = evidence else {
            let outstanding: bool = sqlx::query_scalar("SELECT EXISTS (
                SELECT 1 FROM runner_lease_generation AS generation
                JOIN runner_current_lease_event AS head USING (lease_id, generation)
                JOIN runner_lease_event AS event USING (lease_id, generation, event_ordinal)
                WHERE generation.registration_enrollment_id = $1 AND event.state_kind IN ('offered', 'claimed'))")
                .bind(observed.enrollment().into_uuid()).fetch_one(&mut *transaction).await?;
            transaction.commit().await?;
            return if outstanding {
                Ok(RunnerLeaseResumeOutcome::LoseConnection(
                    connection.ok_or_else(invalid)?,
                ))
            } else {
                Ok(RunnerLeaseResumeOutcome::Empty)
            };
        };
        let correlation = evidence.correlation().clone();
        if correlation.runner != observed.runner() {
            return Err(invalid());
        }
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
        let outcome = match lease.state() {
            RunnerLeaseState::LostUnclaimed
            | RunnerLeaseState::LostExecutionPossible
            | RunnerLeaseState::LostClaimed => RunnerLeaseResumeOutcome::Lost,
            RunnerLeaseState::Completed => {
                if let RunnerLeaseResumeEvidence::Result { observation, .. } = evidence {
                    self.record_tool_lease_result_in(&mut transaction, correlation, observation)
                        .await?;
                } else {
                    return Err(invalid());
                }
                RunnerLeaseResumeOutcome::Recorded
            }
            RunnerLeaseState::Claimed => {
                let connection = connection.ok_or_else(invalid)?;
                let intact: bool = sqlx::query_scalar("SELECT generation.offer_loss_epoch IS NOT DISTINCT FROM loss.loss_epoch
                    FROM runner_lease_generation AS generation
                    LEFT JOIN runner_current_connection_loss AS loss ON loss.enrollment_id = generation.registration_enrollment_id
                    WHERE generation.lease_id = $1 AND generation.generation = $2 AND generation.registration_enrollment_id = $3")
                    .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get()))
                    .bind(observed.enrollment().into_uuid()).fetch_optional(&mut *transaction).await?.ok_or_else(invalid)?;
                if !intact
                    || !matches!(
                        connection.state(),
                        RunnerConnectionState::Connected
                            | RunnerConnectionState::Suspect
                            | RunnerConnectionState::Shutdown
                    )
                {
                    RunnerLeaseResumeOutcome::LoseConnection(connection)
                } else {
                    match evidence {
                        RunnerLeaseResumeEvidence::AwaitingDispatch(_) => {
                            RunnerLeaseResumeOutcome::AwaitingDispatch
                        }
                        RunnerLeaseResumeEvidence::ExecutionPossible(_) => {
                            RunnerLeaseResumeOutcome::LoseConnection(connection)
                        }
                        RunnerLeaseResumeEvidence::Result { observation, .. } => {
                            self.record_tool_lease_result_in(
                                &mut transaction,
                                correlation,
                                observation,
                            )
                            .await?;
                            RunnerLeaseResumeOutcome::Recorded
                        }
                    }
                }
            }
            RunnerLeaseState::Offered => return Err(invalid()),
        };
        commit_mutation(transaction).await?;
        Ok(outcome)
    }
}
