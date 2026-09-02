//! PostgreSQL adapter for the core-owned session lifecycle satellite.
//!
//! The satellite holds the durable session state, its typed detail, the
//! ownership bit, and the terminal outcome. Two of its three moving parts are
//! not written here at all, on purpose:
//!
//! - The §1 mapping — the states the turn and goal machines derive — is
//!   projected by the database from every `turn_lifecycle` and `goal_event`
//!   write, so a path that moves a turn cannot leave the session state behind
//!   it, including a path a later change adds.
//! - The armed deadline is written by the satellite's own trigger from the
//!   configured bound table, so §1's one-armed-deadline invariant holds by
//!   construction rather than by every caller remembering to re-arm.
//!
//! What this module writes is the part no machine below the session can
//! decide: creation, the park that overrides the mapping, the ownership flip,
//! and the closure — which settles the live goal generation in the same
//! transaction, because a pursuing goal beneath a terminal session would keep
//! scheduling work no one owns.

use std::{error::Error, fmt, time::Duration};

use signalbox_domain::{
    CoreAgency, Goal, LifecycleActor, SessionClosureOutcome, SessionCreationCause,
    SessionFailureCause, SessionId, SessionLifecycleState, SessionOwnership,
    SessionOwnershipTransition, SessionParkCause, SessionParkResponder, SessionTerminalOutcome,
    SessionWait, SessionWaitKind, StopStickiness,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    goal::{self, GoalRepositoryError},
    lock_inventory,
    mapping::{
        dispatching_module_to_str, goal_blocked_reason_from_str, goal_blocked_reason_to_str,
        lifecycle_actor_to_str, session_id_from_uuid, session_id_to_uuid,
        session_park_cause_from_str, session_recovery_operation_from_str,
        session_recovery_operation_to_str, session_retirement_cause_from_str,
        session_retryable_cause_from_str, session_structural_cause_from_str,
        session_wait_kind_from_str, session_wait_kind_to_str, session_waker_to_str,
    },
};

/// A durable lifecycle shape that cannot construct the public domain values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionLifecycleCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator is unsupported.
    Unsupported {
        /// The field whose spelling is unsupported.
        field: &'static str,
        /// The stored spelling.
        value: String,
    },
    /// Typed record relationships or variant fields disagree.
    Inconsistent(&'static str),
}

impl fmt::Display for SessionLifecycleCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "session lifecycle is missing {field}"),
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "session lifecycle {field} is unsupported: {value}"
                )
            }
            Self::Inconsistent(detail) => {
                write!(formatter, "session lifecycle is inconsistent: {detail}")
            }
        }
    }
}

impl Error for SessionLifecycleCorruption {}

/// Why one lifecycle transition was refused before any row changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleRejection {
    /// §1's algebra does not admit the transition from the held state.
    TransitionNotAdmitted,
    /// `release` on a `parked` session: `parked` is an owned-only state, so
    /// the park is closed or resumed first (§6).
    ReleaseWhileParked,
    /// The session already holds the ownership the flip would install.
    OwnershipUnchanged,
    /// The closure names an outcome whose goal event the goal contract already
    /// spells, and the generation is still open: the goal command settles it.
    GoalGenerationStillOpen,
    /// The session holds no committed terminal handoff to settle.
    NoPendingTerminal,
}

impl fmt::Display for SessionLifecycleRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::TransitionNotAdmitted => "the session state does not admit the transition",
            Self::ReleaseWhileParked => "a parked session cannot be released",
            Self::OwnershipUnchanged => "the session already holds that ownership",
            Self::GoalGenerationStillOpen => "the goal generation is still open",
            Self::NoPendingTerminal => "the session holds no pending terminal outcome",
        };
        formatter.write_str(detail)
    }
}

/// A database failure, ambiguous commit, refused transition, or corruption.
#[derive(Debug)]
pub enum SessionLifecycleRepositoryError {
    /// The database rejected or could not run one statement.
    Database(sqlx::Error),
    /// The commit response was lost; the outcome is unknown.
    CommitAmbiguous(sqlx::Error),
    /// The named session has no lifecycle row.
    UnknownSession(SessionId),
    /// The transition was refused before any row changed.
    Rejected(SessionLifecycleRejection),
    /// The goal lineage beneath the session could not settle.
    Goal(Box<GoalRepositoryError>),
    /// Durable state cannot construct the domain value.
    Corruption(SessionLifecycleCorruption),
}

impl fmt::Display for SessionLifecycleRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("session lifecycle database failure"),
            Self::CommitAmbiguous(_) => {
                formatter.write_str("session lifecycle commit outcome is unknown")
            }
            Self::UnknownSession(session) => {
                write!(formatter, "session {session:?} has no lifecycle row")
            }
            Self::Rejected(rejection) => write!(formatter, "{rejection}"),
            Self::Goal(_) => formatter.write_str("session closure could not settle its goal"),
            Self::Corruption(corruption) => write!(formatter, "{corruption}"),
        }
    }
}

