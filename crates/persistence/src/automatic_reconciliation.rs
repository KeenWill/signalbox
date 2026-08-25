//! Durable daemon-owned reconciliation of ambiguous physical operations.

use std::{error::Error, fmt, future::Future, time::Duration};

use signalbox_application::{
    AutomaticReconciliationAttempt, AutomaticReconciliationBatch,
    AutomaticReconciliationFailureKind, AutomaticReconciliationOperation,
    AutomaticReconciliationOutcome, ClaimedAutomaticReconciliation, ClassifyOperatorFailure,
    ExhaustedAutomaticReconciliation, OperatorFailureClass,
};
use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, ContextFrontierId, PendingSteeringInput,
    PendingSteeringReclassificationIdentity, ReconstitutedToolAttempt, SemanticTranscriptEntryId,
    TurnId,
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
// numeric-bound: ceiling - prevents durable attempt deadlines expiring before work starts
const CLAIM_WINDOW: i64 = 1;

/// Rows one maintenance statement in the claim scan reads or writes.
///
/// Discovery, abandonment settlement, supersession, and exhaustion each walk
/// durable inventory rather than the one operation about to be worked, so each
/// is paged instead of claimed just in time. Every one of them either leaves
/// its page behind a cursor or removes it from its own predicate, so the rows
/// beyond a window are reached by the next scan rather than lost.
// numeric-bound: ceiling - bounds rows one reconciliation maintenance statement touches
const MAINTENANCE_WINDOW: i64 = 64;

/// How long any one reconciliation statement waits for a contended row.
///
/// All three transactions here - the claim scan, the claimed attempt, and the
/// durable failure record - take inventoried row locks unqualified: none skips
/// a locked row and none refuses to wait. Their caller also bounds them with a
/// client-side timeout, but that bounds only the daemon's patience, because
/// dropping a future queues a `ROLLBACK` rather than sending a `CancelRequest`:
/// the backend keeps waiting for the lock and the pooled connection stays
/// checked out for the full real wait while the caller has already given up and
/// will retry. Under live traffic that turns contention into connection
/// exhaustion.
///
/// `lock_timeout` bounds the database work itself and raises `55P03`, which
/// this repository records as an ordinary infrastructure failure and spends
/// against the attempt budget. It is set before anything is read or written, so
/// it can only interrupt a lock wait, never a commit - a transaction that trips
/// it either holds its rows or has touched nothing.
///
/// One budget covers all three because they wait on the same rows for the same
/// reason, and it is the write budget the quiescent terminalizer already uses:
/// wide enough that the shared outbox sequence row, which every writer in the
/// daemon holds from its first append until it commits, is ordinary traffic
/// rather than a stall, and finite so that one indefinite holder cannot stall
/// this loop.
///
/// It is published because the relationship that makes it work is with the
/// caller's bound rather than with anything in this module. The caller must let
/// this budget expire first; if the two are equal the client gives up before
/// `55P03` can arrive and strands the connection anyway, which is the whole
/// defect this bound exists to close. [`RECONCILIATION_ACQUIRE_WAIT`] takes
/// reaching a connection out of that race, and the caller carries the margin
/// for the rest.
// numeric-bound: ceiling - bounds one reconciliation statement's wait for a contended row
pub const RECONCILIATION_LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long one reconciliation transaction waits to reach a pooled connection.
///
/// The caller's client-side bound starts before the pool is asked for a
/// connection, so an acquisition that spent most of that bound would leave
/// [`RECONCILIATION_LOCK_WAIT`] no room to expire in: the client would give up
/// first, on a transaction that had not yet begun to be one. The shared pool's
/// own acquisition timeout is thirty seconds, which is that outcome with room
/// to spare.
///
/// It bounds reaching a connection and nothing after it. Cancelling an
/// acquisition is safe in a way cancelling a statement is not: no transaction
/// has begun, nothing has been sent, and so there is no work whose fate could
/// be unknown and no backend still carrying it. The value matches the budget
/// the quiescent terminalizer spends on the same step for the same reason.
///
/// It is published for the reason [`RECONCILIATION_LOCK_WAIT`] is: what makes
/// it correct is its relationship to bounds outside this module, so the tests
/// that pin that relationship read the constant rather than a literal.
// numeric-bound: tunable - bounds one reconciliation transaction's wait for a pooled connection
pub const RECONCILIATION_ACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// Reaches a pooled connection under [`RECONCILIATION_ACQUIRE_WAIT`].
///
/// The budget covers the acquisition alone. `Pool::begin` would put `BEGIN`
/// inside it, and cancelling that is not a smaller failure but the exact one
/// this module exists to prevent: the statement is already on the wire, so
/// dropping the future queues a `ROLLBACK` and the connection stays checked out
/// until the backend answers - under the database slowdown that made `BEGIN`
/// slow in the first place, and for successive watchdog attempts in turn.
///
/// A pool that cannot answer inside the budget reports the driver's own
/// `PoolTimedOut`, so the caller reads a plain infrastructure failure rather
/// than learning that this module wraps its acquisitions.
async fn acquire_bounded(
    pool: &PgPool,
) -> Result<PoolConnection<Postgres>, AutomaticReconciliationRepositoryError> {
    timeout(RECONCILIATION_ACQUIRE_WAIT, pool.acquire())
        .await
        .unwrap_or(Err(sqlx::Error::PoolTimedOut))
        .map_err(AutomaticReconciliationRepositoryError::database)
}

