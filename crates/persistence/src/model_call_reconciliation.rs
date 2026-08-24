//! Durable daemon-owned reconciliation of ambiguous model calls.

use std::{error::Error, fmt};

use signalbox_application::{
    ClaimedModelCallReconciliation, ClassifyOperatorFailure, ExhaustedModelCallReconciliation,
    ModelCallReconciliationAttempt, ModelCallReconciliationBatch,
    ModelCallReconciliationFailureKind, ModelCallReconciliationOutcome, OperatorFailureClass,
};
use signalbox_domain::{
    AmbiguousModelCallTurnIdentities, ContextFrontierId, PendingSteeringReclassificationIdentity,
    TurnId,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid},
    model_execution::{
        ModelCallRepositoryError, load_delegated_model_call_recovery,
        lock_delegated_child_endpoint_sessions, persist_automatic_reconciliation,
    },
    session::{SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputRepositoryError, load_scheduling_projection},
};

/// Maximum reconciliation attempts claimed by one watchdog scan.
// numeric-bound: ceiling - bounds reconciliation transactions started per watchdog scan
const CLAIM_WINDOW: i64 = 64;

/// How long one claim scan waits for a row lock before giving the turn up.
///
/// The scan's caller also bounds this transaction with a client-side timeout,
/// but dropping a future does not cancel the statement the server is running:
/// the backend keeps waiting for the lock and the pooled connection stays
/// checked out for the full real wait, so the client bound bounds only the
/// daemon's patience. `lock_timeout` bounds the database work itself and raises
/// `55P03`, which this repository already treats as the clean, classifiable
/// "row was busy" signal. It is set before anything is read or written, so the
/// scan either holds its singleton rows or has touched nothing.
// numeric-bound: ceiling - bounds one claim scan's wait for a contended row
const CLAIM_LOCK_WAIT: &str = "1s";

/// How long one claimed attempt waits for a row lock before giving the turn up.
///
/// The attempt transaction takes the delegated child endpoint locks and then
/// the inventoried strongest-mode lock on the session scheduler row, and takes
/// them unqualified — it neither skips a locked row nor refuses to wait. Its
/// caller bounds the attempt with a client-side timeout too, but that bounds
/// only the daemon, for the reason recorded on the claim scan's budget above:
/// the dropped future queues a `ROLLBACK` instead of cancelling, so the backend
/// keeps waiting and every retry strands another pooled connection. Under live
/// traffic — new exposure, since this transaction used to contend with nothing
/// — that turns contention into connection exhaustion.
///
/// `lock_timeout` bounds the database work itself and raises `55P03`, which
/// this repository records as an ordinary infrastructure failure and spends
/// against the attempt budget. It is set before the first lock is taken, so it
/// can only interrupt a lock wait, never a commit. It matches the claim scan's
/// budget because it waits on the same rows for the same reason.
// numeric-bound: ceiling - bounds one claimed attempt's wait for a contended row
const ATTEMPT_LOCK_WAIT: &str = "1s";

/// The claim statement carries one `CASE` arm per admitted attempt, so its
/// arity is part of the contract with the domain budget: admitting another
/// attempt requires another arm and another bound parameter.
const _: () = assert!(ModelCallReconciliationAttempt::budget() == 5);

/// Returns the product attempt budget as the claim statement's parameter.
fn attempt_budget() -> i32 {
    i32::try_from(ModelCallReconciliationAttempt::budget()).unwrap_or(i32::MAX)
}