impl Error for SessionLifecycleRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Goal(error) => Some(error.as_ref()),
            Self::Corruption(error) => Some(error),
            Self::UnknownSession(_) | Self::Rejected(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionLifecycleRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SessionLifecycleCorruption> for SessionLifecycleRepositoryError {
    fn from(error: SessionLifecycleCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<GoalRepositoryError> for SessionLifecycleRepositoryError {
    fn from(error: GoalRepositoryError) -> Self {
        Self::Goal(Box::new(error))
    }
}

/// Deployment policy for every armed session deadline.
///
/// `None` is the configured `none` — unbounded, journaled, and alarm-exempt.
/// The default is unbounded in every position so a caller that has not written
/// its policy still satisfies §1's invariant with a recorded unbounded
/// deadline rather than an absent one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionLifecycleNumericBounds {
    /// `dispatched` to `active`.
    pub dispatch: Option<Duration>,
    /// A held start gate.
    pub start_gate: Option<Duration>,
    /// An owned ungated creation's first input.
    pub first_input: Option<Duration>,
    /// An active session making no progress.
    pub active_stall: Option<Duration>,
    /// An outstanding approval decision.
    pub waiting_approval: Option<Duration>,
    /// An external gate.
    pub waiting_external: Option<Duration>,
    /// A delegated child.
    pub waiting_child: Option<Duration>,
    /// A provider backoff.
    pub waiting_provider_retry: Option<Duration>,
    /// Pipeline backlog.
    pub waiting_pipeline: Option<Duration>,
    /// A recorded scheduler fault.
    pub waiting_scheduler: Option<Duration>,
    /// A running recovery.
    pub recovering: Option<Duration>,
    /// A blocked generation.
    pub blocked: Option<Duration>,
    /// The parked re-notification interval.
    pub parked_renotify: Option<Duration>,
}

impl SessionLifecycleNumericBounds {
    /// Returns each deadline's durable spelling beside its configured bound.
    fn rows(&self) -> [(&'static str, Option<Duration>); 13] {
        [
            ("dispatch", self.dispatch),
            ("start_gate", self.start_gate),
            ("first_input", self.first_input),
            ("active_stall", self.active_stall),
            ("waiting_approval", self.waiting_approval),
            ("waiting_external", self.waiting_external),
            ("waiting_child", self.waiting_child),
            ("waiting_provider_retry", self.waiting_provider_retry),
            ("waiting_pipeline", self.waiting_pipeline),
            ("waiting_scheduler", self.waiting_scheduler),
            ("recovering", self.recovering),
            ("blocked", self.blocked),
            ("parked_renotify", self.parked_renotify),
        ]
    }
}

/// One session's durable lifecycle facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLifecycleRecord {
    session: SessionId,
    state: SessionLifecycleState,
    ownership: SessionOwnership,
    actor: LifecycleActor,
    pending_terminal: Option<SessionTerminalOutcome>,
}

impl SessionLifecycleRecord {
    /// Returns the session these facts describe.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the durable state.
    pub const fn state(&self) -> SessionLifecycleState {
        self.state
    }

    /// Returns whether the daemon holds a liveness obligation.
    pub const fn ownership(&self) -> SessionOwnership {
        self.ownership
    }

    /// Returns the classified actor of the transition that produced the state.
    pub const fn actor(&self) -> LifecycleActor {
        self.actor
    }

    /// Returns the outcome a closure committed to while a turn still settles.
    pub const fn pending_terminal(&self) -> Option<SessionTerminalOutcome> {
        self.pending_terminal
    }
}

/// PostgreSQL implementation of the session lifecycle port.
#[derive(Clone, Debug)]
pub struct SessionLifecycleRepository {
    pool: PgPool,
}

impl SessionLifecycleRepository {
    /// Uses the supplied pool for atomic transitions and fail-closed loads.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Writes the deployment's configured deadline policy.
    ///
    /// §1 lets a bound live in config or the database. The daemon's
    /// `[numeric_bounds]` policy is the source; this installs it where the
    /// arming trigger reads it, so moving a session and re-arming its deadline
    /// stay one statement.
    pub async fn apply_configured_bounds(
        &self,
        bounds: &SessionLifecycleNumericBounds,
    ) -> Result<(), SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        for (kind, bound) in bounds.rows() {
            sqlx::query(
                "UPDATE session_lifecycle_bound
                    SET bound = $2, updated_at = statement_timestamp()
                  WHERE deadline_kind = $1",
            )
            .bind(kind)
            .bind(bound)
            .execute(&mut *transaction)
            .await?;
        }
        commit(transaction).await
    }

    /// Loads one session's lifecycle facts.
    pub async fn load(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionLifecycleRecord>, SessionLifecycleRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_optional(&mut connection, session).await
    }

    /// Suspends a live session in place, waiting on a human.
    ///
    /// Parking terminalizes nothing and moves no turn: the turn keeps its
    /// phase, and the eligibility sweep and liveness watchdog stop treating
    /// the session as a candidate until it leaves `parked`.
    pub async fn park(
        &self,
        session: SessionId,
        cause: SessionParkCause,
        responder: SessionParkResponder,
        standing: Option<SessionFailureCause>,
        actor: LifecycleActor,
    ) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let held = load_locked(&mut transaction, session).await?;
        let parked = SessionLifecycleState::Parked {
            cause,
            responder,
            standing,
        };
        write_state(&mut transaction, &held, parked, actor).await?;
        commit(transaction).await?;
        Ok(parked)
    }

    /// Returns a parked session to the state its suspended turn maps to.
    ///
    /// The mapping is recomputed rather than remembered: the turn kept its
    /// phase through the park, so the phase is what says where the session
    /// belongs now.
    pub async fn resume(
        &self,
        session: SessionId,
        actor: LifecycleActor,
    ) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let held = load_locked(&mut transaction, session).await?;
        if !held.state.is_parked() {
            return Err(reject(
                transaction,
                SessionLifecycleRejection::TransitionNotAdmitted,
            )
            .await);
        }
        write_state(
            &mut transaction,
            &held,
            SessionLifecycleState::Active,
            actor,
        )
        .await?;
        sqlx::query("SELECT project_session_lifecycle($1)")
            .bind(session_id_to_uuid(session))
            .execute(&mut *transaction)
            .await?;
        let resumed = load_locked(&mut transaction, session).await?.state;
        commit(transaction).await?;
        Ok(resumed)
    }

    /// Closes one session with its declared outcome.
    pub async fn close(
        &self,
        session: SessionId,
        outcome: SessionTerminalOutcome,
        actor: LifecycleActor,
    ) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let terminal = close_in_transaction(&mut transaction, session, outcome, actor).await?;
        commit(transaction).await?;
        Ok(terminal)
    }

    /// Commits a session to an outcome while its live turn still settles.
    ///
    /// §2 requires the turn to settle through the committed machinery before
    /// the session records terminal. The handoff is what lets a closure say
    /// what it decided without recording a terminal session over a live turn.
    pub async fn commit_pending_terminal(
        &self,
        session: SessionId,
        outcome: SessionTerminalOutcome,
    ) -> Result<(), SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let held = load_locked(&mut transaction, session).await?;
        if !held
            .state
            .admits(&SessionLifecycleState::Terminal { outcome })
        {
            return Err(reject(
                transaction,
                SessionLifecycleRejection::TransitionNotAdmitted,
            )
            .await);
        }
        let encoded = EncodedTerminal::from_outcome(outcome);
        sqlx::query(
            "UPDATE session_lifecycle
                SET pending_terminal_outcome_kind = $2,
                    pending_terminal_cause_kind = $3,
                    pending_terminal_stop_sticky = $4,
                    pending_terminal_superseded_by = $5
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .bind(encoded.outcome)
        .bind(encoded.cause)
        .bind(encoded.sticky)
        .bind(encoded.superseded_by)
        .execute(&mut *transaction)
        .await?;
        commit(transaction).await
    }

    /// Records the outcome a closure committed to, now that the turn settled.
    pub async fn settle_pending_terminal(
        &self,
        session: SessionId,
        actor: LifecycleActor,
    ) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let held = load_locked(&mut transaction, session).await?;
        let Some(outcome) = held.pending_terminal else {
            return Err(reject(transaction, SessionLifecycleRejection::NoPendingTerminal).await);
        };
        let terminal = close_in_transaction(&mut transaction, session, outcome, actor).await?;
        commit(transaction).await?;
        Ok(terminal)
    }

    /// Takes the liveness obligation for one session.
    pub async fn adopt(
        &self,
        session: SessionId,
        actor: LifecycleActor,
    ) -> Result<(), SessionLifecycleRepositoryError> {
        self.flip_ownership(session, SessionOwnershipTransition::Adopted, actor)
            .await
    }

    /// Drops the forward-looking obligations for one session.
    ///
    /// Release never interrupts a live operation: the running turn completes
    /// to its boundary under the resources it already holds. What the flip
    /// drops immediately is the forward obligations — the armed deadline goes
    /// with the ownership bit, in the same statement.
    pub async fn release(
        &self,
        session: SessionId,
        actor: LifecycleActor,
    ) -> Result<(), SessionLifecycleRepositoryError> {
        self.flip_ownership(session, SessionOwnershipTransition::Released, actor)
            .await
    }

    async fn flip_ownership(
        &self,
        session: SessionId,
        transition: SessionOwnershipTransition,
        actor: LifecycleActor,
    ) -> Result<(), SessionLifecycleRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let held = load_locked(&mut transaction, session).await?;
        if held.ownership == transition.ownership() {
            return Err(reject(transaction, SessionLifecycleRejection::OwnershipUnchanged).await);
        }
        if held.state.is_parked() && transition == SessionOwnershipTransition::Released {
            return Err(reject(transaction, SessionLifecycleRejection::ReleaseWhileParked).await);
        }
        sqlx::query("UPDATE session_lifecycle SET owned = $2 WHERE session_id = $1")
            .bind(session_id_to_uuid(session))
            .bind(transition.ownership().is_owned())
            .execute(&mut *transaction)
            .await?;
        journal_ownership(&mut transaction, session, transition, actor).await?;
        commit(transaction).await
    }
}

