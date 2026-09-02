//! PostgreSQL adapter for the §12 lifecycle metrics, alarms, and gate.
//!
//! Nothing here computes a metric: the definitions are the views the
//! `202609020003_lifecycle_metrics.sql` migration installs. The operator
//! status surface and the Prometheus gauges read the same statements, so they
//! cannot report two different numbers for the gate.

use std::{error::Error, fmt, time::Duration};

use signalbox_domain::SessionId;
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid, types::time::PrimitiveDateTime};

use crate::mapping::{
    SessionLifecycleStateKind, session_id_from_uuid, session_lifecycle_state_kind_from_str,
};

/// How many weekly cohorts one report carries by default.
///
/// Weeks accrue forever and the report is written to a wire frame, so it names
/// its own horizon.
// numeric-bound: guard - bounds one metric report to a fixed number of weeks
pub(crate) const MAX_REPORTED_WEEKS: i64 = 104;

/// Parts per million, the unit every rate threshold is configured in.
// numeric-bound: not-a-bound - fixed-point scale for exact rate arithmetic
const PARTS_PER_MILLION: u128 = 1_000_000;

/// A durable metric shape this module cannot read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleMetricsCorruption {
    Missing(&'static str),
    Invalid(&'static str),
}

impl fmt::Display for LifecycleMetricsCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing lifecycle metric {field}"),
            Self::Invalid(field) => write!(formatter, "invalid lifecycle metric {field}"),
        }
    }
}

impl Error for LifecycleMetricsCorruption {}

/// Why one lifecycle metric read produced no report.
#[derive(Debug)]
pub enum LifecycleMetricsError {
    Database(sqlx::Error),
    Corruption(LifecycleMetricsCorruption),
}

impl fmt::Display for LifecycleMetricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "lifecycle metric read failed: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for LifecycleMetricsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for LifecycleMetricsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<LifecycleMetricsCorruption> for LifecycleMetricsError {
    fn from(error: LifecycleMetricsCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// The deployment's configured §12 policy.
///
/// `None` is the bound configured `"none"`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleMetricBounds {
    /// Processing latency past an expiry before a deadline counts (F8).
    pub deadline_processing_grace: Option<Duration>,
}

impl LifecycleMetricBounds {
    /// Returns each interval bound beside its durable spelling.
    fn interval_rows(&self) -> [(&'static str, Option<Duration>); 1] {
        [("deadline_processing_grace", self.deadline_processing_grace)]
    }
}

/// One metric's exact numerator and denominator.
///
/// The rate is derived, so a week with no members reports no rate at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleRate {
    numerator: u64,
    denominator: u64,
}

impl LifecycleRate {
    const fn new(numerator: u64, denominator: u64) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Returns the counted members.
    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    /// Returns the population the count is over.
    pub const fn denominator(self) -> u64 {
        self.denominator
    }

    /// Returns the rate in parts per million, absent for an empty population.
    ///
    /// Truncating, so a reported value never overstates the rate.
    pub const fn parts_per_million(self) -> Option<u64> {
        if self.denominator == 0 {
            return None;
        }
        let scaled = (self.numerator as u128) * PARTS_PER_MILLION / (self.denominator as u128);
        Some(scaled as u64)
    }
}

/// One calendar week's §12 report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleWeeklyMetrics {
    week_start: PrimitiveDateTime,
    completion_failure: LifecycleRate,
    failed_unknown_share: LifecycleRate,
    overflow_incidence: LifecycleRate,
    finish_given_overflow: LifecycleRate,
    wall_rate: LifecycleRate,
    wall_occurrences: u64,
    turn_cause_completeness: LifecycleRate,
    model_call_cause_completeness: LifecycleRate,
}

impl LifecycleWeeklyMetrics {
    /// Returns the UTC instant the calendar week begins at.
    pub const fn week_start(&self) -> PrimitiveDateTime {
        self.week_start
    }

    /// Returns the week's UTC start as an ISO-8601 calendar date.
    pub fn week_start_date(&self) -> String {
        let date = self.week_start.date();
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    }

    /// Returns the headline: the §12 completion-failure rate for this week.
    pub const fn completion_failure(&self) -> LifecycleRate {
        self.completion_failure
    }

    /// Returns the `failed_unknown` share over the headline's denominator.
    pub const fn failed_unknown_share(&self) -> LifecycleRate {
        self.failed_unknown_share
    }

    /// Returns overflow incidence over the untrimmed terminal cohort.
    pub const fn overflow_incidence(&self) -> LifecycleRate {
        self.overflow_incidence
    }