/// Returns the retry delay after `ordinal` fails, in whole seconds.
///
/// This is the only place the enforced retry schedule is chosen. It reads the
/// domain ladder so that the schedule the daemon runs and the schedule the
/// specification states cannot drift apart unnoticed.
fn retry_backoff_seconds(ordinal: u32) -> Result<i64, ModelCallReconciliationRepositoryError> {
    let attempt = ModelCallReconciliationAttempt::try_from_u32(ordinal).ok_or(
        ModelCallReconciliationRepositoryError::Corruption("attempt ordinal"),
    )?;
    i64::try_from(attempt.retry_backoff().as_secs())
        .map_err(|_| ModelCallReconciliationRepositoryError::Corruption("retry backoff"))
}

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
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(CLAIM_LOCK_WAIT)
            .execute(&mut *transaction)
            .await?;
        discover_recoveries(&mut transaction, CLAIM_WINDOW).await?;
        settle_abandoned_attempts(&mut transaction, CLAIM_WINDOW).await?;
        mark_superseded_recoveries(&mut transaction, CLAIM_WINDOW).await?;
        let exhausted_rows = mark_exhausted_recoveries(&mut transaction, CLAIM_WINDOW).await?;
        let mut claim =
            sqlx::query(crate::lock_inventory::AUTOMATIC_MODEL_CALL_RECONCILIATION_CLAIM)
                .bind(CLAIM_WINDOW)
                .bind(attempt_budget());
        for ordinal in 1..=ModelCallReconciliationAttempt::budget() {
            claim = claim.bind(retry_backoff_seconds(ordinal)?);
        }
        let rows = claim.fetch_all(&mut *transaction).await?;
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
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(ATTEMPT_LOCK_WAIT)
            .execute(&mut *transaction)
            .await?;
        lock_delegated_child_endpoint_sessions(&mut transaction, claimed.session())
            .await
            .map_err(ModelCallReconciliationRepositoryError::Model)?;
        sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
            .bind(session_id_to_uuid(claimed.session()))
            .fetch_optional(&mut *transaction)
            .await?;
        let origin: Option<String> = sqlx::query_scalar(
            "SELECT origin_kind
               FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'active'
                AND active_phase_kind = 'awaiting_model_call_recovery'
                AND recovery_model_call_id = $3",
        )
        .bind(session_id_to_uuid(claimed.session()))
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(claimed.call().into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(origin) = origin else {
            finish_superseded(&mut transaction, claimed).await?;
            transaction.commit().await.map_err(Self::commit_error)?;
            return Ok(ModelCallReconciliationOutcome::Superseded);
        };
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
        let Some(attempt) = std::num::NonZeroU32::new(claimed.attempt().get()) else {
            return Err(ModelCallReconciliationRepositoryError::Corruption(
                "zero attempt ordinal",
            ));
        };
        let reconciliation = match origin.as_str() {
            "accepted_input" => {
                let active = scheduling
                    .active_turn_execution()
                    .filter(|turn| turn.turn() == claimed.turn())
                    .ok_or(ModelCallReconciliationRepositoryError::Corruption(
                        "accepted-input active turn for exact wait",
                    ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
                )
                .with_pending_steering_reclassifications(
                    active
                        .pending_steering()
                        .iter()
                        .map(|steering| {
                            PendingSteeringReclassificationIdentity::new(
                                steering.accepted_input(),
                                TurnId::from_uuid(uuid::Uuid::now_v7()),
                            )
                        })
                        .collect(),
                );
                scheduling.apply_automatic_model_call_reconciliation(attempt, identities)
            }
            "delegation" => {
                let recovery = load_delegated_model_call_recovery(
                    &mut transaction,
                    claimed.session(),
                    &scheduling,
                )
                .await
                .map_err(ModelCallReconciliationRepositoryError::Model)?
                .ok_or(ModelCallReconciliationRepositoryError::Corruption(
                    "delegated active turn for exact wait",
                ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
                )
                .with_pending_steering_reclassifications(
                    recovery
                        .active
                        .pending_steering()
                        .iter()
                        .map(|steering| {
                            PendingSteeringReclassificationIdentity::new(
                                steering.accepted_input(),
                                TurnId::from_uuid(uuid::Uuid::now_v7()),
                            )
                        })
                        .collect(),
                );
                recovery.active.apply_automatic_model_call_reconciliation(
                    recovery.call,
                    recovery.attempt,
                    recovery.source_snapshot,
                    attempt,
                    identities,
                )
            }
            _ => {
                return Err(ModelCallReconciliationRepositoryError::Corruption(
                    "origin for exact wait",
                ));
            }
        };
        let reconciliation = match reconciliation {
            Ok(reconciliation) if reconciliation.call().id() == claimed.call() => reconciliation,
            // The aggregate produced a transition, but for a different call than
            // the one this attempt claimed. Reported apart from a refused
            // transition because the two are different defects: this one means
            // the durable wait and the claim disagree about which call is
            // ambiguous, and it is the fail-closed path an operator reads a
            // park from, so the two must not arrive under one cause.
            Ok(_) => {
                return Err(ModelCallReconciliationRepositoryError::Corruption(
                    "reconciled call identity for exact wait",
                ));
            }
            Err(_) => {
                return Err(ModelCallReconciliationRepositoryError::Corruption(
                    "aggregate transition for exact wait",
                ));
            }
        };
        persist_automatic_reconciliation(&mut transaction, &reconciliation)
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
    window: i64,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(crate::lock_inventory::AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY)
        .bind(window)
        .execute(connection)
        .await?;
    Ok(())
}

async fn settle_abandoned_attempts(
    connection: &mut PgConnection,
    window: i64,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(
        "WITH abandoned AS MATERIALIZED (
            SELECT turn_id, attempt_count
              FROM automatic_model_call_reconciliation
             WHERE state_kind = 'attempting'
               AND next_attempt_at <= statement_timestamp()
             ORDER BY next_attempt_at, turn_id
             LIMIT $1
         ), attempts AS (
            UPDATE automatic_model_call_reconciliation_attempt AS attempt
               SET outcome_kind = 'infrastructure_failure',
                   finished_at = statement_timestamp()
              FROM abandoned
             WHERE attempt.turn_id = abandoned.turn_id
               AND attempt.attempt_ordinal = abandoned.attempt_count
               AND attempt.outcome_kind = 'attempting'
         )
         UPDATE automatic_model_call_reconciliation AS recovery
            SET state_kind = 'scheduled'
           FROM abandoned
          WHERE recovery.turn_id = abandoned.turn_id
            AND recovery.state_kind = 'attempting'
            AND recovery.attempt_count = abandoned.attempt_count",
    )
    .bind(window)
    .execute(connection)
    .await?;
    Ok(())
}

async fn mark_superseded_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<(), ModelCallReconciliationRepositoryError> {
    sqlx::query(crate::lock_inventory::AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION)
        .bind(window)
        .execute(connection)
        .await?;
    Ok(())
}

/// Parks the recoveries that spent the whole attempt budget without settling.
///
/// This is the one statement in the claim scan that raises an operator-visible
/// alert, and an exhaustion park cannot be retracted, so it carries the same two
/// guards its siblings do:
///
/// * A window, like every other statement in `claim_due`. The returned rows
///   become one `warn!` each in the daemon, so an unbounded result set would let
///   one scan emit an unbounded alert burst. Rows over the window are not lost:
///   the ones this scan retires leave the predicate, so the next scan reaches
///   the rest. No cursor is needed for the same reason — unlike supersession,
///   every row this statement selects is also written.
/// * A `turn_lifecycle` correlation, exactly the one supersession uses one
///   statement earlier. Without it a recovery whose turn already reached a
///   terminal state — the turn ended while the recovery row was still pending —
///   would park an operator against a wait that no longer exists. Such a row is
///   left for supersession, which is its correct disposition.
///
/// Only `scheduled` rows are exhausted. An `attempting` row at the budget is a
/// daemon that was lost mid-attempt; `settle_abandoned_attempts` normalizes it
/// to `scheduled` first, which is what closes its `attempting` attempt-history
/// row. Exhausting it here instead would strand that row `attempting` with
/// `finished_at IS NULL` forever, because settlement never revisits an exhausted
/// recovery — destroying the evidence trail for precisely the parks an operator
/// is being alerted about.
async fn mark_exhausted_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, ModelCallReconciliationRepositoryError> {
    let rows = sqlx::query(
        "WITH page AS (
            SELECT recovery.turn_id
              FROM automatic_model_call_reconciliation AS recovery
             WHERE recovery.state_kind = 'scheduled'
               AND recovery.attempt_count = $2
               AND EXISTS (
                    SELECT 1 FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = recovery.turn_id
                       AND lifecycle.session_id = recovery.session_id
                       AND lifecycle.state_kind = 'active'
                       AND lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                       AND NOT lifecycle.delegation_runtime_terminal
                       AND lifecycle.recovery_model_call_id = recovery.model_call_id
               )
             LIMIT $1
         )
         UPDATE automatic_model_call_reconciliation AS recovery
            SET state_kind = 'exhausted', exhausted_at = statement_timestamp()
           FROM page
          WHERE recovery.turn_id = page.turn_id
      RETURNING recovery.session_id, recovery.turn_id, recovery.model_call_id",
    )
    .bind(window)
    .bind(attempt_budget())
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

#[cfg(test)]
mod tests {
    use crate::lock_inventory::{
        AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY,
        AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION,
    };

    #[test]
    fn discovery_is_a_bounded_keyset_page() {
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY
                .contains("turn_id > bounds.after_turn_id")
        );
        assert!(
            !AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY
                .contains("origin_kind = 'accepted_input'")
        );
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY.contains("ORDER BY turn_id"));
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY.contains("LIMIT $1"));
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY.contains("SET after_turn_id"));
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY.contains("ORDER BY turn_id DESC"));
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY
                .contains("turn_id <= bounds.high_turn_id")
        );
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY.contains("high_turn_id = CASE"));
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_DISCOVERY
                .contains("NOT delegation_runtime_terminal")
        );
    }

    #[test]
    fn supersession_is_an_independent_bounded_keyset_page() {
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("recovery.turn_id > bounds.after_turn_id")
        );
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION.contains("ORDER BY recovery.turn_id")
        );
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION.contains("LIMIT $1"));
        assert!(
            !AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("origin_kind = 'accepted_input'")
        );
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION.contains("SET after_turn_id"));
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("NOT lifecycle.delegation_runtime_terminal")
        );
    }

    /// Each supersession lap is bounded, so late arrivals cannot starve rereads.
    ///
    /// Without the high-water mark the cursor wraps only on an empty page, and
    /// a steady window of higher-id recoveries keeps every page full: rows left
    /// behind the cursor that become superseded afterwards would never be
    /// reinspected. The lap bound and the wrap that ends it are therefore part
    /// of this statement's contract, not an optimization.
    #[test]
    fn supersession_bounds_each_cursor_lap() {
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("recovery.turn_id <= bounds.high_turn_id")
        );
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("ORDER BY recovery.turn_id DESC")
        );
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION.contains("SET after_turn_id = CASE")
        );
        assert!(AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION.contains("high_turn_id = CASE"));
        assert!(
            AUTOMATIC_MODEL_CALL_RECONCILIATION_SUPERSESSION
                .contains("(SELECT count(*) FROM page) = $1")
        );
    }
}