/// Writes the lifecycle satellite for one newly created session.
///
/// Every creation path calls this: `session` carries a deferred foreign key to
/// the satellite, so a creation that skips it fails at commit rather than
/// leaving a session with no state.
pub(crate) async fn insert_created(
    connection: &mut PgConnection,
    session: SessionId,
    cause: &SessionCreationCause,
) -> Result<(), sqlx::Error> {
    let ownership = creation_ownership(cause);
    let actor = creation_actor(cause);
    let (actor_kind, actor_module, actor_turn, actor_request) = encode_actor(actor);
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, actor_kind, actor_module,
             actor_turn_id, actor_tool_request_id)
         VALUES ($1, 'created', $2, $3, $4, $5, $6)",
    )
    .bind(session_id_to_uuid(session))
    .bind(ownership.is_owned())
    .bind(actor_kind)
    .bind(actor_module)
    .bind(actor_turn)
    .bind(actor_request)
    .execute(&mut *connection)
    .await?;
    let transition = match ownership {
        SessionOwnership::Owned => SessionOwnershipTransition::CreatedOwned,
        SessionOwnership::Unmonitored => SessionOwnershipTransition::CreatedUnmonitored,
    };
    journal_ownership(connection, session, transition, actor).await
}

/// Returns the §6 classification of the agency that created one session.
///
/// The cause is the agency: an interactive creation is the operator's, a
/// module dispatch is that module's, and a delegated child is the exact tool
/// request that spawned it — core agency with the acting identity kept.
pub(crate) const fn creation_actor(cause: &SessionCreationCause) -> LifecycleActor {
    match cause {
        SessionCreationCause::Interactive => LifecycleActor::Operator,
        SessionCreationCause::ModuleDispatched { dispatch } => LifecycleActor::Module {
            module: dispatch.module(),
        },
        SessionCreationCause::Delegated { spawning_request } => LifecycleActor::Core {
            agency: CoreAgency::Tool {
                request: *spawning_request,
            },
        },
    }
}