    /// Returns `P(finish | overflow)` over this week's overflow sessions.
    pub const fn finish_given_overflow(&self) -> LifecycleRate {
        self.finish_given_overflow
    }

    /// Returns the wall rate over this week's dispatch cohort.
    pub const fn wall_rate(&self) -> LifecycleRate {
        self.wall_rate
    }

    /// Returns walls recorded in this week, whatever cohort they belong to.
    pub const fn wall_occurrences(&self) -> u64 {
        self.wall_occurrences
    }

    /// Returns cause completeness over this week's terminal turns.
    pub const fn turn_cause_completeness(&self) -> LifecycleRate {
        self.turn_cause_completeness
    }

    /// Returns cause completeness over this week's `known_failed` calls.
    pub const fn model_call_cause_completeness(&self) -> LifecycleRate {
        self.model_call_cause_completeness
    }
}

/// One state an owned session can hold a deadline obligation in.
///
/// `terminal` is not a member: a terminal session owes no deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleNonTerminalState {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
}

/// One owned non-terminal session violating §1's armed-deadline invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleDeadlineViolation {
    session: SessionId,
    state: LifecycleNonTerminalState,
    deadline_kind: Option<String>,
    expired_for_seconds: Option<u64>,
}

impl LifecycleDeadlineViolation {
    /// Returns the session the operator must look at.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the durable state the session is stuck in.
    pub const fn state(&self) -> LifecycleNonTerminalState {
        self.state
    }

    /// Returns the armed deadline's kind, absent when no record exists.
    pub fn deadline_kind(&self) -> Option<&str> {
        self.deadline_kind.as_deref()
    }

    /// Returns how long the expiry has been past, absent for a missing record.
    pub const fn expired_for_seconds(&self) -> Option<u64> {
        self.expired_for_seconds
    }
}

/// One coherent §12 report.
///
/// Carries the alarm as a count, not as rows: a widespread incident is exactly
/// when a periodic reader must not build one object per stuck session. The
/// rows stream through the operator-status snapshot's cursor instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleMetricsReport {
    weeks: Vec<LifecycleWeeklyMetrics>,
    current_week: PrimitiveDateTime,
    nonterminal_past_deadline: u64,
    bounds: LifecycleMetricBounds,
}

impl LifecycleMetricsReport {
    /// Returns the weekly cohorts, oldest first.
    pub fn weeks(&self) -> &[LifecycleWeeklyMetrics] {
        &self.weeks
    }

    /// Returns the configured policy the report was computed under.
    pub const fn bounds(&self) -> LifecycleMetricBounds {
        self.bounds
    }

    /// Returns the `nonterminal_past_deadline` alarm value, target zero.
    pub const fn nonterminal_past_deadline(&self) -> u64 {
        self.nonterminal_past_deadline
    }

    /// Returns the most recent complete week that measures one metric.
    ///
    /// Each metric is searched for independently: a cohort that is still
    /// growing is not a cohort, and a complete week that happens to have no
    /// members for one metric says nothing about it while still saying
    /// something about the others.
    pub fn latest_measured<Select>(&self, select: Select) -> Option<LifecycleRate>
    where
        Select: Fn(&LifecycleWeeklyMetrics) -> LifecycleRate,
    {
        self.weeks
            .iter()
            .rev()
            .filter(|week| week.week_start < self.current_week)
            .map(select)
            .find(|rate| rate.denominator() > 0)
    }
}

/// PostgreSQL-backed §12 metric read boundary.
#[derive(Clone, Debug)]
pub struct LifecycleMetricsRepository {
    pool: PgPool,
}

/// The weekly report, as the one statement both readers issue.
macro_rules! weekly_metrics_sql {
    () => {
        "SELECT week,
       terminal_cohort_size,
       completion_failure_denominator,
       completion_failure_numerator,
       failed_unknown_count,
       overflow_count,
       overflow_finished_count,
       dispatch_cohort_size,
       wall_count,
       wall_occurrence_count,
       terminal_turn_count,
       classified_terminal_turn_count,
       known_failed_call_count,
       classified_known_failed_call_count
  FROM (
        SELECT *
          FROM session_lifecycle_weekly_metric
         ORDER BY week DESC
         LIMIT $1
       ) AS recent
 ORDER BY week"
    };
}

