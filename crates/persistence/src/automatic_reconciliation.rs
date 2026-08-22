//! Durable daemon-owned reconciliation of ambiguous physical operations.

use std::{error::Error, fmt, time::Duration};

use signalbox_application::{
    AutomaticReconciliationAttempt, AutomaticReconciliationBatch,
    AutomaticReconciliationFailureKind, AutomaticReconciliationOperation,
    AutomaticReconciliationOutcome, ClaimedAutomaticReconciliation, ClassifyOperatorFailure,
    ExhaustedAutomaticReconciliation, OperatorFailureClass,
};
use signalbox_domain::{
    AmbiguousModelCallTurnIdentities, ContextFrontierId, PendingSteeringReclassificationIdentity,
    ProviderReportedTokenUsage, ReconstitutedToolAttempt, SemanticTranscriptEntryId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        session_id_from_uuid, session_id_to_uuid, tool_attempt_id_from_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    },
    model_execution::{
        ModelCallRepositoryError, persist_reconciliation_required,
        persist_tool_reconciliation_required,
    },
    session::{SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputRepositoryError, load_scheduling_projection},
    tool_loop::{ToolLoopRepositoryError, load_recovery_batch_by_attempt},
};

/// Reconciliation attempts claimed before the next transaction starts.
///
/// Claiming exactly one keeps a later operation from spending its durable
/// attempt deadline while earlier session-locked transactions run. The daemon
/// still drains a bounded multi-operation scan by returning here after each
/// completed attempt and claiming the next one just in time.
// numeric-bound: ceiling - prevents durable attempt deadlines expiring before work starts
const CLAIM_WINDOW: i64 = 1;

fn decode_operation(
    model_call: Option<uuid::Uuid>,
    tool_attempt: Option<uuid::Uuid>,
) -> Result<AutomaticReconciliationOperation, AutomaticReconciliationRepositoryError> {
    match (model_call, tool_attempt) {
        (Some(call), None) => Ok(AutomaticReconciliationOperation::ModelCall(
            signalbox_domain::ModelCallId::from_uuid(call),
        )),
        (None, Some(attempt)) => Ok(AutomaticReconciliationOperation::ToolAttempt(
            tool_attempt_id_from_uuid(attempt),
        )),
        (Some(_), Some(_)) | (None, None) => Err(
            AutomaticReconciliationRepositoryError::Corruption("operation identity"),
        ),
    }
}

/// Failure while discovering, claiming, or applying automatic reconciliation.
#[derive(Debug)]
pub enum AutomaticReconciliationRepositoryError {
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
    /// Tool-round evidence could not reconstruct the exact recovery wait.
    Tool(ToolLoopRepositoryError),
    /// A durable recovery row was outside the closed application vocabulary.
    Corruption(&'static str),
}

impl AutomaticReconciliationRepositoryError {
    fn database(source: sqlx::Error) -> Self {
        Self::Database {
            commit_ambiguous: false,
            source,
        }
    }

    /// Returns the durable failure class recorded for an attempt.
    pub const fn failure_kind(&self) -> AutomaticReconciliationFailureKind {
        match self {
            Self::Database { .. } => AutomaticReconciliationFailureKind::Infrastructure,
            Self::Session(SessionRepositoryError::Database(_))
            | Self::Scheduling(SubmitInputRepositoryError::Database(_))
            | Self::Scheduling(SubmitInputRepositoryError::CommitAmbiguous(_))
            | Self::Model(ModelCallRepositoryError::Database { .. })
            | Self::Tool(ToolLoopRepositoryError::Database { .. }) => {
                AutomaticReconciliationFailureKind::Infrastructure
            }
            Self::Session(SessionRepositoryError::Corruption(_))
            | Self::Scheduling(_)
            | Self::Model(_)
            | Self::Tool(_)
            | Self::Corruption(_) => AutomaticReconciliationFailureKind::Integrity,
        }
    }
}

impl fmt::Display for AutomaticReconciliationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(
                    formatter,
                    "automatic operation reconciliation failed: {source}"
                )
            }
            Self::Session(source) => source.fmt(formatter),
            Self::Scheduling(source) => source.fmt(formatter),
            Self::Model(source) => source.fmt(formatter),
            Self::Tool(source) => source.fmt(formatter),
            Self::Corruption(detail) => {
                write!(
                    formatter,
                    "invalid automatic operation reconciliation {detail}"
                )
            }
        }
    }
}

