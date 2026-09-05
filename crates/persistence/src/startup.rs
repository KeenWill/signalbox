//! Atomic PostgreSQL recovery of prior-process active attempts.

use std::{collections::BTreeSet, error::Error, fmt, time::Duration};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StaleTurnCandidate, StartupScanIdGenerator,
    StartupScanRepository, StartupScanSessionOutcome, ToolCrashClosureIdentities,
};
use signalbox_domain::{
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities, AttemptEnd,
    CurrentModelCallState, FailedModelCallTurnIdentities, ModelCallDisposition, ModelCallId,
    ModelCallTerminalOutcome, PendingSteeringReclassificationIdentity,
    PreparedAcceptedInputTurnFailure, ReconstitutedToolAttempt,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload, SessionId,
    ToolAttemptCrashOutcome, TurnDisposition, TurnId, TurnTerminalCause,
    UnstoppedAttemptDisposition,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        input_position_to_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
        turn_id_to_uuid, turn_terminal_cause_to_str,
    },
    model_execution::persist_reclassified_pending_steering,
    model_execution::{
        ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        fail_tool_crash_in_transaction, insert_snapshot, lock_delegated_turn_terminal_frontier,
        persist_terminal_outcome, require_live_execution_for_restart,
    },
    outbox,
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputCorruption, SubmitInputRepositoryError, load_scheduling_projection},
    tool_loop::{
        ToolLoopRepositoryError, load_active_batch_from_connection, persist_ended_attempt,
        persist_result_entries, persist_tool_recovery_wait,
    },
};

/// Which fresh startup-recovery identity collided durably.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupScanIdentityCollision {
    /// A proposed tool-closure semantic-entry identity already exists.
    ToolClosureEntry,
    /// The proposed `TurnFailed` entry identity already exists.
    FailureEntry,
    /// The proposed terminal context-frontier identity already exists.
    TerminalFrontier,
    /// A proposed reclassified successor-turn identity already exists.
    ReclassifiedTurn,
}

impl fmt::Display for StartupScanIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::ToolClosureEntry => "tool-closure semantic-entry",
            Self::FailureEntry => "failure semantic-entry",
            Self::TerminalFrontier => "terminal context-frontier",
            Self::ReclassifiedTurn => "reclassified successor-turn",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for StartupScanIdentityCollision {}

/// A durable shape that cannot reconstruct or commit startup recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupScanCorruption {
    /// One required durable record is absent.
    Missing(&'static str),
    /// Correlated durable records disagree.
    Inconsistent(&'static str),
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records fail checked persistence mapping.
    Scheduling(SubmitInputCorruption),
    /// Complete model-call records fail checked persistence mapping.
    ModelCall(ModelCallCorruption),
}

impl fmt::Display for StartupScanCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing startup-scan {record}"),
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent startup-scan {relationship}")
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "startup-scan current Session is invalid: {error}"
                )
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "startup-scan scheduling projection is invalid: {error}"
                )
            }
            Self::ModelCall(error) => error.fmt(formatter),
        }
    }
}

impl Error for StartupScanCorruption {}

/// Database, integrity, or identity-collision failure during startup scan.
#[derive(Debug)]
pub enum StartupScanRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// Durable records cannot reconstruct or commit the accepted shape.
    Corruption {
        /// The invalid durable shape.
        source: StartupScanCorruption,
        /// The active durable turn observed for the scoped session.
        turn: Option<TurnId>,
    },
    /// A supplied fresh identity already names a durable record.
    IdentityCollision(StartupScanIdentityCollision),
}

impl fmt::Display for StartupScanRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => write!(formatter, "startup scan failed: {source}"),
            Self::Corruption { source, .. } => source.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
        }
    }
}

impl Error for StartupScanRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption { source, .. } => Some(source),
            Self::IdentityCollision(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for StartupScanRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption { .. } => OperatorFailureClass::FailClosedCorruption,
            Self::IdentityCollision(_) => OperatorFailureClass::IdentityCollision,
        }
    }
}

impl From<StartupScanCorruption> for StartupScanRepositoryError {
    fn from(error: StartupScanCorruption) -> Self {
        Self::Corruption {
            source: error,
            turn: None,
        }
    }
}

impl From<sqlx::Error> for StartupScanRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl StartupScanRepositoryError {
    /// Returns the relevant durable turn for corruption scoped to one active
    /// turn.
    pub const fn corruption_turn(&self) -> Option<TurnId> {
        match self {
            Self::Corruption { turn, .. } => *turn,
            Self::Database { .. } | Self::IdentityCollision(_) => None,
        }
    }

    fn from_database(error: sqlx::Error, commit_ambiguous: bool) -> Self {
        if let Some(collision) = identity_collision(&error) {
            Self::IdentityCollision(collision)
        } else {
            Self::Database {
                source: error,
                commit_ambiguous,
            }
        }
    }

