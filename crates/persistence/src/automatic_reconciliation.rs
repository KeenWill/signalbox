//! Durable daemon-owned reconciliation of ambiguous physical operations.

use std::{error::Error, fmt, future::Future, num::NonZeroU32, time::Duration};

use signalbox_application::{
    AutomaticReconciliationAttempt, AutomaticReconciliationBatch,
    AutomaticReconciliationFailureKind, AutomaticReconciliationOperation,
    AutomaticReconciliationOutcome, ClaimedAutomaticReconciliation, ClassifyOperatorFailure,
    ExhaustedAutomaticReconciliation, OperatorFailureClass,
};
use signalbox_domain::{
    AmbiguousModelCallTurnIdentities, ContextFrontierId, PendingSteeringReclassificationIdentity,
    ReconstitutedToolAttempt, SemanticTranscriptEntryId, TurnId,
};
use sqlx::{
    PgConnection, PgPool, Postgres, Row, Transaction,
    pool::{MaybePoolConnection, PoolConnection},
};
use tokio::time::timeout;

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        session_id_from_uuid, session_id_to_uuid, tool_attempt_id_from_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    },
    model_execution::{
        ModelCallRepositoryError, load_delegated_model_call_recovery,
        lock_delegated_child_endpoint_sessions, persist_automatic_reconciliation,
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
// numeric-bound: guard - prevents a durable claim deadline from expiring before work starts
const CLAIM_WINDOW: i64 = 1;

/// Admitted attempts the claim statement schedules, one `CASE` arm each.
///
/// Published because it is the ceiling a deployment's attempt budget has to
/// respect. The claim statement's `CASE` ends in an `ELSE` arm, so a budget
/// above this arity does not fail — it silently reuses the last rung's deadline
/// for every attempt past it, while the failure path keeps computing the true
/// exponential. The two paths would then disagree about when an in-flight
/// attempt counts as abandoned, and the claim side is the shorter one, so the
/// sweep would settle attempts that are still running. Configuration admission
/// refuses such a budget rather than letting the arity be discovered in
/// production.
// numeric-bound: not-a-bound - the claim statement's fixed CASE arity, which the ladder and the configured budget must match
pub const RETRY_LADDER_ARITY: usize = 5;

fn retry_ladder_arity_u32() -> u32 {
    u32::try_from(RETRY_LADDER_ARITY).unwrap_or(u32::MAX)
}

/// How long any one reconciliation statement waits for a contended row.
///
/// Every transaction here takes inventoried row locks unqualified: none skips a
/// locked row and none refuses to wait. A client-side deadline bounds only the
/// daemon's patience, because dropping a future queues a `ROLLBACK` rather than
/// sending a `CancelRequest`: the backend keeps waiting and the pooled
/// connection stays checked out for the full real wait while the caller has
/// already given up. Under live traffic that turns contention into connection
/// exhaustion.
///
/// `lock_timeout` bounds the database work itself and raises `55P03`, which this
/// repository records as an ordinary infrastructure failure against the attempt
/// budget. It is installed before anything is read or written, so it can only
/// interrupt a lock wait, never a commit.
///
/// It is published because what makes it correct is its relationship to the
/// caller's deadline: the caller must let this budget expire first, which
/// [`reconciliation_deadline`] enforces.
// numeric-bound: guard - prevents a contended statement from holding a pooled connection for the whole real wait
pub const RECONCILIATION_LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long one reconciliation transaction waits to reach a pooled connection.
///
/// Cancelling an acquisition is safe in a way cancelling a statement is not: no
/// transaction has begun, nothing has been sent, and so there is no work whose
/// fate could be unknown.
// numeric-bound: guard - prevents a saturated pool from consuming the attempt's deadline before it begins
pub const RECONCILIATION_ACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// Headroom the caller's deadline keeps above the summed database-side budgets.
///
/// The deadline covers more than those budgets. It starts before the pool is
/// asked for a connection and so also spans `BEGIN`, the `lock_timeout`
/// statement that installs the lock budget, and the scheduling latency between
/// them — the one stretch no database-side budget covers, because the budget is
/// what the end of it installs. A floor equal to the summed budgets leaves that
/// stretch outside the deadline, so the deadline could expire in the same
/// instant PostgreSQL was about to report `55P03`, which is the strand these
/// bounds exist to prevent rather than a smaller version of it.
// numeric-bound: guard - keeps the caller deadline above the database-side budgets by more than the uncovered BEGIN stretch
pub const RECONCILIATION_DEADLINE_MARGIN: Duration = Duration::from_millis(500);

/// The smallest caller deadline that lets the database-side budgets expire first.
///
/// Derived from the budgets it must outlast plus
/// [`RECONCILIATION_DEADLINE_MARGIN`], never written as their sum: a budget
/// raised without raising this floor would otherwise close the margin silently.
/// A deployment-configured bound is raised to this floor.
// numeric-bound: derived guard from RECONCILIATION_DEADLINE_MARGIN
pub const RECONCILIATION_DEADLINE_FLOOR: Duration = RECONCILIATION_ACQUIRE_WAIT
    .saturating_add(RECONCILIATION_LOCK_WAIT)
    .saturating_add(RECONCILIATION_DEADLINE_MARGIN);

/// The floor must strictly exceed the budgets it exists to outlast, as an
/// arithmetic fact rather than a comment: an equal floor is the stranding case.
const _: () = assert!(
    RECONCILIATION_DEADLINE_FLOOR.as_millis()
        > RECONCILIATION_ACQUIRE_WAIT
            .saturating_add(RECONCILIATION_LOCK_WAIT)
            .as_millis(),
    "the reconciliation deadline floor must outlast the summed database-side budgets"
);

/// The last-resort deadline for one reconciliation transaction.
///
/// This is the only bound the deployment configures. It sits above the
/// database-side budgets as the last resort for a backend that has stopped
/// answering at all, and it never bounds the uncancellable `BEGIN` stretch: a
/// caller that gives up abandons its own wait while the opened transaction
/// completes and rolls back on a connection nobody is racing.
///
/// An unconfigured deployment keeps the shipped default rather than running
/// unbounded, and a configured bound below [`RECONCILIATION_DEADLINE_FLOOR`] is
/// raised to it.
// numeric-bound: guard - prevents an unconfigured deployment from waiting forever on a backend that stopped answering
pub const RECONCILIATION_DEADLINE_DEFAULT: Duration = Duration::from_secs(5);

/// The margin must hold as an arithmetic fact, not as a comment: a
/// database-side budget raised to meet the shipped deadline would silently
/// restore the strand these bounds exist to prevent.
const _: () = assert!(
    RECONCILIATION_DEADLINE_DEFAULT.as_millis() > RECONCILIATION_DEADLINE_FLOOR.as_millis(),
    "the shipped reconciliation deadline must outlast its database-side budgets"
);

/// Resolves the deployment's configured attempt bound into the enforced deadline.
#[must_use]
pub fn reconciliation_deadline(configured: Option<Duration>) -> Duration {
    configured
        .unwrap_or(RECONCILIATION_DEADLINE_DEFAULT)
        .max(RECONCILIATION_DEADLINE_FLOOR)
}

/// Reaches a pooled connection under [`RECONCILIATION_ACQUIRE_WAIT`].
///
/// The budget covers the acquisition alone. `Pool::begin` would put `BEGIN`
/// inside it, and cancelling that is not a smaller failure but the exact one
/// this module exists to prevent.
async fn acquire_bounded(
    pool: &PgPool,
) -> Result<PoolConnection<Postgres>, AutomaticReconciliationRepositoryError> {
    timeout(RECONCILIATION_ACQUIRE_WAIT, pool.acquire())
        .await
        .unwrap_or(Err(sqlx::Error::PoolTimedOut))
        .map_err(AutomaticReconciliationRepositoryError::from)
}

/// Drives `work` where a caller that gives up cannot cancel it.
///
/// Dropping a future is the only way an `async` caller abandons work. Driving
/// the operation on its own task separates the two: the caller's deadline
/// abandons the join handle, while the task keeps running and finishes what it
/// sent.
async fn uncancellable<T>(
    work: impl Future<Output = Result<T, AutomaticReconciliationRepositoryError>> + Send + 'static,
) -> Result<T, AutomaticReconciliationRepositoryError>
where
    T: Send + 'static,
{
    tokio::spawn(work)
        .await
        .unwrap_or_else(|_| Err(sqlx::Error::WorkerCrashed.into()))
}

/// Opens the transaction and installs its budget, both beyond cancellation.
///
/// `BEGIN` and the `lock_timeout` statement are the one stretch no database-side
/// budget covers, because the budget is what the second of them installs.
/// Running the stretch on its own task makes it uncancellable rather than merely
/// unbounded, so from the returned transaction onward
/// [`RECONCILIATION_LOCK_WAIT`] is in force and the caller's deadline is what the
/// specification says it is: a last resort sitting above a database-side budget
/// that expires first. `COMMIT` is never interrupted.
async fn begin_budgeted(
    pool: &PgPool,
) -> Result<Transaction<'static, Postgres>, AutomaticReconciliationRepositoryError> {
    let connection = acquire_bounded(pool).await?;
    uncancellable(async move {
        let mut transaction =
            Transaction::begin(MaybePoolConnection::PoolConnection(connection), None).await?;
        bound_reconciliation_lock_wait(&mut transaction).await?;
        Ok(transaction)
    })
    .await
}

