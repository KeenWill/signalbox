//! Durable daemon-owned reconciliation of ambiguous model calls.

use std::{error::Error, fmt};

use signalbox_application::{
    ClaimedModelCallReconciliation, ClassifyOperatorFailure, ExhaustedModelCallReconciliation,
    ModelCallReconciliationAttempt, ModelCallReconciliationBatch,
    ModelCallReconciliationFailureKind, ModelCallReconciliationOutcome, OperatorFailureClass,
};
use signalbox_domain::{
    AmbiguousModelCallTurnIdentities, ContextFrontierId, PendingSteeringReclassificationIdentity,
    ProviderReportedTokenUsage, TurnId,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid},
    model_execution::{ModelCallRepositoryError, persist_reconciliation_required},
    session::{SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputRepositoryError, load_scheduling_projection},
};

/// Maximum reconciliation attempts claimed by one watchdog scan.
// numeric-bound: ceiling - bounds reconciliation transactions started per watchdog scan
const CLAIM_WINDOW: i64 = 64;

/// Failure while discovering, claiming, or applying automatic reconciliation.
#[derive(Debug)]
pub enum ModelCallReconciliationRepositoryError {
    /// PostgreSQL failed before or during a commit.
    Database {
        /// Whether a commit acknowledgement was lost.
        commit_ambiguous: bool,
        /// Driver failure.
        source: sqlx::Error,
    },
    /// Session rows could not be reconstructed.
    Session(SessionRepositoryError),
    /// Scheduling rows could not be reconstructed.
    Scheduling(SubmitInputRepositoryError),
    /// The shared model-call terminal transition failed.
    Model(ModelCallRepositoryError),
    /// A durable recovery row was outside the closed application vocabulary.
    Corruption(&'static str),
}

impl ModelCallReconciliationRepositoryError {
    fn database(source: sqlx::Error) -> Self {
        Self::Database {
            commit_ambiguous: false,
            source,
        }
    }

    /// Returns the durable failure class recorded for an attempt.
    pub const fn failure_kind(&self) -> ModelCallReconciliationFailureKind {
        match self {
            Self::Database { .. } => ModelCallReconciliationFailureKind::Infrastructure,
            Self::Session(SessionRepositoryError::Database(_))
            | Self::Scheduling(SubmitInputRepositoryError::Database(_))
            | Self::Scheduling(SubmitInputRepositoryError::CommitAmbiguous(_))
            | Self::Model(ModelCallRepositoryError::Database { .. }) => {
                ModelCallReconciliationFailureKind::Infrastructure
            }
            Self::Session(SessionRepositoryError::Corruption(_))
            | Self::Scheduling(_)
            | Self::Model(_)
            | Self::Corruption(_) => ModelCallReconciliationFailureKind::Integrity,
        }
    }
}

impl fmt::Display for ModelCallReconciliationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(
                    formatter,
                    "automatic model-call reconciliation failed: {source}"
                )
            }
            Self::Session(source) => source.fmt(formatter),
            Self::Scheduling(source) => source.fmt(formatter),
            Self::Model(source) => source.fmt(formatter),
            Self::Corruption(detail) => {
                write!(
                    formatter,
                    "invalid automatic model-call reconciliation {detail}"
                )
            }
        }
    }
}

impl Error for ModelCallReconciliationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Session(source) => Some(source),
            Self::Scheduling(source) => Some(source),
            Self::Model(source) => Some(source),
            Self::Corruption(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for ModelCallReconciliationRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Session(SessionRepositoryError::Database(_))
            | Self::Scheduling(SubmitInputRepositoryError::Database(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Scheduling(SubmitInputRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Session(SessionRepositoryError::Corruption(_)) | Self::Scheduling(_) => {
                OperatorFailureClass::FailClosedCorruption
            }
            Self::Model(source) => source.operator_failure_class(),
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Database { .. } => "model_call_reconciliation_database",
            Self::Session(_) => "model_call_reconciliation_session",
            Self::Scheduling(_) => "model_call_reconciliation_scheduling",
            Self::Model(_) => "model_call_reconciliation_transition",
            Self::Corruption(_) => "model_call_reconciliation_corruption",
        }
    }
}

impl From<sqlx::Error> for ModelCallReconciliationRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        Self::database(source)
    }
}

/// PostgreSQL adapter for durable automatic reconciliation attempts.
#[derive(Clone, Debug)]
pub struct PostgresModelCallReconciliationRepository {
    pool: PgPool,
}