/// Drives `work` where a caller that gives up cannot cancel it.
///
/// Dropping a future is the only way an `async` caller abandons work, so a
/// deadline that wraps a database operation cancels it mid-flight. Driving the
/// operation on its own task separates the two: the caller's deadline abandons
/// the join handle, while the task keeps running and finishes what it sent.
///
/// A task that ends without producing its value can only have panicked, which
/// this crate's lint set denies; it is reported as the driver's own worker
/// failure so the caller reads one infrastructure class rather than two.
async fn uncancellable<T>(
    work: impl Future<Output = Result<T, AutomaticReconciliationRepositoryError>> + Send + 'static,
) -> Result<T, AutomaticReconciliationRepositoryError>
where
    T: Send + 'static,
{
    tokio::spawn(work).await.unwrap_or_else(|_| {
        Err(AutomaticReconciliationRepositoryError::database(
            sqlx::Error::WorkerCrashed,
        ))
    })
}

/// Opens the transaction and installs its budget, both beyond cancellation.
///
/// `BEGIN` and the `lock_timeout` statement are the one stretch of a
/// reconciliation transaction that no database-side budget covers, because the
/// budget is what the second of them installs. Until it lands the only bound in
/// play is the caller's whole-transaction deadline, and letting that deadline
/// end the stretch is the defect this module exists to close rather than a
/// smaller one: `BEGIN` is already on the wire, so dropping the future queues a
/// `ROLLBACK` behind a statement the backend has not acknowledged and the
/// pooled connection stays checked out for the full real wait - under the
/// slowdown that made `BEGIN` slow in the first place, and for successive
/// watchdog attempts in turn.
///
/// Running the stretch on its own task makes it uncancellable rather than
/// merely unbounded. A caller that gives up drops the join handle; the task
/// finishes `BEGIN`, and the transaction it opened is dropped where it is
/// complete, which is an ordinary rollback on a connection nobody is racing.
/// From the returned transaction onward [`RECONCILIATION_LOCK_WAIT`] is in
/// force, so the caller's deadline is what the specification says it is: the
/// last resort for a backend that has stopped answering at all, sitting above a
/// database-side budget that expires first.
///
/// The acquisition stays outside, under [`RECONCILIATION_ACQUIRE_WAIT`]:
/// abandoning it is free in the way abandoning a statement is not, so shielding
/// it would only delay a pool failure the caller can already read.
async fn begin_budgeted(
    pool: &PgPool,
) -> Result<Transaction<'static, Postgres>, AutomaticReconciliationRepositoryError> {
    let connection = acquire_bounded(pool).await?;
    uncancellable(async move {
        let mut transaction =
            Transaction::begin(MaybePoolConnection::PoolConnection(connection), None)
                .await
                .map_err(AutomaticReconciliationRepositoryError::database)?;
        bound_reconciliation_lock_wait(&mut transaction).await?;
        Ok(transaction)
    })
    .await
}