macro_rules! deadline_violations_sql {
    () => {
        "SELECT session_id,
       state_kind,
       deadline_kind,
       CASE
           WHEN deadline_missing THEN NULL
           ELSE GREATEST(
               0,
               floor(EXTRACT(EPOCH FROM (clock_timestamp() - expires_at)))::bigint
           )
       END AS expired_for_seconds
  FROM session_lifecycle_deadline_violation
 ORDER BY session_id"
    };
}

pub(crate) const SELECT_WEEKLY_METRICS: &str = weekly_metrics_sql!();

pub(crate) const DECLARE_WEEKLY_METRICS_CURSOR: &str = concat!(
    "DECLARE operator_status_lifecycle_weeks NO SCROLL CURSOR FOR ",
    weekly_metrics_sql!()
);

pub(crate) const COUNT_DEADLINE_VIOLATIONS: &str =
    "SELECT count(*)::bigint AS count FROM session_lifecycle_deadline_violation";

/// The calendar week the read itself falls in.
pub(crate) const SELECT_CURRENT_WEEK: &str =
    "SELECT session_lifecycle_metric_week(clock_timestamp()) AS week";

pub(crate) const DECLARE_DEADLINE_VIOLATIONS_CURSOR: &str = concat!(
    "DECLARE operator_status_lifecycle_deadline_violations NO SCROLL CURSOR FOR ",
    deadline_violations_sql!()
);

pub(crate) const SELECT_BOUNDS: &str = "SELECT bound_kind,
       (EXTRACT(EPOCH FROM interval_bound) * 1000000)::bigint AS interval_microseconds
  FROM session_lifecycle_metric_bound";

impl LifecycleMetricsRepository {
    /// Uses the supplied pool for independent read-only snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Writes the deployment's configured §12 policy where the views read it.
    pub async fn apply_configured_bounds(
        &self,
        bounds: &LifecycleMetricBounds,
    ) -> Result<(), LifecycleMetricsError> {
        let mut transaction = self.pool.begin().await?;
        for (kind, bound) in bounds.interval_rows() {
            sqlx::query(
                "UPDATE session_lifecycle_metric_bound
                    SET interval_bound = $2, updated_at = statement_timestamp()
                  WHERE bound_kind = $1",
            )
            .bind(kind)
            .bind(bound)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Reads one coherent report over every §12 definition.
    ///
    /// One repeatable-read snapshot, so a cohort and the alarm that guards it
    /// describe the same instant.
    pub async fn read(&self) -> Result<LifecycleMetricsReport, LifecycleMetricsError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let bound_rows = sqlx::query(SELECT_BOUNDS)
            .fetch_all(&mut *transaction)
            .await?;
        let bounds = decode_bounds(&bound_rows)?;
        let current_week = sqlx::query(SELECT_CURRENT_WEEK)
            .fetch_one(&mut *transaction)
            .await?
            .try_get::<PrimitiveDateTime, _>("week")?;
        let weekly_rows = sqlx::query(SELECT_WEEKLY_METRICS)
            .bind(MAX_REPORTED_WEEKS)
            .fetch_all(&mut *transaction)
            .await?;
        // Only the count; the rows stream through the status snapshot.
        let nonterminal_past_deadline = sqlx::query(COUNT_DEADLINE_VIOLATIONS)
            .fetch_one(&mut *transaction)
            .await?;
        transaction.commit().await?;
        let weeks = weekly_rows
            .iter()
            .map(decode_week)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LifecycleMetricsReport {
            weeks,
            current_week,
            nonterminal_past_deadline: counted(&nonterminal_past_deadline, "count")?,
            bounds,
        })
    }
}

pub(crate) fn decode_bounds(
    rows: &[PgRow],
) -> Result<LifecycleMetricBounds, LifecycleMetricsError> {
    // A missing row would read as the `none` default and silently drop the
    // grace, which is the one failure a metric policy must not have.
    if rows.len() != 1 {
        return Err(LifecycleMetricsCorruption::Missing("metric bound row").into());
    }
    let mut bounds = LifecycleMetricBounds::default();
    for row in rows {
        let kind = row.try_get::<String, _>("bound_kind")?;
        if kind != "deadline_processing_grace" {
            return Err(LifecycleMetricsCorruption::Invalid("metric bound kind").into());
        }
        bounds.deadline_processing_grace = decode_interval(row)?;
    }
    Ok(bounds)
}