    fn with_corruption_turn(self, turn: Option<TurnId>) -> Self {
        match self {
            Self::Corruption { source, turn: None } => Self::Corruption { source, turn },
            error => error,
        }
    }
}

pub(crate) enum TransactionDecision {
    Commit(StartupScanSessionOutcome),
    Rollback(StartupScanSessionOutcome),
}

/// PostgreSQL inventory and authoritative per-session recovery adapter.
#[derive(Clone, Debug)]
pub struct PostgresStartupScanRepository {
    pool: PgPool,
}

impl PostgresStartupScanRepository {
    /// Uses the supplied shared pool for startup recovery.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads the finite active-session inventory in deterministic order.
    pub async fn active_sessions(&self) -> Result<Box<[SessionId]>, StartupScanRepositoryError> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT session_id
               FROM (
                    SELECT session_id
                      FROM turn_lifecycle
                     WHERE state_kind = 'active'
                       AND NOT delegation_runtime_terminal
                    UNION
                    SELECT session_id
                      FROM context_compaction_model_call
                     WHERE state_kind <> 'terminal'
               ) AS recovery_inventory
              ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(session_id_from_uuid)
            .collect::<Vec<_>>()
            .into_boxed_slice())
    }

    /// Locks one session and atomically terminalizes its prior-process attempt.
    pub async fn recover<Generator>(
        &self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> Result<StartupScanSessionOutcome, StartupScanRepositoryError>
    where
        Generator: StartupScanIdGenerator + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let decision = recover_in_transaction(&mut transaction, session, identities, ids).await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await.map_err(|error| {
                    let commit_ambiguous = commit_failure_is_ambiguous(&error);
                    StartupScanRepositoryError::from_database(error, commit_ambiguous)
                })?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }
}

impl StartupScanRepository for PostgresStartupScanRepository {
    type Error = StartupScanRepositoryError;

    async fn active_sessions(&mut self) -> Result<Box<[SessionId]>, Self::Error> {
        PostgresStartupScanRepository::active_sessions(self).await
    }

    async fn recover<Generator>(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> Result<StartupScanSessionOutcome, Self::Error>
    where
        Generator: StartupScanIdGenerator + Send,
    {
        PostgresStartupScanRepository::recover(self, session, identities, ids).await
    }
}

async fn recover_in_transaction<Generator>(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnFailureIdentities,
    ids: &mut Generator,
) -> Result<TransactionDecision, StartupScanRepositoryError>
where
    Generator: StartupScanIdGenerator + Send,
{
    let session_uuid = session_id_to_uuid(requested_session);
    let observed_active_turn: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND state_kind = 'active'
            AND NOT delegation_runtime_terminal",
    )
    .bind(session_uuid)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(turn) = observed_active_turn {
        lock_delegated_turn_terminal_frontier(
            connection,
            requested_session,
            turn_id_from_uuid(turn),
        )
        .await
        .map_err(map_model_call_error)?;
    }
    let (session_exists, scheduler_session, active_turn) =
        sqlx::query_as::<_, (bool, Option<Uuid>, Option<Uuid>)>(
            crate::lock_inventory::STARTUP_RECOVERY,
        )
        .bind(session_uuid)
        .fetch_one(&mut *connection)
        .await?;

    if active_turn != observed_active_turn {
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    }

    let decision = recover_locked_session(
        connection,
        requested_session,
        identities,
        session_exists,
        scheduler_session,
        active_turn,
        ids,
    )
    .await
    .map_err(|error| error.with_corruption_turn(active_turn.map(turn_id_from_uuid)))?;