/// Applies [`RECONCILIATION_LOCK_WAIT`] to the transaction on `connection`.
///
/// Called before the transaction reads or writes anything, so the only
/// statement the budget can interrupt is one waiting for a row. A bound that
/// could fire later - over the whole statement, or over the future - might
/// interrupt the commit instead, and the caller would then not know whether the
/// attempt had ended.
async fn bound_reconciliation_lock_wait(
    connection: &mut PgConnection,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!("{}ms", RECONCILIATION_LOCK_WAIT.as_millis()))
        .execute(connection)
        .await?;
    Ok(())
}

/// The claim statement carries one `CASE` arm per admitted attempt, so its
/// arity is part of the contract with the domain budget: admitting another
/// attempt requires another arm and another bound parameter.
const _: () = assert!(AutomaticReconciliationAttempt::budget() == 5);

/// Returns the product attempt budget as the claim statement's parameter.
fn attempt_budget() -> i32 {
    i32::try_from(AutomaticReconciliationAttempt::budget()).unwrap_or(i32::MAX)
}

/// Returns the retry delay after `ordinal` fails, in whole seconds.
///
/// This is the only place the enforced retry schedule is chosen. It reads the
/// domain ladder so that the schedule the daemon runs and the schedule the
/// specification states cannot drift apart unnoticed.
fn retry_backoff_seconds(ordinal: u32) -> Result<i64, AutomaticReconciliationRepositoryError> {
    let attempt = AutomaticReconciliationAttempt::try_from_u32(ordinal).ok_or(
        AutomaticReconciliationRepositoryError::Corruption("attempt ordinal"),
    )?;
    i64::try_from(attempt.retry_backoff().as_secs())
        .map_err(|_| AutomaticReconciliationRepositoryError::Corruption("retry backoff"))
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
    ) -> Result<AutomaticReconciliationBatch, AutomaticReconciliationRepositoryError> {
        let mut transaction = begin_budgeted(&self.pool).await?;
        discover_recoveries(&mut transaction, MAINTENANCE_WINDOW).await?;
        settle_abandoned_attempts(&mut transaction, MAINTENANCE_WINDOW).await?;
        mark_superseded_recoveries(&mut transaction, MAINTENANCE_WINDOW).await?;
        let exhausted_rows =
            mark_exhausted_recoveries(&mut transaction, MAINTENANCE_WINDOW).await?;
        let mut claim = sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_CLAIM)
            .bind(CLAIM_WINDOW)
            .bind(attempt_budget());
        for ordinal in 1..=AutomaticReconciliationAttempt::budget() {
            claim = claim.bind(retry_backoff_seconds(ordinal)?);
        }
        let rows = claim.fetch_all(&mut *transaction).await?;
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
            transaction.commit().await.map_err(Self::commit_error)?;
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
        match (claimed.operation(), origin.as_str()) {
            (AutomaticReconciliationOperation::ModelCall(claimed_call), "accepted_input") => {
                let active = scheduling
                    .active_turn_execution()
                    .filter(|turn| turn.turn() == claimed.turn())
                    .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                        "accepted-input active turn for exact wait",
                    ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
                    .with_pending_steering_reclassifications(reclassification_identities(
                        active
                            .pending_steering()
                            .iter()
                            .map(PendingSteeringInput::accepted_input),
                    ));
                let reconciliation = check_reconciled_call(
                    scheduling.apply_automatic_reconciliation(attempt, identities),
                    claimed_call,
                )?;
                persist_automatic_reconciliation(&mut transaction, &reconciliation)
                    .await
                    .map_err(AutomaticReconciliationRepositoryError::Model)?;
            }
            (AutomaticReconciliationOperation::ModelCall(claimed_call), "delegation") => {
                let recovery = load_delegated_model_call_recovery(
                    &mut transaction,
                    claimed.session(),
                    &scheduling,
                )
                .await
                .map_err(AutomaticReconciliationRepositoryError::Model)?
                .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                    "delegated active turn for exact wait",
                ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
                    .with_pending_steering_reclassifications(reclassification_identities(
                        recovery
                            .active
                            .pending_steering()
                            .iter()
                            .map(PendingSteeringInput::accepted_input),
                    ));
                let reconciliation = check_reconciled_call(
                    recovery.active.apply_automatic_reconciliation(
                        recovery.call,
                        recovery.attempt,
                        recovery.source_snapshot,
                        attempt,
                        identities,
                    ),
                    claimed_call,
                )?;
                persist_automatic_reconciliation(&mut transaction, &reconciliation)
                    .await
                    .map_err(AutomaticReconciliationRepositoryError::Model)?;
            }
            (AutomaticReconciliationOperation::ToolAttempt(claimed_attempt), "accepted_input") => {
                let active = scheduling
                    .active_turn_execution()
                    .filter(|turn| turn.turn() == claimed.turn())
                    .ok_or(AutomaticReconciliationRepositoryError::Corruption(
                        "accepted-input active turn for exact wait",
                    ))?;
                let identities = AmbiguousModelCallTurnIdentities::new(terminal_frontier)
                    .with_pending_steering_reclassifications(reclassification_identities(
                        active
                            .pending_steering()
                            .iter()
                            .map(PendingSteeringInput::accepted_input),
                    ));
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
            // A delegated child can only reach a durable wait through its
            // model call: the delegated recovery reconstitution names an ended
            // model call and nothing else, so a tool-attempt wait recorded
            // against a delegated turn is durable state this daemon cannot
            // prove a transition from. It fails closed for an operator rather
            // than reconciling on evidence that was never loaded.
            (AutomaticReconciliationOperation::ToolAttempt(_), "delegation") => {
                return Err(AutomaticReconciliationRepositoryError::Corruption(
                    "delegated tool recovery origin",
                ));
            }
            (
                AutomaticReconciliationOperation::ModelCall(_)
                | AutomaticReconciliationOperation::ToolAttempt(_),
                _,
            ) => {
                return Err(AutomaticReconciliationRepositoryError::Corruption(
                    "origin for exact wait",
                ));
            }
        }
        finish_attempt(&mut transaction, claimed, "reconciled", "reconciled").await?;
        transaction.commit().await.map_err(Self::commit_error)?;
        Ok(AutomaticReconciliationOutcome::Reconciled)
    }

    /// Durably classifies an attempt whose authoritative transaction failed.
    ///
    /// Both updates take row locks that another daemon's settlement or
    /// supersession transaction can already hold - the claim scan settles
    /// abandoned attempts and marks superseded recoveries against exactly these
    /// two tables - so this transaction is bounded inside the database like its
    /// siblings. Unbounded, it is the same defect one step later: the caller's
    /// dropped future queues a `ROLLBACK` instead of cancelling, the backend
    /// keeps waiting, and a run of failing attempts strands a pooled connection
    /// apiece until the pool is gone. Failing as `55P03` costs nothing here:
    /// the attempt is already spent, and the claim scan's own abandonment
    /// settlement reaches a record this transaction could not write.
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
}

