//! Immutable refusal evidence committed with its offered lease and physical attempt.

use super::dispatch::{invalid, tool_error, validate_connection};
use super::*;
use signalbox_domain::{ToolAttemptObservation, ToolExecutionError, ToolExecutionErrorKind};

/// Closed reasons for refusing an offered lease before execution authority is issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerLeaseFailureKind {
    CredentialUnavailable,
    RepositoryUnavailable,
    SandboxUnavailable,
    WorkspaceConflict,
    LeaseAdmissionRefused,
}

impl RunnerLeaseFailureKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::CredentialUnavailable => "credential_unavailable",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::WorkspaceConflict => "workspace_conflict",
            Self::LeaseAdmissionRefused => "lease_admission_refused",
        }
    }
    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "credential_unavailable" => Self::CredentialUnavailable,
            "repository_unavailable" => Self::RepositoryUnavailable,
            "sandbox_unavailable" => Self::SandboxUnavailable,
            "workspace_conflict" => Self::WorkspaceConflict,
            "lease_admission_refused" => Self::LeaseAdmissionRefused,
            _ => return None,
        })
    }
}

impl RunnerProtocolStore {
    /// Retains exact refusal detail and resolves the offer and attempt before acknowledgement.
    pub async fn record_tool_lease_failure(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        correlation: RunnerLeaseCorrelation,
        category: RunnerLeaseFailureKind,
        detail: &serde_json::Value,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.dispatch.session())
            .await
            .map_err(tool_error)?;
        validate_connection(&mut transaction, enrollment, epoch, &correlation).await?;
        let refused = self
            .record_tool_lease_failure_in(&mut transaction, correlation, category, detail)
            .await?;
        commit_mutation(transaction).await?;
        Ok(refused)
    }

    pub(super) async fn record_tool_lease_failure_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        correlation: RunnerLeaseCorrelation,
        category: RunnerLeaseFailureKind,
        detail: &serde_json::Value,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        sqlx::query(RUNNER_LEASE_HEAD)
            .bind(correlation.lease.into_uuid())
            .bind(Decimal::from(correlation.generation.get()))
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or_else(invalid)?;
        let lease = self
            .load_lease_in(transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or_else(invalid)?;
        if lease.correlation() != correlation {
            return Err(invalid());
        }
        if lease.state() == RunnerLeaseState::Refused {
            let row = sqlx::query("SELECT category, detail FROM runner_lease_failure WHERE lease_id = $1 AND generation = $2")
                .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get()))
                .fetch_optional(&mut **transaction).await?.ok_or_else(invalid)?;
            if row.decode_column::<String>("category")? != category.as_str()
                || row.decode_column::<serde_json::Value>("detail")? != *detail
            {
                return Err(invalid());
            }
            return Ok(lease);
        }
        let refused = lease
            .refuse(correlation.clone())
            .map_err(RunnerProtocolStoreError::Domain)?;
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
        let observed = authorized
            .executor_fence()
            .bind(ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            });
        let (current, _) = authorized.into_parts();
        let ended = current
            .apply_terminal_observation(observed)
            .map_err(|_| invalid())?;
        crate::tool_loop::persist_ended_attempt(transaction.as_mut(), &ended)
            .await
            .map_err(tool_error)?;
        sqlx::query("INSERT INTO runner_lease_failure (lease_id, generation, category, detail) VALUES ($1,$2,$3,$4)")
            .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get()))
            .bind(category.as_str()).bind(detail).execute(&mut **transaction).await?;
        append_lease_event_in(transaction, &refused).await?;
        Ok(refused)
    }
}
