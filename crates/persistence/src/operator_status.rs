//! Read-only lifecycle projections for the operator status command.

use crate::session_lifecycle::{SessionSupervisionFailureRecord, decode_supervision_failure};
use signalbox_domain::SessionId;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::lifecycle_metrics::{
    DECLARE_DEADLINE_VIOLATIONS_CURSOR, DECLARE_WEEKLY_METRICS_CURSOR, LifecycleDeadlineViolation,
    LifecycleMetricsError, LifecycleWeeklyMetrics, MAX_REPORTED_WEEKS, decode_violation,
    decode_week,
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// One row in the fixed-phase operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusItem {
    /// One calendar week's §12 metrics.
    LifecycleWeek(LifecycleWeeklyMetrics),
    /// One owned non-terminal session past its §1 deadline obligation.
    LifecycleDeadlineViolation(LifecycleDeadlineViolation),
    /// Pending supervision evidence, including skipped terminal sessions.
    SessionSupervision {
        session: SessionId,
        terminal: bool,
        failure: SessionSupervisionFailureRecord,
    },
}

#[derive(signalbox_derive::Accessors)]
/// Counts committed after every status cursor has been exhausted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessOperatorStatusCounts {
    #[get(copy)]
    lifecycle_weeks: u64,
    /// Returns the `nonterminal_past_deadline` alarm value, target zero.
    #[get(copy)]
    lifecycle_deadline_violations: u64,
    #[get(copy)]
    session_supervision: u64,
    /// Returns the persistent outbox quarantine alarm value, target zero.
    #[get(copy)]
    outbox_quarantines: u64,
}

/// PostgreSQL-backed operator-status read boundary.
#[derive(Clone, Debug)]
pub struct ProcessOperatorStatusRepository {
    pool: PgPool,
}