/// Names one fresh reclassified turn per pending steering input.
fn reclassification_identities(
    accepted: impl Iterator<Item = AcceptedInputId>,
) -> Vec<PendingSteeringReclassificationIdentity> {
    accepted
        .map(|accepted_input| {
            PendingSteeringReclassificationIdentity::new(
                accepted_input,
                TurnId::from_uuid(uuid::Uuid::now_v7()),
            )
        })
        .collect()
}

/// Requires that the aggregate reconciled the exact call this attempt claimed.
///
/// A transition for a different call is reported apart from a refused
/// transition because the two are different defects: this one means the durable
/// wait and the claim disagree about which call is ambiguous, and it is the
/// fail-closed path an operator reads a park from, so the two must not arrive
/// under one cause.
fn check_reconciled_call(
    transition: Result<
        signalbox_domain::ReconciliationRequiredModelCallTurn,
        signalbox_domain::ModelCallClosureError,
    >,
    claimed: signalbox_domain::ModelCallId,
) -> Result<
    signalbox_domain::ReconciliationRequiredModelCallTurn,
    AutomaticReconciliationRepositoryError,
> {
    match transition {
        Ok(reconciliation) if reconciliation.call().id() == claimed => Ok(reconciliation),
        Ok(_) => Err(AutomaticReconciliationRepositoryError::Corruption(
            "reconciled call identity for exact wait",
        )),
        Err(_) => Err(AutomaticReconciliationRepositoryError::Corruption(
            "aggregate transition for exact wait",
        )),
    }
}