/// Commits beyond the caller's deadline.
///
/// Everything before the commit is safe for a caller to abandon: `lock_timeout`
/// bounds it, and dropping it rolls the transaction back leaving nothing durable
/// in doubt. The commit is the one statement that is not. Abandoning it drops
/// the driver mid-`COMMIT`, so the attempt's fate becomes unknown to the daemon
/// exactly as if the backend had failed — the ambiguity this module exists to
/// resolve, reintroduced by the bound meant to contain it.
///
/// Driving it through [`uncancellable`] means an expiring deadline abandons only
/// the join handle while the commit reaches its server-enforced outcome. That is
/// what makes the caller's deadline the last resort the specification describes
/// instead of an interruption of the one statement no bound above it may cut.
async fn commit_uncancellable(
    transaction: Transaction<'static, Postgres>,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    uncancellable(async move { transaction.commit().await.map_err(commit_error) }).await
}

/// Records whether a failed commit left the transaction's fate unknown.
fn commit_error(source: sqlx::Error) -> AutomaticReconciliationRepositoryError {
    AutomaticReconciliationRepositoryError::Database {
        commit_ambiguous: commit_failure_is_ambiguous(&source),
        source,
    }
}

/// Applies [`RECONCILIATION_LOCK_WAIT`] to the transaction on `connection`.
///
/// Called before the transaction reads or writes anything, so the only statement
/// the budget can interrupt is one waiting for a row. A bound that could fire
/// later might interrupt the commit instead, and the caller would then not know
/// whether the attempt had ended.
async fn bound_reconciliation_lock_wait(
    connection: &mut PgConnection,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!("{}ms", RECONCILIATION_LOCK_WAIT.as_millis()))
        .execute(connection)
        .await?;
    Ok(())
}

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
#[derive(Clone, Copy, Debug)]
struct AutomaticReconciliationClassPolicy {
    attempt_budget: u32,
    retry_backoff_base: Option<Duration>,
    retry_backoff_cap: Option<Duration>,
    attempt_lease: Duration,
}