    if let (Some(turn), TransactionDecision::Commit(outcome)) = (active_turn, &decision)
        && startup_recovery_created_ambiguous_wait(outcome)
    {
        sqlx::query(
            "INSERT INTO turn_restart_recovery_origin
                (turn_id, session_id, recorded_at)
             VALUES ($1, $2, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(turn)
        .bind(session_uuid)
        .execute(&mut *connection)
        .await?;
    }

    Ok(decision)
}

fn startup_recovery_created_ambiguous_wait(outcome: &StartupScanSessionOutcome) -> bool {
    match outcome {
        StartupScanSessionOutcome::RecoveredModelCall(outcome) => matches!(
            outcome.as_ref(),
            ModelCallTerminalOutcome::AwaitingRecovery(_)
        ),
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome) => {
            matches!(outcome.as_ref(), ToolAttemptCrashOutcome::Ambiguous(_))
        }
        StartupScanSessionOutcome::Recovered(_)
        | StartupScanSessionOutcome::RecoveredContextCompaction { .. }
        | StartupScanSessionOutcome::ResumableToolBatch { .. }
        | StartupScanSessionOutcome::ResumablePreparedModelCall { .. }
        | StartupScanSessionOutcome::AwaitingRecoveryDecision { .. }
        | StartupScanSessionOutcome::NoActiveTurn => false,
    }
}

/// Recovers only the compaction an expired pre-activation pass abandoned.
///
/// [`recover_in_transaction`] falls through to whichever turn is active when a
/// session holds no unterminalized compaction, which is correct for a startup
/// scan: nothing else is running yet. The expiry handoff has no such
/// guarantee. It runs detached, its pass released the admission slot the moment
/// the bound expired, and it waits between attempts, so a later eligibility
/// sweep can activate a healthy successor turn before this transaction opens.
/// Falling through would then terminalize that successor. Reporting `None`
/// instead keeps recovery correlated with the evidence that justifies it — a
/// compaction still holding the session boundary — and leaves every other
/// shape to the watchdog that owns it.
///
/// `abandoned_call` is that evidence stated exactly. The session alone is not
/// enough to name it: expiry inside the read-only preflight leaves no durable
/// call at all, and by the time a delayed attempt opens this transaction a
/// later admitted pass can be running a different compaction for the same
/// session, which selecting on the session would terminalize. Only the call the
/// expired window itself made durable is recovered here.
pub(crate) async fn recover_abandoned_compaction_in_transaction(
    connection: &mut PgConnection,
    requested_session: SessionId,
    abandoned_call: ModelCallId,
    write_lock_wait: Option<Duration>,
) -> Result<Option<StartupScanSessionOutcome>, StartupScanRepositoryError> {
    let (session_exists, scheduler_session, active_turn) =
        sqlx::query_as::<_, (bool, Option<Uuid>, Option<Uuid>)>(
            crate::lock_inventory::STARTUP_RECOVERY,
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_one(&mut *connection)
        .await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(crate::turn_liveness::postgres_lock_timeout(write_lock_wait))
        .execute(&mut *connection)
        .await?;
    if scheduler_session.is_none() {
        if session_exists {
            return Err(StartupScanCorruption::Missing("session scheduler row").into());
        }
        return Ok(None);
    }
    recover_context_compaction(
        connection,
        requested_session,
        Some(abandoned_call),
        active_turn,
    )
    .await
}

pub(crate) async fn recover_observed_slot_held_in_transaction<Generator>(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
    identities: AcceptedInputTurnFailureIdentities,
    write_lock_wait: Option<Duration>,
    ids: &mut Generator,
) -> Result<Option<TransactionDecision>, StartupScanRepositoryError>
where
    Generator: StartupScanIdGenerator + Send,
{
    let requested_session = candidate.session();
    let session_uuid = session_id_to_uuid(requested_session);
    let observed_active_turn: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND state_kind = 'active'
            AND NOT delegation_runtime_terminal",
    )
    .bind(session_uuid)
    .fetch_optional(&mut *connection)
    .await?;
    if observed_active_turn != Some(turn_id_to_uuid(candidate.turn())) {
        return Ok(None);
    }
    lock_delegated_turn_terminal_frontier(connection, requested_session, candidate.turn())
        .await
        .map_err(map_model_call_error)?;
    let (session_exists, scheduler_session, active_turn) =
        sqlx::query_as::<_, (bool, Option<Uuid>, Option<Uuid>)>(
            crate::lock_inventory::STARTUP_RECOVERY,
        )
        .bind(session_uuid)
        .fetch_one(&mut *connection)
        .await?;
    // The acquisition budget has done its work: the scheduler row is held. The
    // write phase takes over with a budget of its own, exactly as the sibling
    // terminalization does — wide enough that the outbox's shared sequence row,
    // which every writer holds until it commits, is not mistaken for a stall.
    // Recovery reaches that same row, so without this switch its post-lock
    // statements are refused on ordinary busy-daemon traffic.
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(crate::turn_liveness::postgres_lock_timeout(write_lock_wait))
        .execute(&mut *connection)
        .await?;
    if active_turn != Some(turn_id_to_uuid(candidate.turn())) {
        return Ok(None);
    }
    let locked =
        crate::turn_liveness::read_exact_slot_held_candidate(connection, requested_session).await?;
    if locked != Some(candidate) {
        return Ok(None);
    }
    recover_locked_session(
        connection,
        requested_session,
        identities,
        session_exists,
        scheduler_session,
        active_turn,
        ids,
    )
    .await
    .map(Some)
    .map_err(|error| error.with_corruption_turn(active_turn.map(turn_id_from_uuid)))
}

async fn recover_locked_session<Generator>(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnFailureIdentities,
    session_exists: bool,
    scheduler_session: Option<Uuid>,
    active_turn: Option<Uuid>,
    ids: &mut Generator,
) -> Result<TransactionDecision, StartupScanRepositoryError>
where
    Generator: StartupScanIdGenerator + Send,
{
    if scheduler_session.is_none() {
        if session_exists {
            return Err(StartupScanCorruption::Missing("session scheduler row").into());
        }
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    }

    if let Some(recovered) =
        recover_context_compaction(connection, requested_session, None, active_turn).await?
    {
        return Ok(TransactionDecision::Commit(recovered));
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(StartupScanCorruption::Inconsistent("locked session disappeared").into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(StartupScanCorruption::CurrentSession(error).into());
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    let delegated_phase = match (scheduling.active_turn_execution(), active_turn) {
        (None, Some(turn)) => {
            let stored = sqlx::query_scalar::<_, Option<String>>(
                "SELECT active_phase_kind
                   FROM turn_lifecycle
                  WHERE session_id = $1
                    AND turn_id = $2
                    AND origin_kind = 'delegation'
                    AND state_kind = 'active'
                    AND NOT delegation_runtime_terminal",
            )
            .bind(session_id_to_uuid(requested_session))
            .bind(turn)
            .fetch_optional(&mut *connection)
            .await?;
            match stored {
                Some(Some(phase)) => Some(phase),
                Some(None) => {
                    return Err(
                        StartupScanCorruption::Inconsistent("delegated active phase").into(),
                    );
                }
                None => None,
            }
        }
        (Some(_), _) | (None, None) => None,
    };
    if let Some(phase) = delegated_phase.as_deref()
        && phase != "running"
    {
        let delegated_turn =
            active_turn
                .map(turn_id_from_uuid)
                .ok_or(StartupScanCorruption::Inconsistent(
                    "delegated active turn identity",
                ))?;
        let outcome = match phase {
            "awaiting_model_call_recovery" | "awaiting_tool_recovery" => {
                StartupScanSessionOutcome::AwaitingRecoveryDecision {
                    turn: delegated_turn,
                }
            }
            "awaiting_tool_approval" | "awaiting_child" | "awaiting_runner_recovery" => {
                StartupScanSessionOutcome::NoActiveTurn
            }
            _ => {
                return Err(StartupScanCorruption::Inconsistent("delegated active phase").into());
            }
        };
        return Ok(TransactionDecision::Rollback(outcome));
    }
    let accepted_active_turn = scheduling.active_turn_execution();
    let delegated_active_turn = (delegated_phase.as_deref() == Some("running"))
        .then(|| active_turn.map(turn_id_from_uuid))
        .flatten();
    let active_turn_id = accepted_active_turn
        .as_ref()
        .map(signalbox_domain::ActivatedAcceptedInputTurn::turn)
        .or(delegated_active_turn);
    let Some(active_turn_id) = active_turn_id else {
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    };
    match accepted_active_turn.as_ref().map(|turn| turn.phase()) {
        None => {}
        Some(signalbox_domain::ActiveTurnPhase::Running { .. }) => {}
        // A prior process already ended this turn's physical tenure and
        // recorded the exact ambiguity set, so there is no lost live end for
        // the scan to classify. The independent automatic-reconciliation
        // watchdog owns both model-call and tool-attempt waits.
        Some(signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) if ambiguous_operations.iter().all(|operation| {
            matches!(
                operation,
                signalbox_domain::IssuedOperationRef::ModelCall(_)
            )
        }) =>
        {
            return Ok(TransactionDecision::Rollback(
                StartupScanSessionOutcome::AwaitingRecoveryDecision {
                    turn: active_turn_id,
                },
            ));
        }
        Some(signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision { .. })
        | Some(signalbox_domain::ActiveTurnPhase::AwaitingApproval { .. })
        | Some(signalbox_domain::ActiveTurnPhase::AwaitingChild { .. })
        | Some(signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }) => {
            return Ok(TransactionDecision::Rollback(
                StartupScanSessionOutcome::NoActiveTurn,
            ));
        }
    }
    let pending_steering = accepted_active_turn
        .as_ref()
        .and_then(|turn| turn.pending_steering().first())
        .map(signalbox_domain::PendingSteeringInput::accepted_input);

    if let Some(batch) =
        load_active_batch_from_connection(connection, requested_session, active_turn_id)
            .await
            .map_err(map_tool_loop_error)?
    {
        let Some(current) =
            batch
                .requests()
                .iter()
                .find_map(|request| match batch.attempt(request.id()) {
                    Some(ReconstitutedToolAttempt::Current(current)) => Some(current.clone()),
                    Some(ReconstitutedToolAttempt::Ended(_)) | None => None,
                })
        else {
            return Ok(TransactionDecision::Rollback(
                StartupScanSessionOutcome::ResumableToolBatch {
                    turn: active_turn_id,
                },
            ));
        };
        let outcome = current.classify_crash_loss();
        let ended = match &outcome {
            ToolAttemptCrashOutcome::KnownFailed(ended)
            | ToolAttemptCrashOutcome::Ambiguous(ended) => ended,
        };
        persist_ended_attempt(connection, ended)
            .await
            .map_err(map_tool_loop_error)?;
        match &outcome {
            ToolAttemptCrashOutcome::Ambiguous(ended) => {
                persist_tool_recovery_wait(connection, ended, true)
                    .await
                    .map_err(map_tool_loop_error)?;
                return Ok(TransactionDecision::Commit(
                    StartupScanSessionOutcome::RecoveredToolAttempt(Box::new(outcome)),
                ));
            }
            ToolAttemptCrashOutcome::KnownFailed(_) => {
                let closure = ToolCrashClosureIdentities::new(
                    (0..batch.requests().len())
                        .map(|_| ids.next_tool_closure_entry_id())
                        .collect(),
                    ids.next_tool_closure_frontier_id(),
                    FailedModelCallTurnIdentities::new(
                        identities.failure_entry(),
                        identities.terminal_frontier(),
                    ),
                );
                let closed_batch = load_active_batch_from_connection(
                    connection,
                    requested_session,
                    active_turn_id,
                )
                .await
                .map_err(map_tool_loop_error)?
                .ok_or(StartupScanCorruption::Missing("crash-closed tool batch"))?;
                let projection = closed_batch
                    .prepare_failure_projection(
                        closure.result_entries().to_vec(),
                        closure.result_frontier(),
                    )
                    .map_err(|_| {
                        StartupScanCorruption::Inconsistent("known tool crash closure projection")
                    })?;
                persist_result_entries(connection, &projection)
                    .await
                    .map_err(map_tool_loop_error)?;
                insert_snapshot(connection, projection.snapshot())
                    .await
                    .map_err(map_model_call_error)?;
                fail_tool_crash_in_transaction(
                    connection,
                    requested_session,
                    active_turn_id,
                    &projection,
                    closure.failure().clone(),
                    |accepted_input| ids.next_reclassified_turn_id(accepted_input),
                )
                .await
                .map_err(map_model_call_error)?;
                return Ok(TransactionDecision::Commit(
                    StartupScanSessionOutcome::RecoveredToolAttempt(Box::new(outcome)),
                ));
            }
        }
    }

    let delegated_recovery = delegated_active_turn.is_some();
    let model_execution = require_live_execution_for_restart(connection, requested_session)
        .await
        .map_err(map_model_call_error)?;
    if let Some(call_state) = model_execution.current_call().map(|call| call.state()) {
        if call_state == CurrentModelCallState::Prepared {
            return Ok(TransactionDecision::Rollback(
                StartupScanSessionOutcome::ResumablePreparedModelCall {
                    turn: model_execution.turn(),
                },
            ));
        }
        let mut failure_identities = FailedModelCallTurnIdentities::new(
            identities.failure_entry(),
            identities.terminal_frontier(),
        );
        if call_state == CurrentModelCallState::CancellationRequested {
            let mut proposed_turns = BTreeSet::new();
            let mut reclassifications = Vec::new();
            for pending in model_execution.active_turn().pending_steering() {
                let accepted_input = pending.accepted_input();
                let proposed_turn = ids.next_reclassified_turn_id(accepted_input);
                record_reclassified_turn_candidate(
                    model_execution.turn(),
                    proposed_turn,
                    &mut proposed_turns,
                )?;
                reclassifications.push(PendingSteeringReclassificationIdentity::new(
                    accepted_input,
                    proposed_turn,
                ));
            }
            failure_identities =
                failure_identities.with_pending_steering_reclassifications(reclassifications);
        }
        let outcome = model_execution
            .recover_after_restart(failure_identities)
            .map_err(|_| {
                StartupScanCorruption::Inconsistent("model-call restart classification")
            })?;
        if !matches!(
            outcome,
            ModelCallTerminalOutcome::Failed(_)
                | ModelCallTerminalOutcome::AwaitingRecovery(_)
                | ModelCallTerminalOutcome::ReconciliationRequired(_)
        ) {
            return Err(StartupScanCorruption::Inconsistent("model-call restart outcome").into());
        }
        persist_terminal_outcome(
            connection,
            &outcome,
            Some(TurnTerminalCause::AbandonedAtRestart),
        )
        .await
        .map_err(map_model_call_error)?;
        return Ok(TransactionDecision::Commit(
            StartupScanSessionOutcome::RecoveredModelCall(Box::new(outcome)),
        ));
    }

    if pending_steering.is_some() || delegated_recovery {
        let mut proposed_turns = BTreeSet::new();
        let mut reclassifications = Vec::new();
        for pending in model_execution.active_turn().pending_steering() {
            let accepted_input = pending.accepted_input();
            let proposed_turn = ids.next_reclassified_turn_id(accepted_input);
            record_reclassified_turn_candidate(
                model_execution.turn(),
                proposed_turn,
                &mut proposed_turns,
            )?;
            reclassifications.push(PendingSteeringReclassificationIdentity::new(
                accepted_input,
                proposed_turn,
            ));
        }
        let failure_identities = FailedModelCallTurnIdentities::new(
            identities.failure_entry(),
            identities.terminal_frontier(),
        )
        .with_pending_steering_reclassifications(reclassifications);
        let failed = model_execution
            .recover_evidence_free_after_restart(failure_identities)
            .map_err(|_| {
                StartupScanCorruption::Inconsistent("evidence-free restart classification")
            })?;
        let outcome = ModelCallTerminalOutcome::Failed(failed);
        persist_terminal_outcome(
            connection,
            &outcome,
            Some(TurnTerminalCause::AbandonedAtRestart),
        )
        .await
        .map_err(map_model_call_error)?;
        return Ok(TransactionDecision::Commit(
            StartupScanSessionOutcome::RecoveredModelCall(Box::new(outcome)),
        ));
    }

    let identities = lost_failure_identities(identities, &scheduling, ids);
    let prepared = match scheduling.prepare_active_turn_lost_failure(identities) {
        Ok(prepared) => prepared,
        Err(error) => match error.failure() {
            AcceptedInputTurnFailureFailure::NoActiveTurn => {
                return Ok(TransactionDecision::Rollback(
                    StartupScanSessionOutcome::NoActiveTurn,
                ));
            }
            AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists => {
                return Err(StartupScanRepositoryError::IdentityCollision(
                    StartupScanIdentityCollision::FailureEntry,
                ));
            }
            AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists => {
                return Err(StartupScanRepositoryError::IdentityCollision(
                    StartupScanIdentityCollision::TerminalFrontier,
                ));
            }
            AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch
            | AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
            | AcceptedInputTurnFailureFailure::ActiveStartMissing
            | AcceptedInputTurnFailureFailure::StartingSnapshotMissing
            | AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend => {
                return Err(
                    StartupScanCorruption::Inconsistent("active failure preparation").into(),
                );
            }
        },
    };

    let failed =
        insert_prepared_failure(connection, prepared, TurnTerminalCause::AbandonedAtRestart)
            .await?;
    Ok(TransactionDecision::Commit(
        StartupScanSessionOutcome::Recovered(Box::new(failed)),
    ))
}

/// Commits one prepared failed-turn transition, recording `cause` as why the
/// turn ended.
///
/// The startup scan and the liveness watchdog commit the identical transition,
/// which is what keeps every terminal trigger firing for both; the cause is
/// what makes the two distinguishable in the rows rather than only in a log.
/// Proposes one fresh successor turn per steering input pending on the
/// active turn, so a lost failure reclassifies rather than refuses.
pub(crate) fn lost_failure_identities<Generator>(
    identities: AcceptedInputTurnFailureIdentities,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
    ids: &mut Generator,
) -> AcceptedInputTurnFailureIdentities
where
    Generator: StartupScanIdGenerator,
{
    let reclassifications = scheduling
        .active_turn_execution()
        .map(|execution| {
            execution
                .pending_steering()
                .iter()
                .map(|pending| {
                    let accepted_input = pending.accepted_input();
                    PendingSteeringReclassificationIdentity::new(
                        accepted_input,
                        ids.next_reclassified_turn_id(accepted_input),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    identities.with_pending_steering_reclassifications(reclassifications)
}

pub(crate) async fn insert_prepared_failure(
    connection: &mut PgConnection,
    prepared: PreparedAcceptedInputTurnFailure,
    cause: TurnTerminalCause,
) -> Result<signalbox_domain::FailedAcceptedInputTurn, StartupScanRepositoryError> {
    let (failed, failure_entry, terminal_snapshot, reclassified) = prepared.into_parts();
    let session = failed.session();
    let turn = failed.turn();
    if failure_entry.source_session() != session
        || failure_entry.payload() != &(InitialSemanticTranscriptEntryPayload::TurnFailed { turn })
        || terminal_snapshot.frontier().owning_session() != session
        || terminal_snapshot.frontier().snapshot() != failed.terminal_frontier()
        || failed.disposition() != &TurnDisposition::Failed
    {
        return Err(StartupScanCorruption::Inconsistent("prepared failure ownership").into());
    }
    let attempt = failed.ended_attempt();
    if attempt.end()
        != &(AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        })
    {
        return Err(StartupScanCorruption::Inconsistent("prepared Lost attempt end").into());
    }

    let ended = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'lost'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if ended != 1 {
        return Err(StartupScanCorruption::Inconsistent("guarded attempt end cardinality").into());
    }

    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session_id_to_uuid(session))
    .bind(failure_entry.identity().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?;

    insert_snapshot(connection, &terminal_snapshot)
        .await
        .map_err(map_model_call_error)?;
    persist_reclassified_pending_steering(connection, session, turn, &reclassified)
        .await
        .map_err(map_model_call_error)?;

    let updated = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_model_call_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = $8,
                active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL,
                recovery_tool_attempt_id = NULL
          WHERE turn_id = $2
            AND session_id = $3
            AND origin_accepted_input_id = $4
            AND acceptance_position = $5
            AND state_kind = 'active'
            AND starting_frontier_id = $6
            AND active_phase_kind = 'running'
            AND current_attempt_id = $7",
    )
    .bind(terminal_snapshot.frontier().snapshot().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(failed.accepted_input().id().into_uuid())
    .bind(input_position_to_numeric(
        failed.order().acceptance_position(),
    ))
    .bind(failed.start().frontier().snapshot().into_uuid())
    .bind(attempt.id().into_uuid())
    .bind(turn_terminal_cause_to_str(cause))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(
            StartupScanCorruption::Inconsistent("guarded lifecycle terminalization").into(),
        );
    }

    outbox::append(
        connection,
        outbox::OutboxEvent::TurnTerminal {
            session,
            turn,
            disposition: outbox::TurnTerminalOutboxDisposition::Failed {
                failure_entry: failure_entry.identity(),
                terminal_frontier: terminal_snapshot.frontier().snapshot(),
            },
        },
    )
    .await?;

    Ok(failed)
}

/// `only_call`, when given, restricts recovery to that exact compaction call.
/// A startup scan passes `None`: it runs before anything else can be inside the
/// session, so whatever nonterminal compaction it finds is by construction the
/// one the prior process abandoned. The expiry handoff cannot assume that and
/// names its call.
async fn recover_context_compaction(
    connection: &mut PgConnection,
    session: SessionId,
    only_call: Option<ModelCallId>,
    active_turn: Option<Uuid>,
) -> Result<Option<StartupScanSessionOutcome>, StartupScanRepositoryError> {
    let rows = sqlx::query(
        "SELECT call.model_call_id, call.state_kind,
                command.command_id, command.result_kind
           FROM context_compaction_model_call AS call
           FULL OUTER JOIN compact_session_command AS command
             ON command.session_id = call.session_id
            AND command.model_call_id = call.model_call_id
          WHERE COALESCE(call.session_id, command.session_id) = $1
            AND (
                $2::uuid IS NULL
                OR COALESCE(call.model_call_id, command.model_call_id) = $2
            )
            AND (
                call.state_kind <> 'terminal'
                OR command.result_kind = 'pending'
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(only_call.map(ModelCallId::into_uuid))
    .fetch_all(&mut *connection)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 || active_turn.is_some() {
        return Err(StartupScanCorruption::Inconsistent("compaction recovery inventory").into());
    }
    let row = &rows[0];
    let call_id: Option<Uuid> = row.try_get("model_call_id")?;
    let call_state: Option<String> = row.try_get("state_kind")?;
    let command_id: Option<Uuid> = row.try_get("command_id")?;
    let command_state: Option<String> = row.try_get("result_kind")?;
    let (Some(call_id), Some(call_state), Some(command_id)) = (call_id, call_state, command_id)
    else {
        return Err(StartupScanCorruption::Inconsistent("compaction recovery correlation").into());
    };
    if command_state.as_deref() != Some("pending") {
        return Err(
            StartupScanCorruption::Inconsistent("compaction recovery command state").into(),
        );
    }
    let (stored_disposition, disposition) = match call_state.as_str() {
        "prepared" => ("known_failed", ModelCallDisposition::KnownFailed),
        "in_flight" => ("ambiguous", ModelCallDisposition::Ambiguous),
        _ => {
            return Err(
                StartupScanCorruption::Inconsistent("compaction recovery call state").into(),
            );
        }
    };
    let call_rows = sqlx::query(
        "UPDATE context_compaction_model_call
            SET state_kind = 'terminal', terminal_at = statement_timestamp(),
                terminal_disposition_kind = $1
          WHERE session_id = $2
            AND model_call_id = $3
            AND state_kind = $4",
    )
    .bind(stored_disposition)
    .bind(session_id_to_uuid(session))
    .bind(call_id)
    .bind(&call_state)
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let command_rows = sqlx::query(
        "UPDATE compact_session_command
            SET result_kind = 'failed'
          WHERE session_id = $1
            AND command_id = $2
            AND model_call_id = $3
            AND result_kind = 'pending'",
    )
    .bind(session_id_to_uuid(session))
    .bind(command_id)
    .bind(call_id)
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if call_rows != 1 || command_rows != 1 {
        return Err(StartupScanCorruption::Inconsistent("guarded compaction recovery").into());
    }
    Ok(Some(
        StartupScanSessionOutcome::RecoveredContextCompaction {
            call: ModelCallId::from_uuid(call_id),
            disposition,
        },
    ))
}

pub(crate) fn map_scheduling_error(
    error: SubmitInputRepositoryError,
) -> StartupScanRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::CommitAmbiguous(error) => {
            StartupScanRepositoryError::from_database(error, true)
        }
        SubmitInputRepositoryError::Corruption(error) => {
            StartupScanCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            StartupScanCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            StartupScanCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::UnsupportedModelSetting(_) => {
            StartupScanCorruption::Inconsistent("origin model settings").into()
        }
        SubmitInputRepositoryError::ModelExecution(_) => {
            StartupScanCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn map_tool_loop_error(error: ToolLoopRepositoryError) -> StartupScanRepositoryError {
    match error {
        ToolLoopRepositoryError::Database { source, .. } => source.into(),
        ToolLoopRepositoryError::IdentityCollision => {
            StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ToolClosureEntry,
            )
        }
        ToolLoopRepositoryError::Corruption(_)
        | ToolLoopRepositoryError::DifferentCommandKind
        | ToolLoopRepositoryError::ConflictingCommandReuse
        | ToolLoopRepositoryError::InvalidTransition(_) => {
            StartupScanCorruption::Inconsistent("tool-attempt restart state").into()
        }
    }
}

fn map_model_call_error(error: ModelCallRepositoryError) -> StartupScanRepositoryError {
    match error {
        ModelCallRepositoryError::Database { source, .. } => source.into(),
        ModelCallRepositoryError::Corruption(source) => {
            StartupScanCorruption::ModelCall(source).into()
        }
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry) => {
            StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::FailureEntry,
            )
        }
        ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::TerminalFrontier,
        ) => StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::TerminalFrontier,
        ),
        ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn,
        ) => StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::ReclassifiedTurn,
        ),
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ModelCall)
        | ModelCallRepositoryError::NoLiveExecution
        | ModelCallRepositoryError::InvalidTransition(_) => {
            StartupScanCorruption::Inconsistent("model-call recovery transition").into()
        }
    }
}

fn record_reclassified_turn_candidate(
    source_turn: TurnId,
    proposed_turn: TurnId,
    proposed_turns: &mut BTreeSet<TurnId>,
) -> Result<(), StartupScanRepositoryError> {
    if proposed_turn == source_turn || !proposed_turns.insert(proposed_turn) {
        return Err(StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::ReclassifiedTurn,
        ));
    }
    Ok(())
}