impl Error for AutomaticReconciliationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Session(source) => Some(source),
            Self::Scheduling(source) => Some(source),
            Self::Model(source) => Some(source),
            Self::Tool(source) => Some(source),
            Self::Corruption(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for AutomaticReconciliationRepositoryError {
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
            Self::Tool(source) => source.operator_failure_class(),
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Database { .. } => "automatic_reconciliation_database",
            Self::Session(_) => "automatic_reconciliation_session",
            Self::Scheduling(_) => "automatic_reconciliation_scheduling",
            Self::Model(_) => "automatic_reconciliation_transition",
            Self::Tool(_) => "automatic_reconciliation_tool_evidence",
            Self::Corruption(_) => "automatic_reconciliation_corruption",
        }
    }
}

impl From<sqlx::Error> for AutomaticReconciliationRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        Self::database(source)
    }
}

/// PostgreSQL adapter for durable automatic reconciliation attempts.
#[derive(Clone, Debug)]
pub struct PostgresAutomaticReconciliationRepository {
    pool: PgPool,
}

impl PostgresAutomaticReconciliationRepository {
    /// Uses the shared daemon pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Discovers exact ambiguity waits and claims one bounded due window.
    pub async fn claim_due(
        &self,
        transaction_bound: Duration,
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        let mut transaction = self.begin_bounded(transaction_bound).await?;
        discover_recoveries(&mut transaction).await?;
        settle_abandoned_attempts(&mut transaction).await?;
        mark_superseded_recoveries(&mut transaction).await?;
        let exhausted_rows = mark_exhausted_recoveries(&mut transaction).await?;
        let rows = sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLAIM)
            .bind(CLAIM_WINDOW)
            .fetch_all(&mut *transaction)
            .await?;
        transaction.commit().await.map_err(Self::commit_error)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in rows {
            let attempt: i32 = row.try_get("attempt_count")?;
            let attempt = u32::try_from(attempt)
                .ok()
                .and_then(AutomaticReconciliationAttempt::try_from_u32)
                .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                    "attempt ordinal",
                ))?;
            let model_call: Option<uuid::Uuid> = row.try_get("model_call_id")?;
            let tool_attempt: Option<uuid::Uuid> = row.try_get("tool_attempt_id")?;
            let operation = decode_operation(model_call, tool_attempt)?;
            claimed.push(ClaimedAutomaticReconciliation::new(
                session_id_from_uuid(row.try_get("session_id")?),
                turn_id_from_uuid(row.try_get("turn_id")?),
                operation,
                attempt,
            ));
        }
        let mut exhausted = Vec::with_capacity(exhausted_rows.len());
        for row in exhausted_rows {
            let operation = decode_operation(
                row.try_get("model_call_id")?,
                row.try_get("tool_attempt_id")?,
            )?;
            exhausted.push(ExhaustedAutomaticReconciliation::new(
                session_id_from_uuid(row.try_get("session_id")?),
                turn_id_from_uuid(row.try_get("turn_id")?),
                operation,
            ));
        }
        Ok(AutomaticReconciliationBatch::new(
            claimed.into_boxed_slice(),
            exhausted.into_boxed_slice(),
        ))
    }

    /// Applies one claimed attempt under the session scheduler lock.
    pub async fn reconcile(
        &self,
        claimed: ClaimedAutomaticReconciliation,
        transaction_bound: Duration,
    ) -> Result<AutomaticReconciliationOutcome, AutomaticReconciliationRepositoryError> {
        let mut transaction = self.begin_bounded(transaction_bound).await?;
        sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
            .bind(session_id_to_uuid(claimed.session()))
            .fetch_optional(&mut *transaction)
            .await?;
        let (model_call, tool_attempt, phase) = match claimed.operation() {
            AutomaticReconciliationOperation::ModelCall(call) => {
                (Some(call.into_uuid()), None, "awaiting_model_call_recovery")
            }
            AutomaticReconciliationOperation::ToolAttempt(attempt) => {
                (None, Some(attempt.into_uuid()), "awaiting_tool_recovery")
            }
        };
        let exact_wait: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'active'
                   AND active_phase_kind = $3
                   AND recovery_model_call_id IS NOT DISTINCT FROM $4
                   AND recovery_tool_attempt_id IS NOT DISTINCT FROM $5
            )",
        )
        .bind(session_id_to_uuid(claimed.session()))
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(phase)
        .bind(model_call)
        .bind(tool_attempt)
        .fetch_one(&mut *transaction)
        .await?;
        if !exact_wait {
            finish_superseded(&mut transaction, claimed).await?;
            transaction.commit().await.map_err(Self::commit_error)?;
            return Ok(AutomaticReconciliationOutcome::Superseded);
        }
        let Some(session) = load_session_from_connection(&mut transaction, claimed.session())
            .await
            .map_err(AutomaticReconciliationRepositoryError::Session)?
        else {
            return Err(AutomaticReconciliationRepositoryError::Corruption(
                "session for exact wait",
            ));
        };
        let scheduling = load_scheduling_projection(&mut transaction, session)
            .await
            .map_err(AutomaticReconciliationRepositoryError::Scheduling)?;
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
        let pending = pending.ok_or(AutomaticReconciliationRepositoryError::Corruption(
            "active turn for exact wait",
        ))?;
        let terminal_frontier = ContextFrontierId::from_uuid(uuid::Uuid::now_v7());
        let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
            .with_pending_steering_reclassifications(pending);
        let Some(attempt) = std::num::NonZeroU32::new(claimed.attempt().get()) else {
            return Err(AutomaticReconciliationRepositoryError::Corruption(
                "zero attempt ordinal",
            ));
        };
        match claimed.operation() {
            AutomaticReconciliationOperation::ModelCall(claimed_call) => {
                let reconciliation =
                    match scheduling.apply_automatic_reconciliation(attempt, identities) {
                        Ok(reconciliation) if reconciliation.call().id() == claimed_call => {
                            reconciliation
                        }
                        Ok(_) | Err(_) => {
                            return Err(AutomaticReconciliationRepositoryError::Corruption(
                                "model-call aggregate transition for exact wait",
                            ));
                        }
                    };
                persist_reconciliation_required(
                    &mut transaction,
                    &reconciliation,
                    ProviderReportedTokenUsage::unreported(),
                )
                .await
                .map_err(AutomaticReconciliationRepositoryError::Model)?;
            }
            AutomaticReconciliationOperation::ToolAttempt(claimed_attempt) => {
                let batch = load_recovery_batch_by_attempt(
                    &mut transaction,
                    claimed.session(),
                    claimed.turn(),
                    claimed_attempt,
                )
                .await
                .map_err(AutomaticReconciliationRepositoryError::Tool)?;
                let wait = batch.awaiting_recovery().ok_or(
                    AutomaticReconciliationRepositoryError::Corruption(
                        "tool recovery wait evidence",
                    ),
                )?;
                let ended = batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(ended))
                            if ended.attempt() == claimed_attempt =>
                        {
                            Some(ended.clone())
                        }
                        Some(ReconstitutedToolAttempt::Current(_))
                        | Some(ReconstitutedToolAttempt::Ended(_))
                        | None => None,
                    })
                    .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                        "ambiguous tool attempt evidence",
                    ))?;
                let entry_ids = batch
                    .requests()
                    .iter()
                    .map(|_| SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()))
                    .collect();
                let projection = batch
                    .prepare_reconciliation_projection(entry_ids, terminal_frontier)
                    .map_err(|_| {
                        AutomaticReconciliationRepositoryError::Corruption(
                            "tool recovery result projection",
                        )
                    })?;
                let reconciliation = scheduling
                    .apply_automatic_tool_reconciliation(
                        wait, ended, projection, attempt, identities,
                    )
                    .map_err(|_| {
                        AutomaticReconciliationRepositoryError::Corruption(
                            "tool aggregate transition for exact wait",
                        )
                    })?;
                persist_tool_reconciliation_required(&mut transaction, &reconciliation)
                    .await
                    .map_err(AutomaticReconciliationRepositoryError::Model)?;
            }
        }
        finish_attempt(&mut transaction, claimed, "reconciled", "reconciled").await?;
        transaction.commit().await.map_err(Self::commit_error)?;
        Ok(AutomaticReconciliationOutcome::Reconciled)
    }

    /// Durably classifies an attempt whose authoritative transaction failed.
    pub async fn record_failure(
        &self,
        claimed: ClaimedAutomaticReconciliation,
        failure: AutomaticReconciliationFailureKind,
        transaction_bound: Duration,
    ) -> Result<(), AutomaticReconciliationRepositoryError> {
        let mut transaction = self.begin_bounded(transaction_bound).await?;
        let rows = sqlx::query(
            "UPDATE automatic_reconciliation_attempt
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
            "UPDATE automatic_reconciliation
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
            return Err(AutomaticReconciliationRepositoryError::Corruption(
                "attempt failure cardinality",
            ));
        }
        transaction.commit().await.map_err(Self::commit_error)?;
        Ok(())
    }

    fn commit_error(source: sqlx::Error) -> AutomaticReconciliationRepositoryError {
        AutomaticReconciliationRepositoryError::Database {
            commit_ambiguous: commit_failure_is_ambiguous(&source),
            source,
        }
    }

    /// Starts a transaction whose server-side lifetime cannot outlive its
    /// daemon-owned recovery attempt.
    ///
    /// A client-side future timeout cannot cancel PostgreSQL work that is
    /// already running. Installing the bound in PostgreSQL keeps an abandoned
    /// client from leaving a transaction queued on the shared outbox allocator
    /// after the daemon has moved on to later recovery work.
    async fn begin_bounded(
        &self,
        transaction_bound: Duration,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AutomaticReconciliationRepositoryError> {
        let timeout_millis = i64::try_from(transaction_bound.as_millis())
            .ok()
            .filter(|millis| *millis > 0)
            .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                "transaction bound",
            ))?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT set_config('transaction_timeout', $1, true)")
            .bind(format!("{timeout_millis}ms"))
            .execute(&mut *transaction)
            .await?;
        Ok(transaction)
    }
}