/// Returns the ownership a creation cause establishes.
///
/// A dispatched or delegated session is work the daemon drives to a declared
/// outcome. An interactive creation is a conversation: §6's unmonitored bit,
/// which is what keeps a person's chat window out of deadlines, watchdogs, and
/// occupancy accounting.
pub(crate) const fn creation_ownership(cause: &SessionCreationCause) -> SessionOwnership {
    match cause {
        SessionCreationCause::Interactive => SessionOwnership::Unmonitored,
        SessionCreationCause::ModuleDispatched { .. } | SessionCreationCause::Delegated { .. } => {
            SessionOwnership::Owned
        }
    }
}

/// Closes one session inside the caller's transaction.
///
/// The goal generation settles first. A closure whose outcome the goal
/// contract already spells — a verified achievement, a stop — is refused while
/// the generation is open, because the goal command is what settles those and
/// appending a second terminal event would record one closure twice.
pub(crate) async fn close_in_transaction(
    connection: &mut PgConnection,
    session: SessionId,
    outcome: SessionTerminalOutcome,
    actor: LifecycleActor,
) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
    let held = load_locked(connection, session).await?;
    let terminal = SessionLifecycleState::Terminal { outcome };
    if !held.state.admits(&terminal) {
        return Err(SessionLifecycleRepositoryError::Rejected(
            SessionLifecycleRejection::TransitionNotAdmitted,
        ));
    }
    settle_goal(connection, session, outcome, actor).await?;
    write_state(connection, &held, terminal, actor).await?;
    record_cleanup_obligation(connection, session, outcome).await?;
    Ok(terminal)
}

async fn settle_goal(
    connection: &mut PgConnection,
    session: SessionId,
    outcome: SessionTerminalOutcome,
    actor: LifecycleActor,
) -> Result<(), SessionLifecycleRepositoryError> {
    let Some(goal) = goal::load_goal_from_connection(connection, session).await? else {
        return Ok(());
    };
    if !goal.current().state().is_open() {
        return Ok(());
    }
    let Some(closure) = outcome.closure_outcome() else {
        return Err(SessionLifecycleRepositoryError::Rejected(
            SessionLifecycleRejection::GoalGenerationStillOpen,
        ));
    };
    append_session_closure(connection, session, goal, closure, actor).await
}

async fn append_session_closure(
    connection: &mut PgConnection,
    session: SessionId,
    goal: Goal,
    closure: SessionClosureOutcome,
    actor: LifecycleActor,
) -> Result<(), SessionLifecycleRepositoryError> {
    let settled = goal.close_with_session(closure, actor).map_err(|_| {
        SessionLifecycleCorruption::Inconsistent("goal refused its session closure")
    })?;
    let event = settled
        .events()
        .last()
        .ok_or(SessionLifecycleCorruption::Inconsistent(
            "settled goal recorded no event",
        ))?;
    goal::insert_event_for_session_closure(connection, session, event).await?;
    Ok(())
}

/// Records the cleanup one outcome owes beyond the release its closure already
/// performed.
///
/// Every terminal outcome releases the session's held dispatch slot, which the
/// closure's own goal event does. Only an operator write-off leaves worktrees
/// and containers behind, so only it records an obligation for them.
async fn record_cleanup_obligation(
    connection: &mut PgConnection,
    session: SessionId,
    outcome: SessionTerminalOutcome,
) -> Result<(), SessionLifecycleRepositoryError> {
    if !outcome.records_cleanup_obligations() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO session_cleanup_obligation (session_id, outcome_kind)
         VALUES ($1, $2)
         ON CONFLICT (session_id) DO NOTHING",
    )
    .bind(session_id_to_uuid(session))
    .bind(EncodedTerminal::from_outcome(outcome).outcome)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn journal_ownership(
    connection: &mut PgConnection,
    session: SessionId,
    transition: SessionOwnershipTransition,
    actor: LifecycleActor,
) -> Result<(), sqlx::Error> {
    let (actor_kind, actor_module, _, _) = encode_actor(actor);
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after,
             actor_kind, actor_module)
         SELECT $1,
                COALESCE(MAX(event_ordinal), 0) + 1,
                $2,
                $3,
                $4,
                $5
           FROM session_ownership_event
          WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .bind(ownership_transition_to_str(transition))
    .bind(transition.ownership().is_owned())
    .bind(actor_kind)
    .bind(actor_module)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