#[derive(Clone, Debug)]
pub struct PostgresAutomaticReconciliationRepository {
    pool: PgPool,
    model_call_policy: AutomaticReconciliationClassPolicy,
    tool_policy: AutomaticReconciliationClassPolicy,
}

impl PostgresAutomaticReconciliationRepository {
    /// Uses the shared daemon pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            model_call_policy: AutomaticReconciliationClassPolicy {
                attempt_budget: retry_ladder_arity_u32(),
                retry_backoff_base: None,
                retry_backoff_cap: None,
                attempt_lease: RECONCILIATION_DEADLINE_DEFAULT,
            },
            tool_policy: AutomaticReconciliationClassPolicy {
                attempt_budget: retry_ladder_arity_u32(),
                retry_backoff_base: None,
                retry_backoff_cap: None,
                attempt_lease: RECONCILIATION_DEADLINE_DEFAULT,
            },
        }
    }

    /// Applies the deployment's attempt and retry-timing policies.
    pub fn with_policy(
        mut self,
        attempt_budget: Option<u32>,
        retry_backoff_base: Option<Duration>,
        retry_backoff_cap: Option<Duration>,
    ) -> Self {
        let attempt_budget = match attempt_budget {
            Some(budget) => budget,
            None => retry_ladder_arity_u32(),
        };
        self.model_call_policy = AutomaticReconciliationClassPolicy {
            attempt_budget,
            retry_backoff_base,
            retry_backoff_cap,
            attempt_lease: self.model_call_policy.attempt_lease,
        };
        self.tool_policy = AutomaticReconciliationClassPolicy {
            attempt_budget,
            retry_backoff_base,
            retry_backoff_cap,
            attempt_lease: self.tool_policy.attempt_lease,
        };
        self
    }

    /// Applies independent model-call and tool recovery policies.
    pub const fn with_class_policies(
        mut self,
        model_call_attempt_budget: u32,
        model_call_retry_backoff_base: Option<Duration>,
        model_call_retry_backoff_cap: Option<Duration>,
        tool_attempt_budget: u32,
        tool_retry_backoff_base: Option<Duration>,
        tool_retry_backoff_cap: Option<Duration>,
    ) -> Self {
        self.model_call_policy = AutomaticReconciliationClassPolicy {
            attempt_budget: model_call_attempt_budget,
            retry_backoff_base: model_call_retry_backoff_base,
            retry_backoff_cap: model_call_retry_backoff_cap,
            attempt_lease: self.model_call_policy.attempt_lease,
        };
        self.tool_policy = AutomaticReconciliationClassPolicy {
            attempt_budget: tool_attempt_budget,
            retry_backoff_base: tool_retry_backoff_base,
            retry_backoff_cap: tool_retry_backoff_cap,
            attempt_lease: self.tool_policy.attempt_lease,
        };
        self
    }

    /// Keeps an in-flight claim live for at least its caller's class bound.
    pub const fn with_class_attempt_leases(
        mut self,
        model_call_attempt_lease: Duration,
        tool_attempt_lease: Duration,
    ) -> Self {
        self.model_call_policy.attempt_lease = model_call_attempt_lease;
        self.tool_policy.attempt_lease = tool_attempt_lease;
        self
    }

    const fn policy(
        &self,
        operation: AutomaticReconciliationOperation,
    ) -> AutomaticReconciliationClassPolicy {
        match operation {
            AutomaticReconciliationOperation::ModelCall(_) => self.model_call_policy,
            AutomaticReconciliationOperation::ToolAttempt(_) => self.tool_policy,
        }
    }

    /// Discovers exact ambiguity waits and claims one due window under the
    /// module's layered database-side budgets.
    pub async fn claim_due(
        &self,
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        self.claim_due_for_class(None).await
    }

    /// Claims one due model-call reconciliation without consuming a tool slot.
    pub async fn claim_due_model_call(
        &self,
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        self.claim_due_for_class(Some(true)).await
    }

    /// Claims one due tool reconciliation without consuming a model-call slot.
    pub async fn claim_due_tool_attempt(
        &self,
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        self.claim_due_for_class(Some(false)).await
    }

    /// Reports whether a due model-call recovery remains after one bounded discovery.
    pub async fn has_due_model_call(&self) -> Result<bool, AutomaticReconciliationRepositoryError> {
        self.has_due_for_class(true).await
    }

    /// Reports whether a due tool recovery remains after one bounded discovery.
    pub async fn has_due_tool_attempt(
        &self,
    ) -> Result<bool, AutomaticReconciliationRepositoryError> {
        self.has_due_for_class(false).await
    }

    async fn has_due_for_class(
        &self,
        model_call: bool,
    ) -> Result<bool, AutomaticReconciliationRepositoryError> {
        let mut transaction = begin_budgeted(&self.pool).await?;
        discover_recoveries(
            &mut transaction,
            CLAIM_WINDOW,
            i32::try_from(self.model_call_policy.attempt_budget).map_err(|_| {
                AutomaticReconciliationRepositoryError::Corruption("attempt budget")
            })?,
            i32::try_from(self.tool_policy.attempt_budget).map_err(|_| {
                AutomaticReconciliationRepositoryError::Corruption("attempt budget")
            })?,
            Some(model_call),
        )
        .await?;
        let due = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM automatic_reconciliation
                 WHERE state_kind = 'scheduled'
                   AND attempt_count < attempt_ceiling
                   AND next_attempt_at <= statement_timestamp()
                   AND (($1 AND model_call_id IS NOT NULL)
                        OR (NOT $1 AND tool_attempt_id IS NOT NULL))
            )",
        )
        .bind(model_call)
        .fetch_one(&mut *transaction)
        .await?;
        commit_uncancellable(transaction).await?;
        Ok(due)
    }

    async fn claim_due_for_class(
        &self,
        model_call: Option<bool>,
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        let mut transaction = begin_budgeted(&self.pool).await?;
        let model_call_attempt_budget = i32::try_from(self.model_call_policy.attempt_budget)
            .map_err(|_| AutomaticReconciliationRepositoryError::Corruption("attempt budget"))?;
        let tool_attempt_budget = i32::try_from(self.tool_policy.attempt_budget)
            .map_err(|_| AutomaticReconciliationRepositoryError::Corruption("attempt budget"))?;
        discover_recoveries(
            &mut transaction,
            CLAIM_WINDOW,
            model_call_attempt_budget,
            tool_attempt_budget,
            model_call,
        )
        .await?;
        settle_abandoned_attempts(&mut transaction, CLAIM_WINDOW).await?;
        mark_superseded_recoveries(&mut transaction, CLAIM_WINDOW).await?;
        let exhausted_rows = mark_exhausted_recoveries(&mut transaction, CLAIM_WINDOW).await?;
        let mut claim =
            sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLAIM).bind(CLAIM_WINDOW);
        for millis in retry_ladder_millis(
            self.model_call_policy.retry_backoff_base,
            self.model_call_policy.retry_backoff_cap,
        )? {
            claim = claim.bind(millis);
        }
        for millis in retry_ladder_millis(
            self.tool_policy.retry_backoff_base,
            self.tool_policy.retry_backoff_cap,
        )? {
            claim = claim.bind(millis);
        }
        claim = claim.bind(model_call);
        claim = claim.bind(duration_millis(
            self.model_call_policy.attempt_lease,
            "attempt lease",
        )?);
        claim = claim.bind(duration_millis(
            self.tool_policy.attempt_lease,
            "attempt lease",
        )?);
        let rows = claim.fetch_all(&mut *transaction).await?;
        commit_uncancellable(transaction).await?;

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
                u32::try_from(row.try_get::<i32, _>("attempt_ceiling")?)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                        "attempt ceiling",
                    ))?,
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
    ) -> Result<AutomaticReconciliationOutcome, AutomaticReconciliationRepositoryError> {
        let mut transaction = begin_budgeted(&self.pool).await?;
        lock_delegated_child_endpoint_sessions(&mut transaction, claimed.session())
            .await
            .map_err(AutomaticReconciliationRepositoryError::Model)?;
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
        let origin: Option<String> = sqlx::query_scalar(
            "SELECT origin_kind
               FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'active'
                AND active_phase_kind = $3
                AND recovery_model_call_id IS NOT DISTINCT FROM $4
                AND recovery_tool_attempt_id IS NOT DISTINCT FROM $5",
        )
        .bind(session_id_to_uuid(claimed.session()))
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(phase)
        .bind(model_call)
        .bind(tool_attempt)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(origin) = origin else {
            finish_superseded(&mut transaction, claimed).await?;
            commit_uncancellable(transaction).await?;
            return Ok(AutomaticReconciliationOutcome::Superseded);
        };
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
        let terminal_frontier = ContextFrontierId::from_uuid(uuid::Uuid::now_v7());
        let Some(attempt) = std::num::NonZeroU32::new(claimed.attempt().get()) else {
            return Err(AutomaticReconciliationRepositoryError::Corruption(
                "zero attempt ordinal",
            ));
        };
        match claimed.operation() {
            AutomaticReconciliationOperation::ModelCall(claimed_call) => {
                let reconciliation = match origin.as_str() {
                    "accepted_input" => {
                        let active = scheduling
                            .active_turn_execution()
                            .filter(|turn| turn.turn() == claimed.turn())
                            .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                                "accepted-input active turn for exact wait",
                            ))?;
                        let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
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
                        scheduling.apply_automatic_reconciliation(attempt, identities)
                    }
                    "delegation" => {
                        let recovery = load_delegated_model_call_recovery(
                            &mut transaction,
                            claimed.session(),
                            &scheduling,
                        )
                        .await
                        .map_err(AutomaticReconciliationRepositoryError::Model)?
                        .ok_or(
                            AutomaticReconciliationRepositoryError::Corruption(
                                "delegated active turn for exact wait",
                            ),
                        )?;
                        let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
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
                        return Err(AutomaticReconciliationRepositoryError::Corruption(
                            "origin for exact model-call wait",
                        ));
                    }
                };
                let reconciliation = match reconciliation {
                    Ok(reconciliation) if reconciliation.call().id() == claimed_call => {
                        reconciliation
                    }
                    Ok(_) | Err(_) => {
                        return Err(AutomaticReconciliationRepositoryError::Corruption(
                            "model-call aggregate transition for exact wait",
                        ));
                    }
                };
                persist_automatic_reconciliation(&mut transaction, &reconciliation)
                    .await
                    .map_err(AutomaticReconciliationRepositoryError::Model)?;
            }
            AutomaticReconciliationOperation::ToolAttempt(claimed_attempt) => {
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
                    })
                    .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                        "active tool turn for exact wait",
                    ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
                    .with_pending_steering_reclassifications(pending);
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
        commit_uncancellable(transaction).await?;
        Ok(AutomaticReconciliationOutcome::Reconciled)
    }

    /// Durably classifies an attempt whose authoritative transaction failed.
    pub async fn record_failure(
        &self,
        claimed: ClaimedAutomaticReconciliation,
        failure: AutomaticReconciliationFailureKind,
    ) -> Result<(), AutomaticReconciliationRepositoryError> {
        let mut transaction = begin_budgeted(&self.pool).await?;
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
        let policy = self.policy(claimed.operation());
        let retry_delay_millis = policy
            .retry_backoff_base
            .map(|base| {
                claimed
                    .attempt()
                    .retry_backoff(base, policy.retry_backoff_cap)
            })
            .map(|delay| i64::try_from(delay.as_millis()))
            .transpose()
            .map_err(|_| AutomaticReconciliationRepositoryError::Corruption("retry backoff"))?;
        let recovery_rows = sqlx::query(
            "UPDATE automatic_reconciliation
                SET state_kind = 'scheduled',
                    next_attempt_at = CASE
                        WHEN $3::bigint IS NULL THEN 'infinity'::timestamptz
                        ELSE statement_timestamp() + $3 * INTERVAL '1 millisecond'
                    END
              WHERE turn_id = $1 AND attempt_count = $2
                AND state_kind = 'attempting'",
        )
        .bind(turn_id_to_uuid(claimed.turn()))
        .bind(i64::from(claimed.attempt().get()))
        .bind(retry_delay_millis)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if rows != 1 || recovery_rows != 1 {
            return Err(AutomaticReconciliationRepositoryError::Corruption(
                "attempt failure cardinality",
            ));
        }
        commit_uncancellable(transaction).await?;
        Ok(())
    }
}

