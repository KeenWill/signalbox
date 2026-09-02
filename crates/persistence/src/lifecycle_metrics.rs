//! PostgreSQL adapter for the §12 lifecycle metrics, alarms, and gate.
//!
//! Nothing here computes a metric: the definitions are the views the
//! `202609020003_lifecycle_metrics.sql` migration installs. The operator
//! status surface and the Prometheus gauges read the same statements, so they
//! cannot report two different numbers for the gate.

use std::{error::Error, fmt, time::Duration};

use signalbox_domain::SessionId;
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid, types::time::PrimitiveDateTime};

use crate::mapping::session_id_from_uuid;

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
/// Every member is `None` when its bound is configured `"none"`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleMetricBounds {
    /// Processing latency past an expiry before the alarm counts it (F8).
    pub deadline_processing_grace: Option<Duration>,
    /// How long a dispatch cohort's non-terminal member defers maturity (F9).
    pub wall_cohort_maturation: Option<Duration>,
    /// Consecutive matured weekly cohorts the substrate-v0 gate requires.
    pub gate_weeks: Option<u64>,
    /// The headline's gate threshold, in parts per million.
    pub completion_failure_rate_threshold_ppm: Option<u64>,
    /// The wall-rate alarm threshold, in parts per million.
    pub wall_rate_threshold_ppm: Option<u64>,
    /// The `failed_unknown` share alarm threshold, in parts per million.
    pub failed_unknown_share_threshold_ppm: Option<u64>,
}