fn decode_interval(row: &PgRow) -> Result<Option<Duration>, LifecycleMetricsError> {
    // Read as whole microseconds: an `interval`'s month component means a
    // different number of seconds depending on when it is added, which is not
    // a bound. The daemon writes these from `Duration`s, so it is always zero.
    row.try_get::<Option<i64>, _>("interval_microseconds")?
        .map(u64::try_from)
        .transpose()
        .map(|microseconds| microseconds.map(Duration::from_micros))
        .map_err(|_| LifecycleMetricsCorruption::Invalid("metric bound interval").into())
}

pub(crate) fn decode_week(row: &PgRow) -> Result<LifecycleWeeklyMetrics, LifecycleMetricsError> {
    let week_start = row.try_get::<PrimitiveDateTime, _>("week")?;
    let completion_failure = LifecycleRate::new(
        counted(row, "completion_failure_numerator")?,
        counted(row, "completion_failure_denominator")?,
    );
    let failed_unknown_share = LifecycleRate::new(
        counted(row, "failed_unknown_count")?,
        completion_failure.denominator(),
    );
    let overflow_count = counted(row, "overflow_count")?;
    let overflow_incidence =
        LifecycleRate::new(overflow_count, counted(row, "terminal_cohort_size")?);
    let finish_given_overflow =
        LifecycleRate::new(counted(row, "overflow_finished_count")?, overflow_count);
    let wall_rate = LifecycleRate::new(
        counted(row, "wall_count")?,
        counted(row, "dispatch_cohort_size")?,
    );
    let turn_cause_completeness = LifecycleRate::new(
        counted(row, "classified_terminal_turn_count")?,
        counted(row, "terminal_turn_count")?,
    );
    let model_call_cause_completeness = LifecycleRate::new(
        counted(row, "classified_known_failed_call_count")?,
        counted(row, "known_failed_call_count")?,
    );
    Ok(LifecycleWeeklyMetrics {
        week_start,
        completion_failure,
        failed_unknown_share,
        overflow_incidence,
        finish_given_overflow,
        wall_rate,
        wall_occurrences: counted(row, "wall_occurrence_count")?,
        turn_cause_completeness,
        model_call_cause_completeness,
    })
}

pub(crate) fn decode_violation(
    row: &PgRow,
) -> Result<LifecycleDeadlineViolation, LifecycleMetricsError> {
    let expired_for_seconds = row
        .try_get::<Option<i64>, _>("expired_for_seconds")?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| LifecycleMetricsCorruption::Invalid("deadline expiry age"))?;
    let state_kind = row.try_get::<String, _>("state_kind")?;
    let state = match session_lifecycle_state_kind_from_str(&state_kind) {
        Some(SessionLifecycleStateKind::Created) => LifecycleNonTerminalState::Created,
        Some(SessionLifecycleStateKind::Dispatched) => LifecycleNonTerminalState::Dispatched,
        Some(SessionLifecycleStateKind::Active) => LifecycleNonTerminalState::Active,
        Some(SessionLifecycleStateKind::Waiting) => LifecycleNonTerminalState::Waiting,
        Some(SessionLifecycleStateKind::Recovering) => LifecycleNonTerminalState::Recovering,
        Some(SessionLifecycleStateKind::Blocked) => LifecycleNonTerminalState::Blocked,
        Some(SessionLifecycleStateKind::Parked) => LifecycleNonTerminalState::Parked,
        // A terminal session owes no deadline, so the alarm cannot name one.
        Some(SessionLifecycleStateKind::Terminal) | None => {
            return Err(LifecycleMetricsCorruption::Invalid("session lifecycle state").into());
        }
    };
    Ok(LifecycleDeadlineViolation {
        session: session_id_from_uuid(row.try_get::<Uuid, _>("session_id")?),
        state,
        deadline_kind: row.try_get::<Option<String>, _>("deadline_kind")?,
        expired_for_seconds,
    })
}

fn counted(row: &PgRow, field: &'static str) -> Result<u64, LifecycleMetricsError> {
    let value = row.try_get::<i64, _>(field)?;
    u64::try_from(value).map_err(|_| LifecycleMetricsCorruption::Invalid(field).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty population states nothing, and nothing is not zero.
    #[test]
    fn an_empty_population_reports_no_rate() {
        assert_eq!(LifecycleRate::new(0, 0).parts_per_million(), None);
    }

    /// The reported rate truncates, so it never overstates the counts it is
    /// derived from.
    #[test]
    fn a_reported_rate_never_overstates_its_counts() {
        let two_in_three = LifecycleRate::new(2, 3);

        assert_eq!(two_in_three.parts_per_million(), Some(666_666));
        assert_eq!(two_in_three.numerator(), 2);
        assert_eq!(two_in_three.denominator(), 3);
    }
}