/// Renders one retry policy as the claim statement's ladder, in milliseconds.
///
/// Free of the repository because the ladder is a function of configuration
/// alone: no connection, no pool, and nothing about the deployment's database
/// participates in the schedule.
fn retry_ladder_millis(
    base: Option<Duration>,
    cap: Option<Duration>,
) -> Result<[Option<i64>; RETRY_LADDER_ARITY], AutomaticReconciliationRepositoryError> {
    let mut ladder = [None; RETRY_LADDER_ARITY];
    let Some(base) = base else {
        return Ok(ladder);
    };
    for (index, slot) in ladder.iter_mut().enumerate() {
        let ordinal = u32::try_from(index)
            .map_err(|_| {
                AutomaticReconciliationRepositoryError::Corruption("retry ladder ordinal")
            })?
            .saturating_add(1);
        let attempt = AutomaticReconciliationAttempt::try_from_u32(ordinal).ok_or(
            AutomaticReconciliationRepositoryError::Corruption("retry ladder ordinal"),
        )?;
        let millis = i64::try_from(attempt.retry_backoff(base, cap).as_millis())
            .map_err(|_| AutomaticReconciliationRepositoryError::Corruption("retry backoff"))?;
        *slot = Some(millis);
    }
    Ok(ladder)
}

fn duration_millis(
    duration: Duration,
    corruption: &'static str,
) -> Result<i64, AutomaticReconciliationRepositoryError> {
    i64::try_from(duration.as_millis())
        .map_err(|_| AutomaticReconciliationRepositoryError::Corruption(corruption))
}