const fn ownership_transition_to_str(value: SessionOwnershipTransition) -> &'static str {
    match value {
        SessionOwnershipTransition::CreatedOwned => "created_owned",
        SessionOwnershipTransition::CreatedUnmonitored => "created_unmonitored",
        SessionOwnershipTransition::Adopted => "adopted",
        SessionOwnershipTransition::Released => "released",
    }
}

async fn write_state(
    connection: &mut PgConnection,
    held: &SessionLifecycleRecord,
    next: SessionLifecycleState,
    actor: LifecycleActor,
) -> Result<(), SessionLifecycleRepositoryError> {
    if !held.state.admits(&next) {
        return Err(SessionLifecycleRepositoryError::Rejected(
            SessionLifecycleRejection::TransitionNotAdmitted,
        ));
    }
    let encoded = EncodedState::from_state(next, actor);
    sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = $2,
                state_entered_at = statement_timestamp(),
                actor_kind = $3,
                actor_module = $4,
                actor_turn_id = $5,
                actor_tool_request_id = $6,
                waiting_kind = $7,
                waiting_waker = $8,
                waiting_subject_session_id = $9,
                recovering_op = $10,
                blocked_reason = $11,
                blocked_cycle = $12,
                parked_cause = $13,
                parked_responder = $14,
                parked_since = $15,
                -- §12 keeps a supersession that closed a park holding a
                -- failure cause in both the denominator and the numerator,
                -- under that standing cause. The cause has to outlive the park
                -- to be read at all, so terminalization carries it forward
                -- rather than clearing it with the rest of the park detail.
                parked_standing_cause_kind = CASE
                    WHEN $2 = 'terminal'
                        THEN session_lifecycle.parked_standing_cause_kind
                    ELSE $16
                END,
                ended_at = $17,
                terminal_outcome_kind = $18,
                terminal_cause_kind = $19,
                terminal_stop_sticky = $20,
                terminal_superseded_by = $21,
                pending_terminal_outcome_kind = NULL,
                pending_terminal_cause_kind = NULL,
                pending_terminal_stop_sticky = NULL,
                pending_terminal_superseded_by = NULL
          WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(held.session))
    .bind(encoded.state)
    .bind(encoded.actor_kind)
    .bind(encoded.actor_module)
    .bind(encoded.actor_turn)
    .bind(encoded.actor_request)
    .bind(encoded.waiting_kind)
    .bind(encoded.waiting_waker)
    .bind(encoded.waiting_subject)
    .bind(encoded.recovering_op)
    .bind(encoded.blocked_reason)
    .bind(encoded.blocked_cycle)
    .bind(encoded.parked_cause)
    .bind(encoded.parked_responder)
    .bind(encoded.parked_since)
    .bind(encoded.parked_standing)
    .bind(encoded.ended)
    .bind(encoded.terminal.outcome)
    .bind(encoded.terminal.cause)
    .bind(encoded.terminal.sticky)
    .bind(encoded.terminal.superseded_by)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_locked(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<SessionLifecycleRecord, SessionLifecycleRepositoryError> {
    sqlx::query(lock_inventory::SESSION_LIFECYCLE_SESSION)
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SessionLifecycleRepositoryError::UnknownSession(session))?;
    sqlx::query(lock_inventory::SESSION_LIFECYCLE_SATELLITE)
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SessionLifecycleRepositoryError::UnknownSession(session))?;
    load_optional(connection, session)
        .await?
        .ok_or(SessionLifecycleRepositoryError::UnknownSession(session))
}

