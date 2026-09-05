//! Read-only lifecycle projections for the operator status command.

use std::{error::Error, fmt};

use sqlx::{PgPool, Postgres, Transaction, types::Uuid};

use crate::lifecycle_metrics::{
    DECLARE_DEADLINE_VIOLATIONS_CURSOR, DECLARE_WEEKLY_METRICS_CURSOR, LifecycleDeadlineViolation,
    LifecycleMetricsError, LifecycleWeeklyMetrics, MAX_REPORTED_WEEKS, decode_violation,
    decode_week,
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// Singleton key class carried by an operator-status row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusSingletonScope {
    PullRequest,
    Stack,
    Rule,
    Repo,
}

/// One independently failing held-slot release clause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}

/// Current provider mergeability in a convergence assessment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusMergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Current provider review decision in a convergence assessment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusReviewDecision {
    None,
    Approved,
    ReviewRequired,
    ChangesRequested,
}

/// Latest convergence verdict for one pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusConvergenceVerdict {
    NotConverged,
    InternallyConverged,
    MergeReady,
}

/// Durable convergence seal attached to the latest assessment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusConvergenceSeal {
    InternallyConverged,
    MergeReady,
}

/// One decoded singleton key from a repository-watch status projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOperatorStatusSingleton {
    pub(crate) scope: ProcessOperatorStatusSingletonScope,
    pub(crate) repository: Option<String>,
    pub(crate) pull_request_number: Option<u64>,
    pub(crate) stack_root_pull_request_number: Option<u64>,
}

impl ProcessOperatorStatusSingleton {
    pub const fn scope(&self) -> ProcessOperatorStatusSingletonScope {
        self.scope
    }

    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    pub const fn pull_request_number(&self) -> Option<u64> {
        self.pull_request_number
    }

    pub const fn stack_root_pull_request_number(&self) -> Option<u64> {
        self.stack_root_pull_request_number
    }
}

/// Origin fact whose dispatch took one repository-watch singleton slot.
///
/// A rule matching `branch_workflow_run_completed` under `Rule` or
/// `Repository` singleton scope holds a slot from a branch fact, which names no
/// pull request; every other admitted origin names one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusHeldSlotOrigin {
    PullRequest { number: u64 },
    Branch { branch: String },
}

/// One active repository-watch dispatch slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOperatorStatusHeldSlot {
    dispatch_id: Uuid,
    repository: String,
    origin: ProcessOperatorStatusHeldSlotOrigin,
    rule_id: String,
    rule_version: u64,
    singleton: ProcessOperatorStatusSingleton,
    held_for_seconds: u64,
    session_ids: Vec<Uuid>,
    blockers: Vec<ProcessOperatorStatusHeldSlotBlocker>,
}

impl ProcessOperatorStatusHeldSlot {
    pub const fn dispatch_id(&self) -> Uuid {
        self.dispatch_id
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub const fn origin(&self) -> &ProcessOperatorStatusHeldSlotOrigin {
        &self.origin
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub const fn rule_version(&self) -> u64 {
        self.rule_version
    }

    pub const fn singleton(&self) -> &ProcessOperatorStatusSingleton {
        &self.singleton
    }

    pub const fn held_for_seconds(&self) -> u64 {
        self.held_for_seconds
    }

    pub fn session_ids(&self) -> &[Uuid] {
        &self.session_ids
    }

    pub fn blockers(&self) -> &[ProcessOperatorStatusHeldSlotBlocker] {
        &self.blockers
    }
}

/// One owed repository-watch dispatch waiting for admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOperatorStatusQueuedObligation {
    obligation_id: Uuid,
    repository: String,
    rule_id: String,
    rule_version: u64,
    singleton: ProcessOperatorStatusSingleton,
    first_event_id: Uuid,
    latest_event_id: Uuid,
    matched_event_count: u64,
    waiting_for_seconds: u64,
    occupying_dispatch_id: Option<Uuid>,
    occupying_session_ids: Vec<Uuid>,
    cooldown_remaining_seconds: Option<u64>,
    cooldown_never_eligible: bool,
    ready: bool,
}

impl ProcessOperatorStatusQueuedObligation {
    pub const fn obligation_id(&self) -> Uuid {
        self.obligation_id
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub const fn rule_version(&self) -> u64 {
        self.rule_version
    }

    pub const fn singleton(&self) -> &ProcessOperatorStatusSingleton {
        &self.singleton
    }

    pub const fn first_event_id(&self) -> Uuid {
        self.first_event_id
    }

    pub const fn latest_event_id(&self) -> Uuid {
        self.latest_event_id
    }

    pub const fn matched_event_count(&self) -> u64 {
        self.matched_event_count
    }

    pub const fn waiting_for_seconds(&self) -> u64 {
        self.waiting_for_seconds
    }

    pub const fn occupying_dispatch_id(&self) -> Option<Uuid> {
        self.occupying_dispatch_id
    }

    pub fn occupying_session_ids(&self) -> &[Uuid] {
        &self.occupying_session_ids
    }

    pub const fn cooldown_remaining_seconds(&self) -> Option<u64> {
        self.cooldown_remaining_seconds
    }

    pub const fn cooldown_never_eligible(&self) -> bool {
        self.cooldown_never_eligible
    }

    pub const fn ready(&self) -> bool {
        self.ready
    }
}

/// One latest pull-request convergence assessment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOperatorStatusPullRequestConvergence {
    repository: String,
    pull_request_number: u64,
    head_sha: String,
    base_branch: String,
    base_revision: String,
    mergeable_state: ProcessOperatorStatusMergeableState,
    review_decision: ProcessOperatorStatusReviewDecision,
    unresolved_thread_count: u64,
    gating_check_count: u64,
    non_green_gating_checks: Vec<String>,
    verdict: ProcessOperatorStatusConvergenceVerdict,
    seal: Option<ProcessOperatorStatusConvergenceSeal>,
    assessed_seconds_ago: u64,
}

impl ProcessOperatorStatusPullRequestConvergence {
    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub const fn pull_request_number(&self) -> u64 {
        self.pull_request_number
    }

    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    pub fn base_branch(&self) -> &str {
        &self.base_branch
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub const fn mergeable_state(&self) -> ProcessOperatorStatusMergeableState {
        self.mergeable_state
    }

    pub const fn review_decision(&self) -> ProcessOperatorStatusReviewDecision {
        self.review_decision
    }

    pub const fn unresolved_thread_count(&self) -> u64 {
        self.unresolved_thread_count
    }

    pub const fn gating_check_count(&self) -> u64 {
        self.gating_check_count
    }

    pub fn non_green_gating_checks(&self) -> &[String] {
        &self.non_green_gating_checks
    }

    pub const fn verdict(&self) -> ProcessOperatorStatusConvergenceVerdict {
        self.verdict
    }

    pub const fn seal(&self) -> Option<ProcessOperatorStatusConvergenceSeal> {
        self.seal
    }

    pub const fn assessed_seconds_ago(&self) -> u64 {
        self.assessed_seconds_ago
    }
}

/// One stale blocking review whose planned clearance is not yet settled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOperatorStatusPendingStaleReviewClearance {
    repository: String,
    pull_request_number: u64,
    current_head_sha: String,
    review_node_id: String,
    reviewer: String,
    reviewed_head_sha: String,
    pending_for_seconds: u64,
}

impl ProcessOperatorStatusPendingStaleReviewClearance {
    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub const fn pull_request_number(&self) -> u64 {
        self.pull_request_number
    }