async fn discover_recoveries(
    connection: &mut PgConnection,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(
        "INSERT INTO automatic_reconciliation
            (turn_id, session_id, model_call_id, tool_attempt_id)
         SELECT turn_id, session_id, recovery_model_call_id,
                recovery_tool_attempt_id
           FROM turn_lifecycle
          WHERE state_kind = 'active'
            AND active_phase_kind IN (
                'awaiting_model_call_recovery', 'awaiting_tool_recovery'
            )
            AND num_nonnulls(recovery_model_call_id, recovery_tool_attempt_id) = 1
         ON CONFLICT (turn_id) DO NOTHING",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn settle_abandoned_attempts(
    connection: &mut PgConnection,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(
        "UPDATE automatic_reconciliation_attempt AS attempt
            SET outcome_kind = 'infrastructure_failure',
                finished_at = statement_timestamp()
           FROM automatic_reconciliation AS recovery
          WHERE recovery.turn_id = attempt.turn_id
            AND recovery.state_kind = 'attempting'
            AND recovery.next_attempt_at <= statement_timestamp()
            AND attempt.attempt_ordinal = recovery.attempt_count
            AND attempt.outcome_kind = 'attempting'",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE automatic_reconciliation
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
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(
        "UPDATE automatic_reconciliation_attempt AS attempt
            SET outcome_kind = 'superseded', finished_at = statement_timestamp()
           FROM automatic_reconciliation AS recovery
          WHERE recovery.turn_id = attempt.turn_id
            AND recovery.state_kind = 'attempting'
            AND attempt.attempt_ordinal = recovery.attempt_count
            AND attempt.outcome_kind = 'attempting'
            AND NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.turn_id = recovery.turn_id
                   AND lifecycle.session_id = recovery.session_id
                   AND lifecycle.state_kind = 'active'
                   AND (
                        lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                        AND lifecycle.recovery_model_call_id = recovery.model_call_id
                        AND recovery.tool_attempt_id IS NULL
                     OR lifecycle.active_phase_kind = 'awaiting_tool_recovery'
                        AND lifecycle.recovery_tool_attempt_id = recovery.tool_attempt_id
                        AND recovery.model_call_id IS NULL
                   )
            )",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE automatic_reconciliation AS recovery
            SET state_kind = 'superseded', exhausted_at = NULL
          WHERE recovery.state_kind IN ('scheduled', 'attempting', 'exhausted')
            AND NOT EXISTS (
                SELECT 1 FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.turn_id = recovery.turn_id
                   AND lifecycle.session_id = recovery.session_id
                   AND lifecycle.state_kind = 'active'
                   AND (
                        lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                        AND lifecycle.recovery_model_call_id = recovery.model_call_id
                        AND recovery.tool_attempt_id IS NULL
                     OR lifecycle.active_phase_kind = 'awaiting_tool_recovery'
                        AND lifecycle.recovery_tool_attempt_id = recovery.tool_attempt_id
                        AND recovery.model_call_id IS NULL
                   )
            )",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn mark_exhausted_recoveries(
    connection: &mut PgConnection,
) -> Result<Vec<sqlx::postgres::PgRow>, AutomaticReconciliationRepositoryError> {
    let rows = sqlx::query(
        "UPDATE automatic_reconciliation
            SET state_kind = 'exhausted', exhausted_at = statement_timestamp()
          WHERE state_kind IN ('scheduled', 'attempting')
            AND attempt_count = 5
            AND (
                state_kind = 'scheduled'
                OR next_attempt_at <= statement_timestamp()
            )
      RETURNING session_id, turn_id, model_call_id, tool_attempt_id",
    )
    .fetch_all(connection)
    .await?;
    Ok(rows)
}

async fn finish_superseded(
    connection: &mut PgConnection,
    claimed: ClaimedAutomaticReconciliation,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    finish_attempt(connection, claimed, "superseded", "superseded").await
}

async fn finish_attempt(
    connection: &mut PgConnection,
    claimed: ClaimedAutomaticReconciliation,
    attempt_outcome: &str,
    recovery_state: &str,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    let attempt_rows = sqlx::query(
        "UPDATE automatic_reconciliation_attempt
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
        "UPDATE automatic_reconciliation
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
        return Err(AutomaticReconciliationRepositoryError::Corruption(
            "completion cardinality",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{ModelCallId, ToolAttemptId};
    use uuid::Uuid;

    use super::{
        AutomaticReconciliationFailureKind, AutomaticReconciliationOperation,
        AutomaticReconciliationRepositoryError, decode_operation,
    };
    use crate::model_execution::ModelCallRepositoryError;

    const MODEL_CALL_UUID: u128 = 0xa001; // numeric-bound: test - model-call identity fixture
    const TOOL_ATTEMPT_UUID: u128 = 0xa002; // numeric-bound: test - tool-attempt identity fixture

    /// The durable row admits exactly one typed operation identity.
    #[test]
    fn operation_identity_decoding_is_closed_and_typed() {
        let model_call_uuid = Uuid::from_u128(MODEL_CALL_UUID);
        let tool_attempt_uuid = Uuid::from_u128(TOOL_ATTEMPT_UUID);

        let model = decode_operation(Some(model_call_uuid), None)
            .expect("one model-call identity is admitted");
        let tool = decode_operation(None, Some(tool_attempt_uuid))
            .expect("one tool-attempt identity is admitted");
        let both = decode_operation(Some(model_call_uuid), Some(tool_attempt_uuid))
            .expect_err("two operation identities are corruption");
        let neither =
            decode_operation(None, None).expect_err("an absent operation identity is corruption");

        assert_eq!(
            model,
            AutomaticReconciliationOperation::ModelCall(ModelCallId::from_uuid(model_call_uuid))
        );
        assert_eq!(
            tool,
            AutomaticReconciliationOperation::ToolAttempt(ToolAttemptId::from_uuid(
                tool_attempt_uuid
            ))
        );
        assert!(matches!(
            both,
            AutomaticReconciliationRepositoryError::Corruption("operation identity")
        ));
        assert!(matches!(
            neither,
            AutomaticReconciliationRepositoryError::Corruption("operation identity")
        ));
    }

    /// The live `automatic_reconciliation_database` failure remains an
    /// ordinary infrastructure failure when no commit acknowledgement was lost.
    #[test]
    fn automatic_reconciliation_database_failure_keeps_its_operator_contract() {
        let error = AutomaticReconciliationRepositoryError::from(sqlx::Error::PoolClosed);

        assert_eq!(
            error.failure_kind(),
            AutomaticReconciliationFailureKind::Infrastructure
        );
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        );
        assert_eq!(
            error.operator_failure_cause_code(),
            "automatic_reconciliation_database"
        );
        assert!(error.source().is_some());
        assert!(
            error
                .to_string()
                .contains("automatic operation reconciliation failed")
        );
    }

    /// The live `automatic_reconciliation_transition` path preserves the
    /// model transition's caller-or-hub classification and spends integrity budget.
    #[test]
    fn automatic_reconciliation_transition_failure_keeps_its_operator_contract() {
        let error = AutomaticReconciliationRepositoryError::Model(
            ModelCallRepositoryError::InvalidTransition("automatic recovery fixture"),
        );

        assert_eq!(
            error.failure_kind(),
            AutomaticReconciliationFailureKind::Integrity
        );
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::CallerOrHubBug
        );
        assert_eq!(
            error.operator_failure_cause_code(),
            "automatic_reconciliation_transition"
        );
        assert!(error.source().is_some());
        assert!(error.to_string().contains("model-call transition rejected"));
    }

    /// A lost commit acknowledgement remains distinct from an ordinary
    /// database failure so the daemon does not double-record the attempt.
    #[test]
    fn automatic_reconciliation_commit_failure_retains_ambiguity() {
        let error = AutomaticReconciliationRepositoryError::Database {
            commit_ambiguous: true,
            source: sqlx::Error::PoolClosed,
        };

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
        assert_eq!(
            error.failure_kind(),
            AutomaticReconciliationFailureKind::Infrastructure
        );
    }
}