async fn load_optional(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<SessionLifecycleRecord>, SessionLifecycleRepositoryError> {
    let row = sqlx::query(
        "SELECT session_id, state_kind, owned, actor_kind, actor_module,
                actor_turn_id, actor_tool_request_id, waiting_kind,
                waiting_subject_session_id, recovering_op, blocked_reason,
                blocked_cycle, parked_cause, parked_responder,
                parked_standing_cause_kind, terminal_outcome_kind,
                terminal_cause_kind, terminal_stop_sticky,
                terminal_superseded_by, pending_terminal_outcome_kind,
                pending_terminal_cause_kind, pending_terminal_stop_sticky,
                pending_terminal_superseded_by
           FROM session_lifecycle
          WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    row.as_ref().map(decode_record).transpose()
}

async fn commit(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), SessionLifecycleRepositoryError> {
    transaction.commit().await.map_err(|error| {
        if crate::commit_failure_is_ambiguous(&error) {
            SessionLifecycleRepositoryError::CommitAmbiguous(error)
        } else {
            SessionLifecycleRepositoryError::Database(error)
        }
    })
}

async fn reject(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    rejection: SessionLifecycleRejection,
) -> SessionLifecycleRepositoryError {
    match transaction.rollback().await {
        Ok(()) => SessionLifecycleRepositoryError::Rejected(rejection),
        Err(error) => SessionLifecycleRepositoryError::Database(error),
    }
}

struct EncodedTerminal {
    outcome: Option<&'static str>,
    cause: Option<&'static str>,
    sticky: Option<bool>,
    superseded_by: Option<Uuid>,
}

impl EncodedTerminal {
    const fn empty() -> Self {
        Self {
            outcome: None,
            cause: None,
            sticky: None,
            superseded_by: None,
        }
    }

    fn from_outcome(outcome: SessionTerminalOutcome) -> Self {
        match outcome {
            SessionTerminalOutcome::AchievedVerified => Self {
                outcome: Some("achieved_verified"),
                ..Self::empty()
            },
            SessionTerminalOutcome::FailedRetryable { cause } => Self {
                outcome: Some("failed_retryable"),
                cause: Some(crate::mapping::session_retryable_cause_to_str(cause)),
                ..Self::empty()
            },
            SessionTerminalOutcome::FailedStructural { cause } => Self {
                outcome: Some("failed_structural"),
                cause: Some(crate::mapping::session_structural_cause_to_str(cause)),
                ..Self::empty()
            },
            SessionTerminalOutcome::FailedUnknown => Self {
                outcome: Some("failed_unknown"),
                ..Self::empty()
            },
            SessionTerminalOutcome::Stopped { sticky } => Self {
                outcome: Some("stopped"),
                sticky: Some(matches!(sticky, StopStickiness::Sticky)),
                ..Self::empty()
            },
            SessionTerminalOutcome::Superseded { by } => Self {
                outcome: Some("superseded"),
                superseded_by: by.map(session_id_to_uuid),
                ..Self::empty()
            },
            SessionTerminalOutcome::Abandoned => Self {
                outcome: Some("abandoned"),
                ..Self::empty()
            },
            SessionTerminalOutcome::Retired { cause } => Self {
                outcome: Some("retired"),
                cause: Some(crate::mapping::session_retirement_cause_to_str(cause)),
                ..Self::empty()
            },
        }
    }
}

struct EncodedState {
    state: &'static str,
    waiting_kind: Option<&'static str>,
    waiting_waker: Option<&'static str>,
    waiting_subject: Option<Uuid>,
    recovering_op: Option<&'static str>,
    blocked_reason: Option<&'static str>,
    blocked_cycle: Option<i64>,
    actor_kind: &'static str,
    actor_module: Option<&'static str>,
    actor_turn: Option<Uuid>,
    actor_request: Option<Uuid>,
    parked_cause: Option<&'static str>,
    parked_responder: Option<&'static str>,
    parked_since: Option<sqlx::types::time::OffsetDateTime>,
    parked_standing: Option<&'static str>,
    ended: Option<sqlx::types::time::OffsetDateTime>,
    terminal: EncodedTerminal,
}

impl EncodedState {
    fn from_state(state: SessionLifecycleState, actor: LifecycleActor) -> Self {
        let (actor_kind, actor_module, actor_turn, actor_request) = encode_actor(actor);
        let now = sqlx::types::time::OffsetDateTime::now_utc();
        let mut encoded = Self {
            state: crate::mapping::session_lifecycle_state_to_str(&state),
            waiting_kind: None,
            waiting_waker: None,
            waiting_subject: None,
            recovering_op: None,
            blocked_reason: None,
            blocked_cycle: None,
            actor_kind,
            actor_module,
            actor_turn,
            actor_request,
            parked_cause: None,
            parked_responder: None,
            parked_since: None,
            parked_standing: None,
            ended: None,
            terminal: EncodedTerminal::empty(),
        };
        match state {
            SessionLifecycleState::Waiting { wait } => {
                encoded.waiting_kind = Some(session_wait_kind_to_str(wait.kind()));
                encoded.waiting_waker = Some(session_waker_to_str(wait.waker()));
                encoded.waiting_subject = match wait {
                    SessionWait::Child { session } => Some(session_id_to_uuid(session)),
                    SessionWait::Approval
                    | SessionWait::External
                    | SessionWait::ProviderRetry
                    | SessionWait::Pipeline
                    | SessionWait::Scheduler => None,
                };
            }
            SessionLifecycleState::Recovering { operation } => {
                encoded.recovering_op = Some(session_recovery_operation_to_str(operation));
            }
            SessionLifecycleState::Blocked { reason, cycle } => {
                encoded.blocked_reason = Some(goal_blocked_reason_to_str(reason));
                encoded.blocked_cycle = Some(cycle_to_stored(cycle));
            }
            SessionLifecycleState::Parked {
                cause,
                responder,
                standing,
            } => {
                encoded.parked_cause = Some(crate::mapping::session_park_cause_to_str(cause));
                encoded.parked_responder = Some(park_responder_to_str(responder));
                encoded.parked_since = Some(now);
                encoded.parked_standing = standing.map(failure_cause_to_str);
            }
            SessionLifecycleState::Terminal { outcome } => {
                encoded.ended = Some(now);
                encoded.terminal = EncodedTerminal::from_outcome(outcome);
            }
            SessionLifecycleState::Created
            | SessionLifecycleState::Dispatched
            | SessionLifecycleState::Active => {}
        }
        encoded
    }
}

const fn park_responder_to_str(responder: SessionParkResponder) -> &'static str {
    match responder {
        SessionParkResponder::Operator => "operator",
        SessionParkResponder::Module { module } => dispatching_module_to_str(module),
    }
}

const fn failure_cause_to_str(cause: SessionFailureCause) -> &'static str {
    match cause {
        SessionFailureCause::Retryable(cause) => {
            crate::mapping::session_retryable_cause_to_str(cause)
        }
        SessionFailureCause::Structural(cause) => {
            crate::mapping::session_structural_cause_to_str(cause)
        }
    }
}