impl PostgresModelCallReconciliationRepository {
    /// Uses the shared daemon pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Discovers exact model-call waits and claims one bounded due window.
    pub async fn claim_due(
        &self,
    ) -> Result<ModelCallReconciliationBatch, ModelCallReconciliationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        discover_recoveries(&mut transaction).await?;
        settle_abandoned_attempts(&mut transaction).await?;
        mark_superseded_recoveries(&mut transaction).await?;
        let exhausted_rows = mark_exhausted_recoveries(&mut transaction).await?;
        let rows = sqlx::query(crate::lock_inventory::AUTOMATIC_MODEL_CALL_RECONCILIATION_CLAIM)
            .bind(CLAIM_WINDOW)
            .fetch_all(&mut *transaction)
            .await?;
        transaction.commit().await.map_err(Self::commit_error)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in rows {
            let attempt: i32 = row.try_get("attempt_count")?;
            let attempt = u32::try_from(attempt)
                .ok()
                .and_then(ModelCallReconciliationAttempt::try_from_u32)
                .ok_or(ModelCallReconciliationRepositoryError::Corruption(
                    "attempt ordinal",
                ))?;
            claimed.push(ClaimedModelCallReconciliation::new(
                session_id_from_uuid(row.try_get("session_id")?),
                turn_id_from_uuid(row.try_get("turn_id")?),
                signalbox_domain::ModelCallId::from_uuid(row.try_get("model_call_id")?),
                attempt,
            ));
        }
        let mut exhausted = Vec::with_capacity(exhausted_rows.len());
        for row in exhausted_rows {
            exhausted.push(ExhaustedModelCallReconciliation::new(
                session_id_from_uuid(row.try_get("session_id")?),
                turn_id_from_uuid(row.try_get("turn_id")?),
                signalbox_domain::ModelCallId::from_uuid(row.try_get("model_call_id")?),
            ));
        }
        Ok(ModelCallReconciliationBatch::new(
            claimed.into_boxed_slice(),
            exhausted.into_boxed_slice(),
        ))
    }

    /// Applies one claimed attempt under the session scheduler lock.
    pub async fn reconcile(
        &self,
        claimed: ClaimedModelCallReconciliation,
    ) -> Result<ModelCallReconciliationOutcome, ModelCallReconciliationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
            .bind(session_id_to_uuid(claimed.session()))
            .fetch_optional(&mut *transaction)
            .await?;
        let exact_wait: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'active'
                   AND active_phase_kind = 'awaiting_model_call_recovery'
                   AND recovery_model_call_id = $3
            )",
        )
        .bind(session_id_to_uuid(claimed.session()))
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(claimed.call().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if !exact_wait {
            finish_superseded(&mut transaction, claimed).await?;
            transaction.commit().await.map_err(Self::commit_error)?;
            return Ok(ModelCallReconciliationOutcome::Superseded);
        }
        let Some(session) = load_session_from_connection(&mut transaction, claimed.session())
            .await
            .map_err(ModelCallReconciliationRepositoryError::Session)?
        else {
            return Err(ModelCallReconciliationRepositoryError::Corruption(
                "session for exact wait",
            ));
        };
        let scheduling = load_scheduling_projection(&mut transaction, session)
            .await
            .map_err(ModelCallReconciliationRepositoryError::Scheduling)?;
        let pending = scheduling
            .active_turn_execution()
            .filter(|turn| turn.turn() == claimed.turn())
            .map(|turn| {
                turn.pending_steering()
                    .iter()
                    .map(|steering| {
                        PendingSteeringReclassificationIdentity::new(
                            steering.accepted_input(),
                            TurnId::from_uuid(uuid::Uuid::now_v7()),
                        )
                    })
                    .collect::<Vec<_>>()
            });
        let pending = pending.ok_or(ModelCallReconciliationRepositoryError::Corruption(
            "active turn for exact wait",
        ))?;
        let identities = AmbiguousModelCallTurnIdentities::new(ContextFrontierId::from_uuid(
            uuid::Uuid::now_v7(),
        ))
        .with_pending_steering_reclassifications(pending);
        let Some(attempt) = std::num::NonZeroU32::new(claimed.attempt().get()) else {
            return Err(ModelCallReconciliationRepositoryError::Corruption(
                "zero attempt ordinal",
            ));
        };
        let reconciliation = match scheduling
            .apply_automatic_model_call_reconciliation(attempt, identities)
        {
            Ok(reconciliation) if reconciliation.call().id() == claimed.call() => reconciliation,
            Ok(_) | Err(_) => {
                return Err(ModelCallReconciliationRepositoryError::Corruption(
                    "aggregate transition for exact wait",
                ));
            }
        };
        persist_reconciliation_required(
            &mut transaction,
            &reconciliation,
            ProviderReportedTokenUsage::unreported(),
        )
        .await
        .map_err(ModelCallReconciliationRepositoryError::Model)?;
        finish_attempt(&mut transaction, claimed, "reconciled", "reconciled").await?;
        transaction.commit().await.map_err(Self::commit_error)?;
        Ok(ModelCallReconciliationOutcome::Reconciled)
    }

    /// Durably classifies an attempt whose authoritative transaction failed.
    pub async fn record_failure(
        &self,
        claimed: ClaimedModelCallReconciliation,
        failure: ModelCallReconciliationFailureKind,
    ) -> Result<(), ModelCallReconciliationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(
            "UPDATE automatic_model_call_reconciliation_attempt
                SET outcome_kind = $3, finished_at = statement_timestamp()
              WHERE turn_id = $1 AND attempt_ordinal = $2
                AND outcome_kind = 'attempting'",
        )
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(i64::from(claimed.attempt().get()))
        .bind(failure.as_str())
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        let recovery_rows = sqlx::query(
            "UPDATE automatic_model_call_reconciliation
                SET state_kind = 'scheduled'
              WHERE turn_id = $1 AND attempt_count = $2
                AND state_kind = 'attempting'",
        )
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(i64::from(claimed.attempt().get()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if rows != 1 || recovery_rows != 1 {
            return Err(ModelCallReconciliationRepositoryError::Corruption(
                "attempt failure cardinality",
            ));
        }
        transaction.commit().await.map_err(Self::commit_error)?;
        Ok(())
    }

    fn commit_error(source: sqlx::Error) -> ModelCallReconciliationRepositoryError {
        ModelCallReconciliationRepositoryError::Database {
            commit_ambiguous: commit_failure_is_ambiguous(&source),
            source,
        }
    }
}