impl ProcessOperatorStatusRepository {
    /// Uses the supplied pool for independent repeatable-read snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Opens one coherent lifecycle snapshot.
    pub async fn open(&self) -> Result<ProcessOperatorStatusReader, ProcessOperatorStatusError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let outbox_quarantines =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM outbox_event_quarantine")
                .fetch_one(&mut *transaction)
                .await?
                .try_into()
                .map_err(|_| {
                    ProcessOperatorStatusCorruption::InvalidNumber("outbox quarantine count")
                })?;
        declare_status_cursors(&mut transaction).await?;
        Ok(ProcessOperatorStatusReader {
            transaction: Some(transaction),
            phase: ProcessOperatorStatusPhase::LifecycleWeeks,
            counts: ProcessOperatorStatusCounts {
                outbox_quarantines,
                ..ProcessOperatorStatusCounts::default()
            },
            committed_counts: None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessOperatorStatusPhase {
    LifecycleWeeks,
    LifecycleDeadlineViolations,
    SessionSupervision,
    Complete,
}

/// Incremental reader over one coherent operator-status snapshot.
pub struct ProcessOperatorStatusReader {
    transaction: Option<Transaction<'static, Postgres>>,
    phase: ProcessOperatorStatusPhase,
    counts: ProcessOperatorStatusCounts,
    committed_counts: Option<ProcessOperatorStatusCounts>,
}

impl ProcessOperatorStatusReader {
    /// Reads the next row, advancing through the fixed section order.
    pub async fn next_item(
        &mut self,
    ) -> Result<Option<ProcessOperatorStatusItem>, ProcessOperatorStatusError> {
        loop {
            let transaction =
                self.transaction
                    .as_mut()
                    .ok_or(ProcessOperatorStatusCorruption::Missing(
                        "operator status transaction",
                    ))?;
            let (statement, next_phase) = match self.phase {
                ProcessOperatorStatusPhase::LifecycleWeeks => (
                    "FETCH NEXT FROM operator_status_lifecycle_weeks",
                    ProcessOperatorStatusPhase::LifecycleDeadlineViolations,
                ),
                ProcessOperatorStatusPhase::LifecycleDeadlineViolations => (
                    "FETCH NEXT FROM operator_status_lifecycle_deadline_violations",
                    ProcessOperatorStatusPhase::SessionSupervision,
                ),
                ProcessOperatorStatusPhase::SessionSupervision => (
                    "FETCH NEXT FROM operator_status_session_supervision",
                    ProcessOperatorStatusPhase::Complete,
                ),
                ProcessOperatorStatusPhase::Complete => {
                    return Ok(None);
                }
            };
            let row = sqlx::query(statement)
                .fetch_optional(&mut **transaction)
                .await?;
            let Some(row) = row else {
                self.phase = next_phase;
                if next_phase == ProcessOperatorStatusPhase::Complete {
                    let transaction =
                        self.transaction
                            .take()
                            .ok_or(ProcessOperatorStatusCorruption::Missing(
                                "operator status transaction",
                            ))?;
                    transaction.commit().await?;
                    self.committed_counts = Some(self.counts);
                    return Ok(None);
                }
                continue;
            };
            let item = match self.phase {
                ProcessOperatorStatusPhase::LifecycleWeeks => {
                    self.counts.lifecycle_weeks =
                        increment(self.counts.lifecycle_weeks, "lifecycle week count")?;
                    ProcessOperatorStatusItem::LifecycleWeek(decode_week(&row).map_err(
                        |error| lifecycle_read_failure(error, "lifecycle weekly metric"),
                    )?)
                }
                ProcessOperatorStatusPhase::LifecycleDeadlineViolations => {
                    self.counts.lifecycle_deadline_violations = increment(
                        self.counts.lifecycle_deadline_violations,
                        "lifecycle deadline violation count",
                    )?;
                    ProcessOperatorStatusItem::LifecycleDeadlineViolation(
                        decode_violation(&row).map_err(|error| {
                            lifecycle_read_failure(error, "lifecycle deadline violation")
                        })?,
                    )
                }
                ProcessOperatorStatusPhase::SessionSupervision => {
                    self.counts.session_supervision =
                        increment(self.counts.session_supervision, "session supervision count")?;
                    ProcessOperatorStatusItem::SessionSupervision {
                        session: SessionId::from_uuid(row.try_get("session_id")?),
                        terminal: row.try_get("terminal")?,
                        failure: decode_supervision_failure(&row)
                            .map_err(|_| {
                                ProcessOperatorStatusCorruption::Inconsistent(
                                    "session supervision failure",
                                )
                            })?
                            .ok_or(ProcessOperatorStatusCorruption::Missing(
                                "session supervision failure",
                            ))?,
                    }
                }
                ProcessOperatorStatusPhase::Complete => {
                    return Err(ProcessOperatorStatusCorruption::Inconsistent(
                        "operator status cursor phase",
                    )
                    .into());
                }
            };
            return Ok(Some(item));
        }
    }

    /// Returns counts only after all cursors committed successfully.
    pub const fn counts(&self) -> Option<ProcessOperatorStatusCounts> {
        self.committed_counts
    }
}

/// Carries one lifecycle-metric read failure into this module's own class.
///
/// A dropped connection answers `unavailable`; only corruption is a defect.
fn lifecycle_read_failure(
    error: LifecycleMetricsError,
    field: &'static str,
) -> ProcessOperatorStatusError {
    match error {
        LifecycleMetricsError::Database(error) => ProcessOperatorStatusError::Database(error),
        LifecycleMetricsError::Corruption(_) => {
            ProcessOperatorStatusCorruption::Inconsistent(field).into()
        }
    }
}

async fn declare_status_cursors(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DECLARE operator_status_session_supervision NO SCROLL CURSOR FOR
        SELECT session_id, COALESCE(state_kind = 'terminal', false) AS terminal, supervision_failure_class,
               supervision_cause_code, supervision_pending
        FROM session_supervision LEFT JOIN session_lifecycle USING (session_id)
        WHERE supervision_pending ORDER BY session_id",
    )
    .execute(&mut **transaction)
    .await?;
    // All status reads share the same repeatable-read snapshot.
    sqlx::query(DECLARE_WEEKLY_METRICS_CURSOR)
        .bind(MAX_REPORTED_WEEKS)
        .execute(&mut **transaction)
        .await?;
    sqlx::query(DECLARE_DEADLINE_VIOLATIONS_CURSOR)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn increment(value: u64, field: &'static str) -> Result<u64, ProcessOperatorStatusCorruption> {
    value
        .checked_add(1)
        .ok_or(ProcessOperatorStatusCorruption::InvalidNumber(field))
}

#[derive(signalbox_derive::OperatorError)]
/// Stored operator-status data contradicted its view contract.
#[derive(Debug)]
pub enum ProcessOperatorStatusCorruption {
    #[error("operator status is missing {field_0}")]
    Missing(&'static str),
    #[error("operator status has inconsistent {field_0}")]
    Inconsistent(&'static str),
    #[error("operator status has invalid {field_0}")]
    InvalidNumber(&'static str),
    #[error("operator status has unsupported {field} {value:?}")]
    Unsupported { field: &'static str, value: String },
}

#[derive(signalbox_derive::OperatorError)]
/// Failure to read or decode one operator-status snapshot.
#[derive(Debug)]
pub enum ProcessOperatorStatusError {
    #[error("operator-status database read failed: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error(transparent)]
    Corruption(#[source] ProcessOperatorStatusCorruption),
}

impl From<sqlx::Error> for ProcessOperatorStatusError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProcessOperatorStatusCorruption> for ProcessOperatorStatusError {
    fn from(error: ProcessOperatorStatusCorruption) -> Self {
        Self::Corruption(error)
    }
}