fn encode_actor(
    actor: LifecycleActor,
) -> (
    &'static str,
    Option<&'static str>,
    Option<Uuid>,
    Option<Uuid>,
) {
    let kind = lifecycle_actor_to_str(actor);
    match actor {
        LifecycleActor::Core {
            agency: CoreAgency::Model { turn },
        } => (
            kind,
            None,
            Some(crate::mapping::turn_id_to_uuid(turn)),
            None,
        ),
        LifecycleActor::Core {
            agency: CoreAgency::Tool { request },
        } => (
            kind,
            None,
            None,
            Some(crate::mapping::tool_request_id_to_uuid(request)),
        ),
        LifecycleActor::Module { module } => {
            (kind, Some(dispatching_module_to_str(module)), None, None)
        }
        LifecycleActor::Core {
            agency: CoreAgency::Daemon,
        }
        | LifecycleActor::Operator
        | LifecycleActor::Watchdog => (kind, None, None, None),
    }
}

fn decode_record(row: &PgRow) -> Result<SessionLifecycleRecord, SessionLifecycleRepositoryError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let owned: bool = required(row, "owned")?;
    let pending_terminal = decode_terminal_outcome(
        row.try_get("pending_terminal_outcome_kind")?,
        row.try_get("pending_terminal_cause_kind")?,
        row.try_get("pending_terminal_stop_sticky")?,
        row.try_get("pending_terminal_superseded_by")?,
    )?;
    Ok(SessionLifecycleRecord {
        session,
        state: decode_state(row)?,
        ownership: if owned {
            SessionOwnership::Owned
        } else {
            SessionOwnership::Unmonitored
        },
        actor: decode_actor(
            required::<String>(row, "actor_kind")?,
            row.try_get("actor_module")?,
            row.try_get("actor_turn_id")?,
            row.try_get("actor_tool_request_id")?,
        )?,
        pending_terminal,
    })
}

/// Rebuilds the state and the typed detail its own shape constraint pairs it
/// with, so a row whose detail belongs to another state is corrupt rather than
/// silently read as a bare state.
fn decode_state(row: &PgRow) -> Result<SessionLifecycleState, SessionLifecycleRepositoryError> {
    let state: String = required(row, "state_kind")?;
    match state.as_str() {
        "created" => Ok(SessionLifecycleState::Created),
        "dispatched" => Ok(SessionLifecycleState::Dispatched),
        "active" => Ok(SessionLifecycleState::Active),
        "waiting" => Ok(SessionLifecycleState::Waiting {
            wait: decode_wait(
                &required::<String>(row, "waiting_kind")?,
                row.try_get("waiting_subject_session_id")?,
            )?,
        }),
        "recovering" => Ok(SessionLifecycleState::Recovering {
            operation: session_recovery_operation_from_str(&required::<String>(
                row,
                "recovering_op",
            )?)
            .ok_or(SessionLifecycleCorruption::Inconsistent(
                "recovering operation",
            ))?,
        }),
        "blocked" => Ok(SessionLifecycleState::Blocked {
            reason: goal_blocked_reason_from_str(&required::<String>(row, "blocked_reason")?)
                .ok_or(SessionLifecycleCorruption::Inconsistent("blocked reason"))?,
            cycle: cycle_from_stored(required::<i64>(row, "blocked_cycle")?)?,
        }),
        "parked" => Ok(SessionLifecycleState::Parked {
            cause: session_park_cause_from_str(&required::<String>(row, "parked_cause")?)
                .ok_or(SessionLifecycleCorruption::Inconsistent("park cause"))?,
            responder: decode_park_responder(&required::<String>(row, "parked_responder")?)?,
            standing: row
                .try_get::<Option<String>, _>("parked_standing_cause_kind")?
                .map(|cause| decode_failure_cause(&cause))
                .transpose()?,
        }),
        "terminal" => Ok(SessionLifecycleState::Terminal {
            outcome: decode_terminal_outcome(
                row.try_get("terminal_outcome_kind")?,
                row.try_get("terminal_cause_kind")?,
                row.try_get("terminal_stop_sticky")?,
                row.try_get("terminal_superseded_by")?,
            )?
            .ok_or(SessionLifecycleCorruption::Inconsistent("terminal outcome"))?,
        }),
        _ => Err(SessionLifecycleCorruption::Unsupported {
            field: "state",
            value: state,
        }
        .into()),
    }
}

fn decode_wait(
    kind: &str,
    subject: Option<Uuid>,
) -> Result<SessionWait, SessionLifecycleRepositoryError> {
    match (session_wait_kind_from_str(kind), subject) {
        (Some(SessionWaitKind::Approval), None) => Ok(SessionWait::Approval),
        (Some(SessionWaitKind::External), None) => Ok(SessionWait::External),
        (Some(SessionWaitKind::Child), Some(child)) => Ok(SessionWait::Child {
            session: session_id_from_uuid(child),
        }),
        (Some(SessionWaitKind::ProviderRetry), None) => Ok(SessionWait::ProviderRetry),
        (Some(SessionWaitKind::Pipeline), None) => Ok(SessionWait::Pipeline),
        (Some(SessionWaitKind::Scheduler), None) => Ok(SessionWait::Scheduler),
        (Some(_), _) => Err(SessionLifecycleCorruption::Inconsistent("waiting subject").into()),
        (None, _) => Err(SessionLifecycleCorruption::Unsupported {
            field: "waiting kind",
            value: String::from(kind),
        }
        .into()),
    }
}