    pub fn current_head_sha(&self) -> &str {
        &self.current_head_sha
    }

    pub fn review_node_id(&self) -> &str {
        &self.review_node_id
    }

    pub fn reviewer(&self) -> &str {
        &self.reviewer
    }

    pub fn reviewed_head_sha(&self) -> &str {
        &self.reviewed_head_sha
    }

    pub const fn pending_for_seconds(&self) -> u64 {
        self.pending_for_seconds
    }
}

/// One row in the fixed-phase operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessOperatorStatusItem {
    HeldSlot(ProcessOperatorStatusHeldSlot),
    QueuedObligation(ProcessOperatorStatusQueuedObligation),
    PullRequestConvergence(ProcessOperatorStatusPullRequestConvergence),
    PendingStaleReviewClearance(ProcessOperatorStatusPendingStaleReviewClearance),
    /// One calendar week's §12 metrics.
    LifecycleWeek(LifecycleWeeklyMetrics),
    /// One owned non-terminal session past its §1 deadline obligation.
    LifecycleDeadlineViolation(LifecycleDeadlineViolation),
}

/// Counts committed after every status cursor has been exhausted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessOperatorStatusCounts {
    held_slots: u64,
    queued_obligations: u64,
    pull_request_convergences: u64,
    pending_stale_review_clearances: u64,
    lifecycle_weeks: u64,
    lifecycle_deadline_violations: u64,
}

impl ProcessOperatorStatusCounts {
    pub const fn held_slots(self) -> u64 {
        self.held_slots
    }

    pub const fn queued_obligations(self) -> u64 {
        self.queued_obligations
    }

    pub const fn pull_request_convergences(self) -> u64 {
        self.pull_request_convergences
    }

    pub const fn pending_stale_review_clearances(self) -> u64 {
        self.pending_stale_review_clearances
    }

    pub const fn lifecycle_weeks(self) -> u64 {
        self.lifecycle_weeks
    }

    /// Returns the `nonterminal_past_deadline` alarm value, target zero.
    pub const fn lifecycle_deadline_violations(self) -> u64 {
        self.lifecycle_deadline_violations
    }
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
        declare_status_cursors(&mut transaction).await?;
        Ok(ProcessOperatorStatusReader {
            transaction: Some(transaction),
            phase: ProcessOperatorStatusPhase::LifecycleWeeks,
            counts: ProcessOperatorStatusCounts::default(),
            committed_counts: None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessOperatorStatusPhase {
    LifecycleWeeks,
    LifecycleDeadlineViolations,
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
    // The two lifecycle-metric sections read the same views the telemetry pass reads, in
    // the same snapshot as the sections above.
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

/// Stored operator-status data contradicted its view contract.
#[derive(Debug)]
pub enum ProcessOperatorStatusCorruption {
    Missing(&'static str),
    Inconsistent(&'static str),
    InvalidNumber(&'static str),
    Unsupported { field: &'static str, value: String },
}

impl fmt::Display for ProcessOperatorStatusCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "operator status is missing {field}"),
            Self::Inconsistent(field) => {
                write!(formatter, "operator status has inconsistent {field}")
            }
            Self::InvalidNumber(field) => write!(formatter, "operator status has invalid {field}"),
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "operator status has unsupported {field} {value:?}"
                )
            }
        }
    }
}

impl Error for ProcessOperatorStatusCorruption {}

/// Failure to read or decode one operator-status snapshot.
#[derive(Debug)]
pub enum ProcessOperatorStatusError {
    Database(sqlx::Error),
    Corruption(ProcessOperatorStatusCorruption),
}

impl fmt::Display for ProcessOperatorStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "operator-status database read failed: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProcessOperatorStatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
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