async fn discover_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_DISCOVERY)
        .bind(window)
        .execute(connection)
        .await?;
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
) -> Result<(), AutomaticReconciliationRepositoryError> {
    sqlx::query(crate::lock_inventory::AUTOMATIC_RECONCILIATION_SUPERSESSION)
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
///   the rest. No cursor is needed for the same reason - unlike supersession,
///   every row this statement selects is also written.
/// * A `turn_lifecycle` correlation, exactly the one supersession uses one
///   statement earlier, naming the same exact operation the recovery row claims.
///   Without it a recovery whose turn already reached a terminal state - the
///   turn ended while the recovery row was still pending - would park an
///   operator against a wait that no longer exists. Such a row is left for
///   supersession, which is its correct disposition.
///
/// Only `scheduled` rows are exhausted. An `attempting` row at the budget is a
/// daemon that was lost mid-attempt; `settle_abandoned_attempts` normalizes it
/// to `scheduled` first, which is what closes its `attempting` attempt-history
/// row. Exhausting it here instead would strand that row `attempting` with
/// `finished_at IS NULL` forever, because settlement never revisits an exhausted
/// recovery - destroying the evidence trail for precisely the parks an operator
/// is being alerted about.
async fn mark_exhausted_recoveries(
    connection: &mut PgConnection,
    window: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, AutomaticReconciliationRepositoryError> {
    let rows = sqlx::query(
        "WITH page AS (
            SELECT recovery.turn_id
              FROM automatic_reconciliation AS recovery
             WHERE recovery.state_kind = 'scheduled'
               AND recovery.attempt_count = $2
               AND EXISTS (
                    SELECT 1 FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = recovery.turn_id
                       AND lifecycle.session_id = recovery.session_id
                       AND lifecycle.state_kind = 'active'
                       AND NOT lifecycle.delegation_runtime_terminal
                       AND (
                            (
                                lifecycle.active_phase_kind =
                                    'awaiting_model_call_recovery'
                                AND recovery.tool_attempt_id IS NULL
                                AND lifecycle.recovery_model_call_id =
                                    recovery.model_call_id
                            )
                            OR (
                                lifecycle.active_phase_kind =
                                    'awaiting_tool_recovery'
                                AND recovery.model_call_id IS NULL
                                AND lifecycle.recovery_tool_attempt_id =
                                    recovery.tool_attempt_id
                            )
                       )
               )
             LIMIT $1
         )
         UPDATE automatic_reconciliation AS recovery
            SET state_kind = 'exhausted', exhausted_at = statement_timestamp()
           FROM page
          WHERE recovery.turn_id = page.turn_id
      RETURNING recovery.session_id, recovery.turn_id, recovery.model_call_id,
                recovery.tool_attempt_id",
    )
    .bind(window)
    .bind(attempt_budget())
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
    use std::time::Duration;

    use tokio::{sync::oneshot, time::timeout};

    use super::uncancellable;
    use crate::lock_inventory::{
        AUTOMATIC_RECONCILIATION_DISCOVERY, AUTOMATIC_RECONCILIATION_SUPERSESSION,
    };

    /// A caller that gives up abandons its own wait, not the work.
    ///
    /// This is the whole property `begin_budgeted` rests on: `BEGIN` and the
    /// `lock_timeout` statement run where the caller's whole-transaction
    /// deadline cannot cancel them, so no future is ever dropped between the
    /// statement reaching the wire and the backend answering.
    ///
    /// Nothing here is timed. The caller's deadline is already expired when it
    /// first polls, and the work is held on a channel that is sent only after
    /// the caller has gone, so a shield that had become a plain `await` would
    /// leave that send with no receiver rather than losing a race.
    #[tokio::test]
    async fn work_a_caller_abandons_still_runs_to_completion() {
        let (release, released) = oneshot::channel::<()>();
        let (finish, finished) = oneshot::channel::<()>();
        let shielded = uncancellable(async move {
            let _ = released.await;
            let _ = finish.send(());
            Ok(())
        });

        let abandoned = timeout(Duration::ZERO, shielded).await;
        let _ = release.send(());

        assert!(
            abandoned.is_err(),
            "the caller gave up before the work could"
        );
        assert!(
            finished.await.is_ok(),
            "the abandoned work ran to completion on its own task"
        );
    }

    #[test]
    fn discovery_is_a_bounded_keyset_page() {
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("turn_id > bounds.after_turn_id"));
        assert!(!AUTOMATIC_RECONCILIATION_DISCOVERY.contains("origin_kind = 'accepted_input'"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("ORDER BY turn_id"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("LIMIT $1"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("SET after_turn_id"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("ORDER BY turn_id DESC"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("turn_id <= bounds.high_turn_id"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("high_turn_id = CASE"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("NOT delegation_runtime_terminal"));
    }

    /// Discovery admits both ambiguous shapes, and only one identity apiece.
    ///
    /// The generalized wait is the whole reason this statement exists in its
    /// current form: a tool attempt left ambiguous by a lost daemon parks the
    /// same way a model call does, and the durable row carries exactly one of
    /// the two identities.
    #[test]
    fn discovery_admits_model_call_and_tool_attempt_waits() {
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("'awaiting_model_call_recovery'"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("'awaiting_tool_recovery'"));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("num_nonnulls("));
        assert!(AUTOMATIC_RECONCILIATION_DISCOVERY.contains("recovery_tool_attempt_id"));
    }

    #[test]
    fn supersession_is_an_independent_bounded_keyset_page() {
        assert!(
            AUTOMATIC_RECONCILIATION_SUPERSESSION
                .contains("recovery.turn_id > bounds.after_turn_id")
        );
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("ORDER BY recovery.turn_id"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("LIMIT $1"));
        assert!(!AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("origin_kind = 'accepted_input'"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("SET after_turn_id"));
        assert!(
            AUTOMATIC_RECONCILIATION_SUPERSESSION
                .contains("NOT lifecycle.delegation_runtime_terminal")
        );
    }

    /// Supersession correlates the exact operation the recovery row claims.
    #[test]
    fn supersession_correlates_each_ambiguous_shape() {
        assert!(
            AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("lifecycle.recovery_model_call_id =")
        );
        assert!(
            AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("lifecycle.recovery_tool_attempt_id =")
        );
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("page.tool_attempt_id IS NULL"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("page.model_call_id IS NULL"));
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
            AUTOMATIC_RECONCILIATION_SUPERSESSION
                .contains("recovery.turn_id <= bounds.high_turn_id")
        );
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("ORDER BY recovery.turn_id DESC"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("SET after_turn_id = CASE"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("high_turn_id = CASE"));
        assert!(AUTOMATIC_RECONCILIATION_SUPERSESSION.contains("(SELECT count(*) FROM page) = $1"));
    }
}