fn decode_park_responder(
    responder: &str,
) -> Result<SessionParkResponder, SessionLifecycleRepositoryError> {
    match responder {
        "operator" => Ok(SessionParkResponder::Operator),
        module => crate::mapping::dispatching_module_from_str(module)
            .map(|module| SessionParkResponder::Module { module })
            .ok_or_else(|| {
                SessionLifecycleCorruption::Unsupported {
                    field: "park responder",
                    value: String::from(responder),
                }
                .into()
            }),
    }
}

fn decode_failure_cause(
    cause: &str,
) -> Result<SessionFailureCause, SessionLifecycleRepositoryError> {
    session_retryable_cause_from_str(cause)
        .map(SessionFailureCause::Retryable)
        .or_else(|| session_structural_cause_from_str(cause).map(SessionFailureCause::Structural))
        .ok_or_else(|| {
            SessionLifecycleCorruption::Unsupported {
                field: "standing failure cause",
                value: String::from(cause),
            }
            .into()
        })
}

/// Rebuilds one outcome and the exact member §2 gives it.
///
/// Outcome and member are decoded together: a stored `failed_structural` whose
/// cause spelling belongs to the retryable set is corrupt, not a structural
/// failure with a surprising cause.
fn decode_terminal_outcome(
    outcome: Option<String>,
    cause: Option<String>,
    sticky: Option<bool>,
    superseded_by: Option<Uuid>,
) -> Result<Option<SessionTerminalOutcome>, SessionLifecycleRepositoryError> {
    let Some(outcome) = outcome else {
        return Ok(None);
    };
    let decoded = match (outcome.as_str(), cause, sticky, superseded_by) {
        ("achieved_verified", None, None, None) => SessionTerminalOutcome::AchievedVerified,
        ("failed_retryable", Some(cause), None, None) => SessionTerminalOutcome::FailedRetryable {
            cause: session_retryable_cause_from_str(&cause).ok_or(
                SessionLifecycleCorruption::Inconsistent("retryable failure cause"),
            )?,
        },
        ("failed_structural", Some(cause), None, None) => {
            SessionTerminalOutcome::FailedStructural {
                cause: session_structural_cause_from_str(&cause).ok_or(
                    SessionLifecycleCorruption::Inconsistent("structural failure cause"),
                )?,
            }
        }
        ("failed_unknown", None, None, None) => SessionTerminalOutcome::FailedUnknown,
        ("stopped", None, Some(sticky), None) => SessionTerminalOutcome::Stopped {
            sticky: if sticky {
                StopStickiness::Sticky
            } else {
                StopStickiness::Redispatchable
            },
        },
        ("superseded", None, None, successor) => SessionTerminalOutcome::Superseded {
            by: successor.map(session_id_from_uuid),
        },
        ("abandoned", None, None, None) => SessionTerminalOutcome::Abandoned,
        ("retired", Some(cause), None, None) => SessionTerminalOutcome::Retired {
            cause: session_retirement_cause_from_str(&cause)
                .ok_or(SessionLifecycleCorruption::Inconsistent("retirement cause"))?,
        },
        (
            "achieved_verified" | "failed_retryable" | "failed_structural" | "failed_unknown"
            | "stopped" | "superseded" | "abandoned" | "retired",
            _,
            _,
            _,
        ) => {
            return Err(SessionLifecycleCorruption::Inconsistent("terminal outcome shape").into());
        }
        _ => {
            return Err(SessionLifecycleCorruption::Unsupported {
                field: "terminal outcome",
                value: outcome,
            }
            .into());
        }
    };
    Ok(Some(decoded))
}

/// Stores a resume-cycle count, saturating at the column's own ceiling rather
/// than wrapping into a negative one.
const fn cycle_to_stored(cycle: u64) -> i64 {
    if cycle > i64::MAX as u64 {
        i64::MAX
    } else {
        cycle as i64
    }
}

fn cycle_from_stored(cycle: i64) -> Result<u64, SessionLifecycleRepositoryError> {
    u64::try_from(cycle)
        .map_err(|_| SessionLifecycleCorruption::Inconsistent("blocked cycle").into())
}

/// Rebuilds the §6 classification and the exact agency behind a core one.
fn decode_actor(
    kind: String,
    module: Option<String>,
    turn: Option<Uuid>,
    request: Option<Uuid>,
) -> Result<LifecycleActor, SessionLifecycleRepositoryError> {
    match (kind.as_str(), module, turn, request) {
        ("core", None, None, None) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Daemon,
        }),
        ("core", None, Some(turn), None) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Model {
                turn: crate::mapping::turn_id_from_uuid(turn),
            },
        }),
        ("core", None, None, Some(request)) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Tool {
                request: crate::mapping::tool_request_id_from_uuid(request),
            },
        }),
        ("operator", None, None, None) => Ok(LifecycleActor::Operator),
        ("watchdog", None, None, None) => Ok(LifecycleActor::Watchdog),
        ("module", Some(module), None, None) => {
            crate::mapping::dispatching_module_from_str(&module)
                .map(|module| LifecycleActor::Module { module })
                .ok_or_else(|| {
                    SessionLifecycleCorruption::Unsupported {
                        field: "actor module",
                        value: module,
                    }
                    .into()
                })
        }
        ("core" | "operator" | "watchdog" | "module", _, _, _) => {
            Err(SessionLifecycleCorruption::Inconsistent("actor provenance").into())
        }
        _ => Err(SessionLifecycleCorruption::Unsupported {
            field: "actor",
            value: kind,
        }
        .into()),
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionLifecycleRepositoryError>
where
    T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SessionLifecycleCorruption::Missing(field).into())
}