async fn discover_recoveries(
    connection: &mut PgConnection,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(
        "INSERT INTO automatic_model_call_reconciliation
            (turn_id, session_id, model_call_id)
         SELECT turn_id, session_id, recovery_model_call_id
           FROM turn_lifecycle
          WHERE state_kind = 'active'
            AND active_phase_kind = 'awaiting_model_call_recovery'
            AND recovery_model_call_id IS NOT NULL
         ON CONFLICT (turn_id) DO NOTHING",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn settle_abandoned_attempts(
    connection: &mut PgConnection,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(
        "UPDATE automatic_model_call_reconciliation_attempt AS attempt
            SET outcome_kind = 'infrastructure_failure',
                finished_at = statement_timestamp()
           FROM automatic_model_call_reconciliation AS recovery
          WHERE recovery.turn_id = attempt.turn_id
            AND recovery.state_kind = 'attempting'
            AND recovery.next_attempt_at <= statement_timestamp()
            AND attempt.attempt_ordinal = recovery.attempt_count
            AND attempt.outcome_kind = 'attempting'",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE automatic_model_call_reconciliation
            SET state_kind = 'scheduled'
          WHERE state_kind = 'attempting'
            AND attempt_count < 5
            AND next_attempt_at <= statement_timestamp()",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn mark_superseded_recoveries(
    connection: &mut PgConnection,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(
        "UPDATE automatic_model_call_reconciliation AS recovery
            SET state_kind = 'superseded', exhausted_at = NULL
          WHERE recovery.state_kind IN ('scheduled', 'attempting', 'exhausted')
            AND NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.turn_id = recovery.turn_id
                   AND lifecycle.session_id = recovery.session_id
                   AND lifecycle.state_kind = 'active'
                   AND lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                   AND lifecycle.recovery_model_call_id = recovery.model_call_id
            )",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn mark_exhausted_recoveries(
    connection: &mut PgConnection,
) -> Result<Vec<sqlx::postgres::PgRow>, ModelCallReconciliationRepositoryError> {
    let rows = sqlx::query(
        "UPDATE automatic_model_call_reconciliation
            SET state_kind = 'exhausted', exhausted_at = statement_timestamp()
          WHERE state_kind IN ('scheduled', 'attempting')
            AND attempt_count = 5
            AND (
                state_kind = 'scheduled'
                OR next_attempt_at <= statement_timestamp()
            )
      RETURNING session_id, turn_id, model_call_id",
    )
    .fetch_all(connection)
    .await?;
    Ok(rows)
}

async fn finish_superseded(
    connection: &mut PgConnection,
    claimed: ClaimedModelCallReconciliation,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    finish_attempt(connection, claimed, "superseded", "superseded").await
}

async fn finish_attempt(
    connection: &mut PgConnection,
    claimed: ClaimedModelCallReconciliation,
    attempt_outcome: &str,
    recovery_state: &str,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    let attempt_rows = sqlx::query(
        "UPDATE automatic_model_call_reconciliation_attempt
            SET outcome_kind = $3, finished_at = statement_timestamp()
          WHERE turn_id = $1 AND attempt_ordinal = $2
            AND outcome_kind = 'attempting'",
    )
    .bind(turn_id_to_uuid(claimed.turn()))
    .bind(i64::from(claimed.attempt().get()))
    .bind(attempt_outcome)
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let recovery_rows = sqlx::query(
        "UPDATE automatic_model_call_reconciliation
            SET state_kind = $3
          WHERE turn_id = $1 AND attempt_count = $2
            AND state_kind = 'attempting'",
    )
    .bind(turn_id_to_uuid(claimed.turn()))
    .bind(i64::from(claimed.attempt().get()))
    .bind(recovery_state)
    .execute(connection)
    .await?
    .rows_affected();
    if attempt_rows != 1 || recovery_rows != 1 {
        return Err(ModelCallReconciliationRepositoryError::Corruption(
            "completion cardinality",
        ));
    }
    Ok(())
}