async fn discover_recoveries(
    connection: &mut PgConnection,
    window: i64,
    model_call_attempt_budget: i32,
    tool_attempt_budget: i32,
    model_call: Option<bool>,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    let query = match model_call {
        Some(model_call) => {
            sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLASS_DISCOVERY)
                .bind(window)
                .bind(model_call_attempt_budget)
                .bind(tool_attempt_budget)
                .bind(model_call)
        }
        None => sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_DISCOVERY)
            .bind(window)
            .bind(model_call_attempt_budget)
            .bind(tool_attempt_budget),
    };
    query.execute(connection).await?;
    Ok(())
}

async fn settle_abandoned_attempts(
    connection: &mut PgConnection,
    window: i64,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(
        "WITH abandoned AS MATERIALIZED (
            SELECT turn_id, attempt_count
              FROM automatic_reconciliation
             WHERE state_kind = 'attempting'
               AND next_attempt_at <= statement_timestamp()
             ORDER BY next_attempt_at, turn_id
             LIMIT $1
         ), attempts AS (
            UPDATE automatic_reconciliation_attempt AS attempt
               SET outcome_kind = 'infrastructure_failure',
                   finished_at = statement_timestamp()
              FROM abandoned
             WHERE attempt.turn_id = abandoned.turn_id
               AND attempt.attempt_ordinal = abandoned.attempt_count
               AND attempt.outcome_kind = 'attempting'
         )
         UPDATE automatic_reconciliation AS recovery
            SET state_kind = 'scheduled'
           FROM abandoned
          WHERE recovery.turn_id = abandoned.turn_id
            AND recovery.state_kind = 'attempting'
            AND recovery.attempt_count = abandoned.attempt_count
            AND recovery.attempt_count < recovery.attempt_ceiling",
    )
    .bind(window)
    .execute(connection)
    .await?;
    Ok(())
}