fn identity_collision(error: &sqlx::Error) -> Option<StartupScanIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(StartupScanIdentityCollision::FailureEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(StartupScanIdentityCollision::TerminalFrontier)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeSet, error::Error, fmt, io};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::TurnId;
    use sqlx::error::{DatabaseError, ErrorKind};
    use sqlx::types::Uuid;

    use super::{
        StartupScanCorruption, StartupScanIdentityCollision, StartupScanRepositoryError,
        commit_failure_is_ambiguous, map_tool_loop_error, record_reclassified_turn_candidate,
    };
    use crate::tool_loop::ToolLoopRepositoryError;

    /// INV-034: a generated source-turn identity is a retryable collision, not
    /// durable corruption.
    #[test]
    fn inv034_generated_successor_source_candidate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let mut proposed = BTreeSet::new();

        assert!(matches!(
            record_reclassified_turn_candidate(source, source, &mut proposed),
            Err(StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    /// INV-034: a duplicate generated successor is a retryable collision, not
    /// durable corruption.
    #[test]
    fn inv034_generated_successor_duplicate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let successor = TurnId::from_uuid(Uuid::from_u128(2));
        let mut proposed = BTreeSet::new();

        record_reclassified_turn_candidate(source, successor, &mut proposed)
            .expect("the first source-safe successor is accepted");
        assert!(matches!(
            record_reclassified_turn_candidate(source, successor, &mut proposed),
            Err(StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    #[test]
    fn tool_closure_identity_collision_remains_retryable_at_startup_boundary() {
        assert!(matches!(
            map_tool_loop_error(ToolLoopRepositoryError::IdentityCollision),
            StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ToolClosureEntry
            )
        ));
    }

    #[derive(Debug)]
    struct ServerCommitFailure {
        code: &'static str,
    }

    impl fmt::Display for ServerCommitFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("server reported commit failure")
        }
    }

    impl Error for ServerCommitFailure {}

    impl DatabaseError for ServerCommitFailure {
        fn message(&self) -> &str {
            "server reported commit failure"
        }

        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }
    }

    #[test]
    fn corruption_retains_the_scoped_durable_turn() {
        let turn = TurnId::from_uuid(Uuid::from_u128(1));
        let error =
            StartupScanRepositoryError::from(StartupScanCorruption::Missing("active turn record"))
                .with_corruption_turn(Some(turn));

        assert_eq!(error.corruption_turn(), Some(turn));
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
    }

    #[test]
    fn precommit_database_failure_is_not_commit_ambiguous() {
        let error = StartupScanRepositoryError::from_database(sqlx::Error::PoolClosed, false);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn lost_commit_response_is_commit_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let error = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_rejected_commit_is_not_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "23514" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(!commit_ambiguous);
        let error = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn server_reported_transaction_resolution_unknown_is_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "08007" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let classified = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_reported_statement_completion_unknown_is_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "40003" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let classified = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }
}