impl LifecycleMetricBounds {
    /// Returns each interval bound beside its durable spelling.
    fn interval_rows(&self) -> [(&'static str, Option<Duration>); 2] {
        [
            ("deadline_processing_grace", self.deadline_processing_grace),
            ("wall_cohort_maturation", self.wall_cohort_maturation),
        ]
    }

    /// Returns each count bound beside its durable spelling.
    fn count_rows(&self) -> [(&'static str, Option<u64>); 4] {
        [
            ("gate_weeks", self.gate_weeks),
            (
                "completion_failure_rate_threshold_ppm",
                self.completion_failure_rate_threshold_ppm,
            ),
            ("wall_rate_threshold_ppm", self.wall_rate_threshold_ppm),
            (
                "failed_unknown_share_threshold_ppm",
                self.failed_unknown_share_threshold_ppm,
            ),
        ]
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

    /// Returns whether the rate is strictly below a parts-per-million threshold.
    ///
    /// Cross-multiplies the exact counts: `parts_per_million` truncates, so a
    /// cohort a hair above its target reports exactly the target. The reported
    /// number is for reading; this is for deciding. An empty population is
    /// below no threshold and breaches none.
    pub const fn below(self, threshold_ppm: u64) -> Option<bool> {
        if self.denominator == 0 {
            return None;
        }
        let scaled = (self.numerator as u128) * PARTS_PER_MILLION;
        let target = (threshold_ppm as u128) * (self.denominator as u128);
        Some(scaled < target)
    }

    /// Returns whether the rate is at or above a parts-per-million threshold.
    pub const fn breaches(self, threshold_ppm: u64) -> Option<bool> {
        match self.below(threshold_ppm) {
            None => None,
            Some(below) => Some(!below),
        }
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
    wall_cohort_matured: bool,
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

    /// Returns whether the dispatch cohort has matured enough to gate on.
    pub const fn wall_cohort_matured(&self) -> bool {
        self.wall_cohort_matured
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

/// The substrate-v0 gate's verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleGateVerdict {
    /// Every required week held the headline below target with the alarm at zero.
    Met,
    /// A required week missed the target, or the integrity alarm is nonzero.
    NotMet,
    /// The gate cannot be decided: no window configured, or too few weeks.
    Indeterminate,
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

    /// Returns the latest complete week whose wall cohort has matured.
    ///
    /// A cohort still inside its maturation window is not evaluable (F9), and
    /// reading one would clear the alarm on a week that has not finished
    /// happening.
    pub fn latest_matured_wall_rate(&self) -> Option<LifecycleRate> {
        self.weeks
            .iter()
            .rev()
            .filter(|week| week.week_start < self.current_week)
            .filter(|week| week.wall_cohort_matured)
            .map(LifecycleWeeklyMetrics::wall_rate)
            .find(|rate| rate.denominator() > 0)
    }

    /// Returns whether the wall rate breached its configured threshold.
    ///
    /// Absent when no threshold is configured or no matured week measures one.
    pub fn wall_rate_breached(&self) -> Option<bool> {
        let threshold = self.bounds.wall_rate_threshold_ppm?;
        self.latest_matured_wall_rate()?.breaches(threshold)
    }

    /// Returns whether the `failed_unknown` share breached its threshold.
    pub fn failed_unknown_share_breached(&self) -> Option<bool> {
        let threshold = self.bounds.failed_unknown_share_threshold_ppm?;
        self.latest_measured(LifecycleWeeklyMetrics::failed_unknown_share)?
            .breaches(threshold)
    }

    /// Returns the substrate-v0 gate verdict.
    pub fn gate_verdict(&self) -> LifecycleGateVerdict {
        gate_verdict(
            &self.weeks,
            self.current_week,
            self.nonterminal_past_deadline,
            self.bounds,
        )
    }
}

/// Returns the substrate-v0 gate verdict for one report's parts.
///
/// Two kinds of week are never assessed: one with an empty denominator, which
/// states nothing, and the week in progress, whose members are still arriving.
///
/// The alarm has no durable weekly history — a deadline record is re-armed in
/// place — so its half of the gate reads at assessment time.
pub fn gate_verdict(
    weeks: &[LifecycleWeeklyMetrics],
    current_week: PrimitiveDateTime,
    nonterminal_past_deadline: u64,
    bounds: LifecycleMetricBounds,
) -> LifecycleGateVerdict {
    let (Some(required), Some(threshold)) = (
        bounds.gate_weeks,
        bounds.completion_failure_rate_threshold_ppm,
    ) else {
        return LifecycleGateVerdict::Indeterminate;
    };
    let Ok(required) = usize::try_from(required) else {
        return LifecycleGateVerdict::Indeterminate;
    };
    if required == 0 {
        return LifecycleGateVerdict::Indeterminate;
    }
    let assessed = weeks
        .iter()
        .filter(|week| week.week_start < current_week)
        .filter(|week| week.completion_failure.denominator() > 0)
        .rev()
        .take(required)
        .collect::<Vec<_>>();
    if assessed.len() < required {
        return LifecycleGateVerdict::Indeterminate;
    }
    if nonterminal_past_deadline > 0 {
        return LifecycleGateVerdict::NotMet;
    }
    if assessed
        .iter()
        .all(|week| week.completion_failure.below(threshold) == Some(true))
    {
        return LifecycleGateVerdict::Met;
    }
    LifecycleGateVerdict::NotMet
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
       wall_cohort_matured,
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
       (EXTRACT(EPOCH FROM interval_bound) * 1000000)::bigint AS interval_microseconds,
       count_bound
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
        for (kind, bound) in bounds.count_rows() {
            let bound = bound.map(i64::try_from).transpose().map_err(|_| {
                LifecycleMetricsError::Corruption(LifecycleMetricsCorruption::Invalid(
                    "configured metric count",
                ))
            })?;
            sqlx::query(
                "UPDATE session_lifecycle_metric_bound
                    SET count_bound = $2, updated_at = statement_timestamp()
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
            .bind(reported_week_limit(bounds))
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

/// Returns how many weeks one report carries.
///
/// The horizon bounds a wire frame; it never shortens a configured gate
/// window, which would leave that gate `indeterminate` forever.
pub(crate) fn reported_week_limit(bounds: LifecycleMetricBounds) -> i64 {
    let configured = bounds
        .gate_weeks
        .and_then(|weeks| i64::try_from(weeks).ok())
        .unwrap_or(0);
    configured.max(MAX_REPORTED_WEEKS)
}

/// Every policy row the metric definitions read.
// numeric-bound: not-a-bound - fixed size of the closed policy vocabulary
const METRIC_BOUND_KINDS: usize = 6;

pub(crate) fn decode_bounds(
    rows: &[PgRow],
) -> Result<LifecycleMetricBounds, LifecycleMetricsError> {
    // A missing row would read as the `none` default and silently disable its
    // alarm or its grace, which is the one failure a metric policy must not
    // have.
    if rows.len() != METRIC_BOUND_KINDS {
        return Err(LifecycleMetricsCorruption::Missing("metric bound row").into());
    }
    let mut bounds = LifecycleMetricBounds::default();
    for row in rows {
        let kind = row.try_get::<String, _>("bound_kind")?;
        match kind.as_str() {
            "deadline_processing_grace" => {
                bounds.deadline_processing_grace = decode_interval(row)?;
            }
            "wall_cohort_maturation" => {
                bounds.wall_cohort_maturation = decode_interval(row)?;
            }
            "gate_weeks" => bounds.gate_weeks = decode_count(row)?,
            "completion_failure_rate_threshold_ppm" => {
                bounds.completion_failure_rate_threshold_ppm = decode_count(row)?;
            }
            "wall_rate_threshold_ppm" => {
                bounds.wall_rate_threshold_ppm = decode_count(row)?;
            }
            "failed_unknown_share_threshold_ppm" => {
                bounds.failed_unknown_share_threshold_ppm = decode_count(row)?;
            }
            _ => {
                return Err(LifecycleMetricsCorruption::Invalid("metric bound kind").into());
            }
        }
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

fn decode_count(row: &PgRow) -> Result<Option<u64>, LifecycleMetricsError> {
    row.try_get::<Option<i64>, _>("count_bound")?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| LifecycleMetricsCorruption::Invalid("metric bound count").into())
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
        wall_cohort_matured: row.try_get::<bool, _>("wall_cohort_matured")?,
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
    let state = match state_kind.as_str() {
        "created" => LifecycleNonTerminalState::Created,
        "dispatched" => LifecycleNonTerminalState::Dispatched,
        "active" => LifecycleNonTerminalState::Active,
        "waiting" => LifecycleNonTerminalState::Waiting,
        "recovering" => LifecycleNonTerminalState::Recovering,
        "blocked" => LifecycleNonTerminalState::Blocked,
        "parked" => LifecycleNonTerminalState::Parked,
        _ => {
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
    use sqlx::types::time::{Date, Time};

    fn week(start_day: u16, numerator: u64, denominator: u64) -> LifecycleWeeklyMetrics {
        let date = Date::from_ordinal_date(2026, start_day)
            .expect("the fixture names a real ordinal date");
        LifecycleWeeklyMetrics {
            week_start: PrimitiveDateTime::new(date, Time::MIDNIGHT),
            completion_failure: LifecycleRate::new(numerator, denominator),
            failed_unknown_share: LifecycleRate::new(0, denominator),
            overflow_incidence: LifecycleRate::new(0, denominator),
            finish_given_overflow: LifecycleRate::new(0, 0),
            wall_rate: LifecycleRate::new(0, 0),
            wall_cohort_matured: true,
            wall_occurrences: 0,
            turn_cause_completeness: LifecycleRate::new(0, 0),
            model_call_cause_completeness: LifecycleRate::new(0, 0),
        }
    }

    /// The week every fixture calls "in progress", one past its latest cohort.
    fn current_week() -> PrimitiveDateTime {
        PrimitiveDateTime::new(
            Date::from_ordinal_date(2026, 31).expect("the fixture names a real ordinal date"),
            Time::MIDNIGHT,
        )
    }

    const fn gate_policy(weeks: u64, threshold_ppm: u64) -> LifecycleMetricBounds {
        LifecycleMetricBounds {
            deadline_processing_grace: None,
            wall_cohort_maturation: None,
            gate_weeks: Some(weeks),
            completion_failure_rate_threshold_ppm: Some(threshold_ppm),
            wall_rate_threshold_ppm: None,
            failed_unknown_share_threshold_ppm: None,
        }
    }

    /// An empty population states nothing, and nothing is not zero.
    #[test]
    fn an_empty_population_reports_no_rate_and_no_verdict() {
        let empty = LifecycleRate::new(0, 0);

        assert_eq!(empty.parts_per_million(), None);
        assert_eq!(empty.below(1), None);
    }

    /// §12 states the gate as the headline below its target, not at it.
    #[test]
    fn a_rate_exactly_at_its_threshold_is_not_below_it() {
        let one_in_ten = LifecycleRate::new(1, 10);

        assert_eq!(one_in_ten.parts_per_million(), Some(100_000));
        assert_eq!(one_in_ten.below(100_000), Some(false));
        assert_eq!(one_in_ten.below(100_001), Some(true));
    }

    /// The report never derives a rate it was not given the counts for, so a
    /// truncating division cannot overstate one either.
    #[test]
    fn a_reported_rate_never_overstates_its_counts() {
        let two_in_three = LifecycleRate::new(2, 3);

        assert_eq!(two_in_three.parts_per_million(), Some(666_666));
        assert_eq!(two_in_three.numerator(), 2);
        assert_eq!(two_in_three.denominator(), 3);
    }

    /// A deployment that configured no gate window has no gate to fail.
    #[test]
    fn the_gate_is_indeterminate_without_a_configured_window() {
        let verdict = gate_verdict(
            &[week(24, 0, 10)],
            current_week(),
            0,
            LifecycleMetricBounds::default(),
        );

        assert_eq!(verdict, LifecycleGateVerdict::Indeterminate);
    }

    /// A week with no cohort members states nothing about the headline, so it
    /// is skipped rather than counted as a pass: a gate consecutive quiet
    /// weeks could satisfy would measure quiet, not reliability.
    #[test]
    fn an_empty_week_does_not_count_toward_the_gate_window() {
        let quiet = gate_verdict(
            &[week(17, 0, 0), week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(2, 100_000),
        );
        let populated = gate_verdict(
            &[week(17, 0, 10), week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(2, 100_000),
        );

        assert_eq!(quiet, LifecycleGateVerdict::Indeterminate);
        assert_eq!(populated, LifecycleGateVerdict::Met);
    }

    /// The companion alarm is the headline's integrity condition: a cohort
    /// thinned by sessions stuck outside `terminal` passes nothing.
    #[test]
    fn a_live_integrity_alarm_fails_a_gate_the_headline_would_pass() {
        let alarmed = gate_verdict(
            &[week(24, 0, 10)],
            current_week(),
            1,
            gate_policy(1, 100_000),
        );
        let silent = gate_verdict(
            &[week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(1, 100_000),
        );

        assert_eq!(alarmed, LifecycleGateVerdict::NotMet);
        assert_eq!(silent, LifecycleGateVerdict::Met);
    }

    /// Only the most recent weeks the window covers are graded, and one breach
    /// among them ends the run.
    #[test]
    fn a_breach_inside_the_window_fails_the_gate() {
        let breached = gate_verdict(
            &[week(17, 2, 10), week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(2, 100_000),
        );
        let outside_window = gate_verdict(
            &[week(17, 2, 10), week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(1, 100_000),
        );

        assert_eq!(breached, LifecycleGateVerdict::NotMet);
        assert_eq!(outside_window, LifecycleGateVerdict::Met);
    }

    /// §12 asks for a result sustained across completed weekly cohorts, and
    /// the week in progress is not one: its members are still arriving, so one
    /// early success would satisfy a window that later failures reverse.
    #[test]
    fn the_week_in_progress_never_counts_toward_the_gate() {
        let in_progress = gate_verdict(
            &[week(31, 0, 10)],
            current_week(),
            0,
            gate_policy(1, 100_000),
        );
        let completed = gate_verdict(
            &[week(24, 0, 10)],
            current_week(),
            0,
            gate_policy(1, 100_000),
        );

        assert_eq!(in_progress, LifecycleGateVerdict::Indeterminate);
        assert_eq!(completed, LifecycleGateVerdict::Met);
    }

    /// A window wider than the report horizon still reads every week it has,
    /// rather than sitting at `indeterminate` because the horizon truncated
    /// the history the gate was configured to grade.
    #[test]
    fn a_gate_window_wider_than_the_horizon_widens_the_report() {
        let narrow = reported_week_limit(gate_policy(4, 100_000));
        let wide = reported_week_limit(gate_policy(400, 100_000));

        assert_eq!(narrow, MAX_REPORTED_WEEKS);
        assert_eq!(wide, 400);
    }

    /// The rate a report prints truncates; the verdict it reaches does not.
    #[test]
    fn a_rate_a_hair_above_its_target_does_not_pass_on_the_printed_number() {
        let hair_above = LifecycleRate::new(1_000_001, 10_000_000);

        assert_eq!(hair_above.parts_per_million(), Some(100_000));
        assert_eq!(hair_above.below(100_000), Some(false));
        assert_eq!(hair_above.breaches(100_000), Some(true));
    }
}