async fn mark_superseded_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_SUPERSESSION)
        .bind(window)
        .execute(connection)
        .await?;
    Ok(())
}

/// Exhausts one bounded window of recoveries that have spent their budget.
///
/// Paged the way `settle_abandoned_attempts` and the claim statement page. A
/// mass-exhaustion event — one provider outage ending many ambiguous calls at
/// the same attempt — would otherwise update and materialize the entire backlog
/// inside a single transaction, outlast the caller's deadline, and leave a large
/// transaction running on a connection nobody is waiting for any more. The
/// specification promises bounded exhaustion work, and the window is what makes
/// it bounded; successive scans drain the rest.
///
/// The budget test is `>=`, the exact complement of the claim statement's
/// `attempt_count < attempt_ceiling`. Persisting the ceiling on discovery keeps
/// both predicates on one policy even if deployment configuration changes while
/// the recovery is pending.
async fn mark_exhausted_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, AutomaticReconciliationRepositoryError> {
    let rows = sqlx::query(
        "WITH spent AS MATERIALIZED (
            SELECT turn_id
              FROM automatic_reconciliation
             WHERE state_kind IN ('scheduled', 'attempting')
               AND attempt_count >= attempt_ceiling
               AND (
                   state_kind = 'scheduled'
                   OR next_attempt_at <= statement_timestamp()
               )
             ORDER BY next_attempt_at, turn_id
             LIMIT $1
         )
         UPDATE automatic_reconciliation AS recovery
            SET state_kind = 'exhausted', exhausted_at = statement_timestamp()
           FROM spent
          WHERE recovery.turn_id = spent.turn_id
            AND recovery.state_kind IN ('scheduled', 'attempting')
            AND recovery.attempt_count >= recovery.attempt_ceiling
      RETURNING recovery.session_id, recovery.turn_id,
                recovery.model_call_id, recovery.tool_attempt_id,
                recovery.attempt_ceiling",
    )
    .bind(window)
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
    use super::{
        AutomaticReconciliationAttempt, RECONCILIATION_ACQUIRE_WAIT,
        RECONCILIATION_DEADLINE_DEFAULT, RECONCILIATION_DEADLINE_FLOOR,
        RECONCILIATION_DEADLINE_MARGIN, RECONCILIATION_LOCK_WAIT, RETRY_LADDER_ARITY,
        reconciliation_deadline, retry_ladder_millis,
    };
    use std::time::Duration;

    /// The floor has to outlast the database-side budgets *strictly*. Equal is
    /// the stranding case the layered bounds exist to close: the deadline would
    /// start before the pool is asked for a connection and so also cover `BEGIN`
    /// and the `lock_timeout` statement, leaving it able to expire in the same
    /// instant PostgreSQL was about to report `55P03`.
    #[test]
    fn the_deadline_floor_outlasts_both_database_side_budgets() {
        let budgets = RECONCILIATION_ACQUIRE_WAIT + RECONCILIATION_LOCK_WAIT;
        assert!(
            RECONCILIATION_DEADLINE_FLOOR > budgets,
            "floor {RECONCILIATION_DEADLINE_FLOOR:?} must outlast budgets {budgets:?}"
        );
        assert_eq!(
            RECONCILIATION_DEADLINE_FLOOR - budgets,
            RECONCILIATION_DEADLINE_MARGIN
        );
        assert!(!RECONCILIATION_DEADLINE_MARGIN.is_zero());
    }

    /// The ladder carries milliseconds, not truncated seconds. A sub-second
    /// configured backoff is the case whole seconds destroyed: every rung
    /// collapsed to a zero claim-side deadline, which the abandonment sweep
    /// reads as "already due" and settles an attempt that is still running.
    #[test]
    fn a_sub_second_backoff_survives_the_claim_ladder() {
        let ladder = retry_ladder_millis(Some(Duration::from_millis(500)), None)
            .expect("a representable sub-second ladder");
        assert_eq!(
            ladder,
            [
                Some(500),
                Some(1_000),
                Some(2_000),
                Some(4_000),
                Some(8_000)
            ]
        );
        assert!(
            ladder.iter().all(|rung| rung != &Some(0)),
            "no rung may collapse to an immediate deadline: {ladder:?}"
        );
    }

    /// The claim ladder and the failure path have to schedule one policy, so
    /// each rung must equal what `record_failure` would compute for the same
    /// attempt. This is the divergence the unit mismatch introduced.
    #[test]
    fn every_ladder_rung_matches_the_failure_paths_schedule() {
        let base = Duration::from_millis(750);
        let cap = Some(Duration::from_secs(4));
        let ladder = retry_ladder_millis(Some(base), cap).expect("a representable ladder");
        for (index, rung) in ladder.iter().enumerate() {
            let attempt = AutomaticReconciliationAttempt::try_from_u32(
                u32::try_from(index).expect("a ladder index fits a u32") + 1,
            )
            .expect("a one-based ladder ordinal");
            let expected = i64::try_from(attempt.retry_backoff(base, cap).as_millis())
                .expect("a representable backoff");
            assert_eq!(rung, &Some(expected), "rung {index} diverged");
        }
    }

    /// An unconfigured base backoff still yields the all-`NULL` ladder the claim
    /// statement reads as "no claimable deadline".
    #[test]
    fn an_unconfigured_backoff_yields_no_claimable_deadline() {
        let ladder = retry_ladder_millis(None, None).expect("an unconfigured ladder");
        assert_eq!(ladder, [None; RETRY_LADDER_ARITY]);
    }

    /// The published arity has to be the arity the claim statement actually
    /// carries, because configuration admission refuses budgets above it. If the
    /// two drifted, the daemon would reject admissible budgets or admit ones the
    /// `ELSE` arm silently flattens.
    #[test]
    fn the_published_arity_covers_each_class_claim_ladder() {
        let claim = crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLAIM;
        assert_eq!(RETRY_LADDER_ARITY, 5);
        assert!(claim.contains("WHEN 1 THEN $2::bigint"));
        assert!(claim.contains("WHEN 4 THEN $5::bigint"));
        assert!(claim.contains("ELSE $6::bigint"));
        assert!(claim.contains("WHEN 1 THEN $7::bigint"));
        assert!(claim.contains("WHEN 4 THEN $10::bigint"));
        assert!(claim.contains("ELSE $11::bigint"));
        assert!(claim.contains("$12::boolean"));
        assert!(claim.contains("$13::bigint"));
        assert!(claim.contains("$14::bigint"));
        // The schedule is expressed in the same unit the failure path uses.
        assert!(claim.contains("interval '1 millisecond'"));
        assert!(!claim.contains("interval '1 second'"));
    }

    /// Claims read the ceiling frozen onto each recovery at discovery time.
    /// Configuration changes therefore cannot move an already-pending recovery
    /// between the claim and exhaustion predicates.
    #[test]
    fn the_claim_uses_the_persisted_attempt_ceiling() {
        assert!(
            crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLAIM
                .contains("attempt_count < attempt_ceiling"),
            "the claim statement must read the recovery's persisted ceiling"
        );
    }

    /// A deployment cannot configure a deadline that would expire inside the
    /// uncancellable `BEGIN` stretch: it is raised to the floor instead.
    #[test]
    fn a_configured_bound_below_the_begin_budget_is_raised_to_the_floor() {
        let undercutting = Duration::from_millis(1);
        assert!(undercutting < RECONCILIATION_ACQUIRE_WAIT);
        assert_eq!(
            reconciliation_deadline(Some(undercutting)),
            RECONCILIATION_DEADLINE_FLOOR
        );
        assert_eq!(
            reconciliation_deadline(Some(RECONCILIATION_ACQUIRE_WAIT)),
            RECONCILIATION_DEADLINE_FLOOR
        );
    }

    /// A configured deadline above the floor is honoured exactly.
    #[test]
    fn a_configured_bound_above_the_floor_is_honoured() {
        let configured = Duration::from_secs(30);
        assert_eq!(reconciliation_deadline(Some(configured)), configured);
    }

    /// An unconfigured deployment keeps the shipped default rather than
    /// running the attempt unbounded.
    #[test]
    fn an_unconfigured_deployment_keeps_the_shipped_default() {
        assert_eq!(
            reconciliation_deadline(None),
            RECONCILIATION_DEADLINE_DEFAULT
        );
        assert!(RECONCILIATION_DEADLINE_DEFAULT > RECONCILIATION_DEADLINE_FLOOR);
    }
}
