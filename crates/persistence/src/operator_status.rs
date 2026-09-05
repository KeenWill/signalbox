//! Read-only repository-watch projections for the operator status command.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{RepoWatchConvergenceVerdict, RepoWatchReviewDecision};
use signalbox_domain::MergeableState;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    lifecycle_metrics::{
        DECLARE_DEADLINE_VIOLATIONS_CURSOR, DECLARE_WEEKLY_METRICS_CURSOR,
        LifecycleDeadlineViolation, LifecycleMetricsError, LifecycleWeeklyMetrics,
        MAX_REPORTED_WEEKS, decode_violation, decode_week,
    },
    mapping::{
        RepoWatchSingletonScopeStorageKind, repo_watch_convergence_verdict_from_str,
        repo_watch_mergeable_state_from_str, repo_watch_review_decision_from_str,
        repo_watch_singleton_scope_from_str,
    },
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
    /// One calendar week's lifecycle metrics.
    LifecycleWeek(LifecycleWeeklyMetrics),
    /// One owned non-terminal session past its deadline obligation.
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

    /// Opens one coherent snapshot over all four repository-watch views.
    pub async fn open(&self) -> Result<ProcessOperatorStatusReader, ProcessOperatorStatusError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        declare_status_cursors(&mut transaction).await?;
        Ok(ProcessOperatorStatusReader {
            transaction: Some(transaction),
            phase: ProcessOperatorStatusPhase::HeldSlots,
            counts: ProcessOperatorStatusCounts::default(),
            committed_counts: None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessOperatorStatusPhase {
    HeldSlots,
    QueuedObligations,
    PullRequestConvergences,
    PendingStaleReviewClearances,
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
                ProcessOperatorStatusPhase::HeldSlots => (
                    "FETCH NEXT FROM operator_status_held_slots",
                    ProcessOperatorStatusPhase::QueuedObligations,
                ),
                ProcessOperatorStatusPhase::QueuedObligations => (
                    "FETCH NEXT FROM operator_status_queued_obligations",
                    ProcessOperatorStatusPhase::PullRequestConvergences,
                ),
                ProcessOperatorStatusPhase::PullRequestConvergences => (
                    "FETCH NEXT FROM operator_status_pull_request_convergences",
                    ProcessOperatorStatusPhase::PendingStaleReviewClearances,
                ),
                ProcessOperatorStatusPhase::PendingStaleReviewClearances => (
                    "FETCH NEXT FROM operator_status_pending_stale_review_clearances",
                    ProcessOperatorStatusPhase::LifecycleWeeks,
                ),
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
                ProcessOperatorStatusPhase::HeldSlots => {
                    self.counts.held_slots = increment(self.counts.held_slots, "held slot count")?;
                    ProcessOperatorStatusItem::HeldSlot(decode_held_slot(&row)?)
                }
                ProcessOperatorStatusPhase::QueuedObligations => {
                    self.counts.queued_obligations =
                        increment(self.counts.queued_obligations, "queued obligation count")?;
                    ProcessOperatorStatusItem::QueuedObligation(decode_queued_obligation(&row)?)
                }
                ProcessOperatorStatusPhase::PullRequestConvergences => {
                    self.counts.pull_request_convergences = increment(
                        self.counts.pull_request_convergences,
                        "pull request convergence count",
                    )?;
                    ProcessOperatorStatusItem::PullRequestConvergence(decode_convergence(&row)?)
                }
                ProcessOperatorStatusPhase::PendingStaleReviewClearances => {
                    self.counts.pending_stale_review_clearances = increment(
                        self.counts.pending_stale_review_clearances,
                        "pending stale review clearance count",
                    )?;
                    ProcessOperatorStatusItem::PendingStaleReviewClearance(
                        decode_pending_clearance(&row)?,
                    )
                }
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
    // A branch-origin hold carries `workflow_branch` where a pull-request
    // origin carries `pull_request_number`; exactly one is non-null per row,
    // so both are read and the ordering states its null placement rather than
    // leaning on a server default.
    sqlx::query(
        "DECLARE operator_status_held_slots NO SCROLL CURSOR FOR
         SELECT dispatch_id, repository, pull_request_number, workflow_branch,
                rule_id, rule_version,
                singleton_scope, singleton_repository, singleton_pull_request_number,
                singleton_stack_root_pull_request_number,
                GREATEST(0, floor(extract(epoch FROM
                    (transaction_timestamp() - held_since))))::numeric
                    AS held_for_seconds,
                session_ids, blockers
           FROM repo_watch_held_dispatch_slot
          ORDER BY repository, pull_request_number ASC NULLS LAST,
                   workflow_branch ASC NULLS LAST, rule_id, dispatch_id",
    )
    .execute(&mut **transaction)
    .await?;
    // Readiness is the view's own decision, which excludes an occupying
    // dispatch, an external live session holding the target, a parked
    // obligation, and an exhausted retry budget. The transaction-clock cooldown
    // is conjoined rather than substituted: the view compares eligibility
    // against `clock_timestamp()` while the reported remaining cooldown is
    // measured from `transaction_timestamp()`, so a cooldown expiring mid-read
    // would otherwise emit a ready row alongside a positive remaining cooldown.
    // Narrowing keeps the snapshot self-consistent and never reports ready for
    // an obligation the dispatch loader would skip.
    sqlx::query(
        "DECLARE operator_status_queued_obligations NO SCROLL CURSOR FOR
         SELECT obligation_id, repository, rule_id, rule_version, singleton_scope,
                singleton_repository, singleton_pull_request_number,
                singleton_stack_root_pull_request_number, first_event_id,
                latest_event_id, matched_event_count,
                GREATEST(0, floor(extract(epoch FROM
                    (transaction_timestamp() - owed_since))))::numeric
                    AS waiting_for_seconds,
                occupying_dispatch_id, occupying_session_ids,
                CASE WHEN eligible_at = 'infinity'::timestamptz THEN NULL
                     WHEN eligible_at > transaction_timestamp()
                     THEN ceil(extract(epoch FROM
                         (eligible_at - transaction_timestamp())))::numeric
                     ELSE NULL
                END AS cooldown_remaining_seconds,
                COALESCE(eligible_at = 'infinity'::timestamptz, false)
                    AS cooldown_never_eligible,
                obligation.ready
                    AND (eligible_at IS NULL OR eligible_at <= transaction_timestamp())
                    AS ready
           FROM repo_watch_outstanding_dispatch_obligation AS obligation
          ORDER BY owed_since, obligation_id",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "DECLARE operator_status_pull_request_convergences NO SCROLL CURSOR FOR
         SELECT repository, pull_request_number, head_sha, base_branch,
                base_revision, mergeable_state, review_decision,
                unresolved_thread_count::numeric AS unresolved_thread_count,
                gating_check_count, non_green_gating_checks, verdict_kind,
                sealed_kind,
                GREATEST(0, floor(extract(epoch FROM
                    (transaction_timestamp() - recorded_at))))::numeric
                    AS assessed_seconds_ago
           FROM repo_watch_current_pull_request_convergence
          ORDER BY repository, pull_request_number",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "DECLARE operator_status_pending_stale_review_clearances NO SCROLL CURSOR FOR
         SELECT repository, pull_request_number, current_head_sha, review_node_id,
                reviewer, reviewed_head_sha,
                GREATEST(0, floor(extract(epoch FROM
                    (transaction_timestamp() - planned_at))))::numeric
                    AS pending_for_seconds
           FROM repo_watch_pending_stale_review_clearance
          ORDER BY planned_at, repository, pull_request_number, review_node_id",
    )
    .execute(&mut **transaction)
    .await?;
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

fn decode_held_slot(
    row: &PgRow,
) -> Result<ProcessOperatorStatusHeldSlot, ProcessOperatorStatusError> {
    let blockers = row
        .try_get::<Vec<String>, _>("blockers")?
        .into_iter()
        .map(|value| match value.as_str() {
            "undelivered_action" => Ok(ProcessOperatorStatusHeldSlotBlocker::UndeliveredAction),
            "delivery_turn_runtime_relevant" => {
                Ok(ProcessOperatorStatusHeldSlotBlocker::DeliveryTurnRuntimeRelevant)
            }
            "live_runtime_turn" => Ok(ProcessOperatorStatusHeldSlotBlocker::LiveRuntimeTurn),
            "pursuing_goal" => Ok(ProcessOperatorStatusHeldSlotBlocker::PursuingGoal),
            _ => Err(ProcessOperatorStatusCorruption::Unsupported {
                field: "held slot blocker",
                value,
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProcessOperatorStatusHeldSlot {
        dispatch_id: row.try_get("dispatch_id")?,
        repository: row.try_get("repository")?,
        origin: decode_held_slot_origin(row)?,
        rule_id: row.try_get("rule_id")?,
        rule_version: positive_i64(row.try_get("rule_version")?, "held rule version")?,
        singleton: decode_singleton(row)?,
        held_for_seconds: nonnegative_decimal(row.try_get("held_for_seconds")?, "held duration")?,
        session_ids: row.try_get("session_ids")?,
        blockers,
    })
}

/// Decodes the exclusive pull-request or branch origin of one held slot.
///
/// `repo_watch_event` admits a pull-request target with a number and no
/// workflow branch, or a branch workflow-run target with a branch and no
/// number; any other pairing contradicts that shape check.
fn decode_held_slot_origin(
    row: &PgRow,
) -> Result<ProcessOperatorStatusHeldSlotOrigin, ProcessOperatorStatusError> {
    let pull_request_number = row
        .try_get::<Option<Decimal>, _>("pull_request_number")?
        .map(|value| positive_decimal(value, "held pull request number"))
        .transpose()?;
    let workflow_branch = row.try_get::<Option<String>, _>("workflow_branch")?;
    match (pull_request_number, workflow_branch) {
        (Some(number), None) => Ok(ProcessOperatorStatusHeldSlotOrigin::PullRequest { number }),
        (None, Some(branch)) => Ok(ProcessOperatorStatusHeldSlotOrigin::Branch { branch }),
        _ => Err(ProcessOperatorStatusCorruption::Inconsistent("held dispatch origin").into()),
    }
}

fn decode_queued_obligation(
    row: &PgRow,
) -> Result<ProcessOperatorStatusQueuedObligation, ProcessOperatorStatusError> {
    Ok(ProcessOperatorStatusQueuedObligation {
        obligation_id: row.try_get("obligation_id")?,
        repository: row.try_get("repository")?,
        rule_id: row.try_get("rule_id")?,
        rule_version: positive_i64(row.try_get("rule_version")?, "queued rule version")?,
        singleton: decode_singleton(row)?,
        first_event_id: row.try_get("first_event_id")?,
        latest_event_id: row.try_get("latest_event_id")?,
        matched_event_count: positive_i64(
            row.try_get("matched_event_count")?,
            "matched event count",
        )?,
        waiting_for_seconds: nonnegative_decimal(
            row.try_get("waiting_for_seconds")?,
            "queued duration",
        )?,
        occupying_dispatch_id: row.try_get("occupying_dispatch_id")?,
        occupying_session_ids: row
            .try_get::<Option<Vec<Uuid>>, _>("occupying_session_ids")?
            .unwrap_or_default(),
        cooldown_remaining_seconds: row
            .try_get::<Option<Decimal>, _>("cooldown_remaining_seconds")?
            .map(|value| positive_decimal(value, "cooldown remaining"))
            .transpose()?,
        cooldown_never_eligible: row.try_get("cooldown_never_eligible")?,
        ready: row.try_get("ready")?,
    })
}

fn decode_convergence(
    row: &PgRow,
) -> Result<ProcessOperatorStatusPullRequestConvergence, ProcessOperatorStatusError> {
    let mergeable_state_value = row.try_get::<String, _>("mergeable_state")?;
    let mergeable_state = match repo_watch_mergeable_state_from_str(&mergeable_state_value).ok_or(
        ProcessOperatorStatusCorruption::Unsupported {
            field: "mergeable state",
            value: mergeable_state_value,
        },
    )? {
        MergeableState::Mergeable => ProcessOperatorStatusMergeableState::Mergeable,
        MergeableState::Conflicting => ProcessOperatorStatusMergeableState::Conflicting,
        MergeableState::Unknown => ProcessOperatorStatusMergeableState::Unknown,
    };
    let review_decision_value = row.try_get::<String, _>("review_decision")?;
    let review_decision = match repo_watch_review_decision_from_str(&review_decision_value).ok_or(
        ProcessOperatorStatusCorruption::Unsupported {
            field: "review decision",
            value: review_decision_value,
        },
    )? {
        RepoWatchReviewDecision::None => ProcessOperatorStatusReviewDecision::None,
        RepoWatchReviewDecision::Approved => ProcessOperatorStatusReviewDecision::Approved,
        RepoWatchReviewDecision::ReviewRequired => {
            ProcessOperatorStatusReviewDecision::ReviewRequired
        }
        RepoWatchReviewDecision::ChangesRequested => {
            ProcessOperatorStatusReviewDecision::ChangesRequested
        }
    };
    let verdict_value = row.try_get::<String, _>("verdict_kind")?;
    let verdict = match repo_watch_convergence_verdict_from_str(&verdict_value).ok_or(
        ProcessOperatorStatusCorruption::Unsupported {
            field: "convergence verdict",
            value: verdict_value,
        },
    )? {
        RepoWatchConvergenceVerdict::NotConverged => {
            ProcessOperatorStatusConvergenceVerdict::NotConverged
        }
        RepoWatchConvergenceVerdict::InternallyConverged => {
            ProcessOperatorStatusConvergenceVerdict::InternallyConverged
        }
        RepoWatchConvergenceVerdict::MergeReady => {
            ProcessOperatorStatusConvergenceVerdict::MergeReady
        }
    };
    let seal = row
        .try_get::<Option<String>, _>("sealed_kind")?
        .map(|value| {
            match repo_watch_convergence_verdict_from_str(&value).ok_or_else(|| {
                ProcessOperatorStatusCorruption::Unsupported {
                    field: "convergence seal",
                    value: value.clone(),
                }
            })? {
                RepoWatchConvergenceVerdict::InternallyConverged => {
                    Ok(ProcessOperatorStatusConvergenceSeal::InternallyConverged)
                }
                RepoWatchConvergenceVerdict::MergeReady => {
                    Ok(ProcessOperatorStatusConvergenceSeal::MergeReady)
                }
                RepoWatchConvergenceVerdict::NotConverged => {
                    Err(ProcessOperatorStatusCorruption::Unsupported {
                        field: "convergence seal",
                        value,
                    })
                }
            }
        })
        .transpose()?;
    Ok(ProcessOperatorStatusPullRequestConvergence {
        repository: row.try_get("repository")?,
        pull_request_number: positive_decimal(
            row.try_get("pull_request_number")?,
            "convergence pull request number",
        )?,
        head_sha: row.try_get("head_sha")?,
        base_branch: row.try_get("base_branch")?,
        base_revision: row.try_get("base_revision")?,
        mergeable_state,
        review_decision,
        unresolved_thread_count: nonnegative_decimal(
            row.try_get("unresolved_thread_count")?,
            "unresolved thread count",
        )?,
        gating_check_count: nonnegative_i64(
            row.try_get("gating_check_count")?,
            "gating check count",
        )?,
        non_green_gating_checks: row.try_get("non_green_gating_checks")?,
        verdict,
        seal,
        assessed_seconds_ago: nonnegative_decimal(
            row.try_get("assessed_seconds_ago")?,
            "assessment age",
        )?,
    })
}

fn decode_pending_clearance(
    row: &PgRow,
) -> Result<ProcessOperatorStatusPendingStaleReviewClearance, ProcessOperatorStatusError> {
    Ok(ProcessOperatorStatusPendingStaleReviewClearance {
        repository: row.try_get("repository")?,
        pull_request_number: positive_decimal(
            row.try_get("pull_request_number")?,
            "clearance pull request number",
        )?,
        current_head_sha: row.try_get("current_head_sha")?,
        review_node_id: row.try_get("review_node_id")?,
        reviewer: row.try_get("reviewer")?,
        reviewed_head_sha: row.try_get("reviewed_head_sha")?,
        pending_for_seconds: nonnegative_decimal(
            row.try_get("pending_for_seconds")?,
            "clearance duration",
        )?,
    })
}

fn decode_singleton(
    row: &PgRow,
) -> Result<ProcessOperatorStatusSingleton, ProcessOperatorStatusError> {
    let scope_value = row.try_get::<String, _>("singleton_scope")?;
    let scope = match repo_watch_singleton_scope_from_str(&scope_value).ok_or(
        ProcessOperatorStatusCorruption::Unsupported {
            field: "singleton scope",
            value: scope_value,
        },
    )? {
        RepoWatchSingletonScopeStorageKind::PullRequest => {
            ProcessOperatorStatusSingletonScope::PullRequest
        }
        RepoWatchSingletonScopeStorageKind::Stack => ProcessOperatorStatusSingletonScope::Stack,
        RepoWatchSingletonScopeStorageKind::Rule => ProcessOperatorStatusSingletonScope::Rule,
        RepoWatchSingletonScopeStorageKind::Repository => ProcessOperatorStatusSingletonScope::Repo,
    };
    Ok(ProcessOperatorStatusSingleton {
        scope,
        repository: row.try_get("singleton_repository")?,
        pull_request_number: row
            .try_get::<Option<Decimal>, _>("singleton_pull_request_number")?
            .map(|value| positive_decimal(value, "singleton pull request number"))
            .transpose()?,
        stack_root_pull_request_number: row
            .try_get::<Option<Decimal>, _>("singleton_stack_root_pull_request_number")?
            .map(|value| positive_decimal(value, "singleton stack root pull request number"))
            .transpose()?,
    })
}

fn increment(value: u64, field: &'static str) -> Result<u64, ProcessOperatorStatusCorruption> {
    value
        .checked_add(1)
        .ok_or(ProcessOperatorStatusCorruption::InvalidNumber(field))
}

fn nonnegative_decimal(
    value: Decimal,
    field: &'static str,
) -> Result<u64, ProcessOperatorStatusCorruption> {
    if value.is_sign_negative() || value.fract() != Decimal::ZERO {
        return Err(ProcessOperatorStatusCorruption::InvalidNumber(field));
    }
    u64::try_from(value).map_err(|_| ProcessOperatorStatusCorruption::InvalidNumber(field))
}

fn positive_decimal(
    value: Decimal,
    field: &'static str,
) -> Result<u64, ProcessOperatorStatusCorruption> {
    let value = nonnegative_decimal(value, field)?;
    if value == 0 {
        Err(ProcessOperatorStatusCorruption::InvalidNumber(field))
    } else {
        Ok(value)
    }
}

fn nonnegative_i64(
    value: i64,
    field: &'static str,
) -> Result<u64, ProcessOperatorStatusCorruption> {
    u64::try_from(value).map_err(|_| ProcessOperatorStatusCorruption::InvalidNumber(field))
}

fn positive_i64(value: i64, field: &'static str) -> Result<u64, ProcessOperatorStatusCorruption> {
    let value = nonnegative_i64(value, field)?;
    if value == 0 {
        Err(ProcessOperatorStatusCorruption::InvalidNumber(field))
    } else {
        Ok(value)
    }
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
