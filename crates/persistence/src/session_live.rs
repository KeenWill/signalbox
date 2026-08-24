//! PostgreSQL adapter for the bounded current-session projection.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    SessionLiveActiveState, SessionLiveActiveTurn, SessionLiveReader, SessionLiveReconciliation,
    SessionLiveRunner, SessionLiveRunnerConnectionHealth, SessionLiveRunnerState,
    SessionLiveSnapshot, max_session_live_queued_turns,
};
use signalbox_domain::{ModelCallId, RunnerId, SessionId, ToolAttemptId, ToolRequestId, TurnId};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

use crate::process_read::{
    ProcessReadError, ProcessRunnerConnectionHealth, ProcessRunnerProjection,
    ProcessRunnerProjectionState, load_process_runner_projection,
};

/// Database or fail-closed live-projection failure.
#[derive(Debug)]
pub enum SessionLiveRepositoryError {
    Database(sqlx::Error),
    Process(ProcessReadError),
    Corruption(&'static str),
    Unsupported { field: &'static str, value: String },
}

impl fmt::Display for SessionLiveRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "session live database failure: {error}"),
            Self::Process(error) => error.fmt(formatter),
            Self::Corruption(field) => write!(formatter, "invalid session live {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported session live {field}: {value}")
            }
        }
    }
}

impl Error for SessionLiveRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Process(error) => Some(error),
            Self::Corruption(_) | Self::Unsupported { .. } => None,
        }
    }
}

impl From<sqlx::Error> for SessionLiveRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProcessReadError> for SessionLiveRepositoryError {
    fn from(error: ProcessReadError) -> Self {
        Self::Process(error)
    }
}

/// PostgreSQL implementation of the current-session read port.
#[derive(Clone, Debug)]
pub struct SessionLiveRepository {
    pool: PgPool,
}

impl SessionLiveRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn read_live_snapshot(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionLiveSnapshot>, SessionLiveRepositoryError> {
        self.read_live_snapshot_at_completion(session, || ())
            .await
            .map(|result| result.map(|(snapshot, ())| snapshot))
    }

    /// Reads one snapshot and samples caller-owned state immediately after its
    /// repeatable-read transaction completes.
    pub async fn read_live_snapshot_at_completion<T>(
        &self,
        session: SessionId,
        at_completion: impl FnOnce() -> T,
    ) -> Result<Option<(SessionLiveSnapshot, T)>, SessionLiveRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let base = sqlx::query(
            "SELECT state.last_sequence, facts.queued_turn_count
               FROM session
               LEFT JOIN session_timeline_fact AS facts USING (session_id)
               LEFT JOIN outbox_sequence_state AS state ON state.singleton
              WHERE session.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(base) = base else {
            transaction.commit().await?;
            return Ok(None);
        };
        let observed_through = required_decimal(&base, "last_sequence")
            .and_then(|value| decimal_u64(value, "outbox cursor"))?;
        let queued_turn_count = required_decimal(&base, "queued_turn_count")
            .and_then(|value| decimal_u64(value, "queued turn count"))?;
        let active_rows = sqlx::query(ACTIVE_TURN_SQL)
            .bind(session.into_uuid())
            .fetch_all(&mut *transaction)
            .await?;
        if active_rows.len() > 1 {
            return Err(SessionLiveRepositoryError::Corruption(
                "active turn cardinality",
            ));
        }
        let active = active_rows.first().map(decode_active).transpose()?;
        let queued_rows = sqlx::query(QUEUED_PREVIEW_SQL)
            .bind(session.into_uuid())
            .bind(i64::from(max_session_live_queued_turns()))
            .fetch_all(&mut *transaction)
            .await?;
        let queued_turns = queued_rows
            .iter()
            .map(|row| row.try_get::<Uuid, _>("turn_id").map(TurnId::from_uuid))
            .collect::<Result<Vec<_>, _>>()?;
        let expected_queued_turns =
            usize::try_from(queued_turn_count.min(u64::from(max_session_live_queued_turns())))
                .map_err(|_| SessionLiveRepositoryError::Corruption("queued turn count"))?;
        if queued_turns.len() != expected_queued_turns {
            return Err(SessionLiveRepositoryError::Corruption("queued turn count"));
        }
        let reconciliation = if active.is_none() {
            sqlx::query(RECONCILIATION_SQL)
                .bind(session.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?
                .as_ref()
                .map(decode_latest_reconciliation)
                .transpose()?
                .flatten()
        } else {
            None
        };
        let runner = load_process_runner_projection(&mut transaction, session)
            .await?
            .as_ref()
            .map(map_runner)
            .transpose()?;
        let snapshot = SessionLiveSnapshot {
            session,
            observed_through,
            active,
            queued_turn_count,
            queued_turns,
            reconciliation,
            runner,
        };
        let completion = at_completion();
        transaction.commit().await?;
        Ok(Some((snapshot, completion)))
    }
}

impl SessionLiveReader for SessionLiveRepository {
    type Error = SessionLiveRepositoryError;

    async fn read_live_snapshot(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionLiveSnapshot>, Self::Error> {
        SessionLiveRepository::read_live_snapshot(self, session).await
    }
}

const ACTIVE_TURN_SQL: &str = r#"
SELECT lifecycle.turn_id, lifecycle.active_phase_kind,
       lifecycle.recovery_model_call_id, lifecycle.approval_tool_request_id,
       lifecycle.child_wait_request_id, child_wait.child_session_id,
       lifecycle.recovery_tool_attempt_id, lifecycle.runner_recovery_runner_id,
       lifecycle.runner_recovery_placement_revision, current_call.model_call_id
  FROM turn_lifecycle AS lifecycle
  LEFT JOIN session_delegation_wait AS child_wait
    ON child_wait.awaiting_tool_request_id = lifecycle.child_wait_request_id
   AND child_wait.parent_turn_id = lifecycle.turn_id
   AND child_wait.parent_session_id = lifecycle.session_id
   AND child_wait.wait_mode = 'foreground'
  LEFT JOIN model_call AS current_call
    ON current_call.turn_attempt_id = lifecycle.current_attempt_id
   AND current_call.turn_id = lifecycle.turn_id
   AND current_call.session_id = lifecycle.session_id
   AND current_call.state_kind <> 'terminal'
 WHERE lifecycle.session_id = $1
   AND lifecycle.state_kind = 'active'
   AND goal_turn_is_runtime_relevant(lifecycle.session_id, lifecycle.turn_id)
 ORDER BY lifecycle.acceptance_position
 LIMIT 2
"#;

const QUEUED_PREVIEW_SQL: &str = r#"
SELECT turn_id
  FROM session_live_queued_turn
 WHERE session_id = $1
 ORDER BY acceptance_position
 LIMIT $2
"#;

const RECONCILIATION_SQL: &str = r#"
SELECT turn_id, state_kind, terminal_disposition_kind,
       terminal_model_call_id, terminal_tool_attempt_id
  FROM turn_lifecycle
 WHERE session_id = $1
   AND goal_turn_is_runtime_relevant(session_id, turn_id)
 ORDER BY acceptance_position DESC
 LIMIT 1
"#;

fn decode_active(row: &PgRow) -> Result<SessionLiveActiveTurn, SessionLiveRepositoryError> {
    let turn = TurnId::from_uuid(row.try_get("turn_id")?);
    let phase: String = row.try_get("active_phase_kind")?;
    let shape = (
        phase.as_str(),
        row.try_get::<Option<Uuid>, _>("model_call_id")?,
        row.try_get::<Option<Uuid>, _>("recovery_model_call_id")?,
        row.try_get::<Option<Uuid>, _>("approval_tool_request_id")?,
        row.try_get::<Option<Uuid>, _>("child_wait_request_id")?,
        row.try_get::<Option<Uuid>, _>("child_session_id")?,
        row.try_get::<Option<Uuid>, _>("recovery_tool_attempt_id")?,
        row.try_get::<Option<Uuid>, _>("runner_recovery_runner_id")?,
        row.try_get::<Option<Decimal>, _>("runner_recovery_placement_revision")?,
    );
    let state = match shape {
        ("running", model_call, None, None, None, None, None, None, None) => {
            SessionLiveActiveState::Running {
                model_call: model_call.map(ModelCallId::from_uuid),
            }
        }
        ("awaiting_model_call_recovery", None, Some(call), None, None, None, None, None, None) => {
            SessionLiveActiveState::AwaitingModelCallRecovery {
                call: ModelCallId::from_uuid(call),
            }
        }
        ("awaiting_tool_approval", None, None, Some(request), None, None, None, None, None) => {
            SessionLiveActiveState::AwaitingToolApproval {
                request: ToolRequestId::from_uuid(request),
            }
        }
        ("awaiting_child", None, None, None, Some(request), Some(child), None, None, None) => {
            SessionLiveActiveState::AwaitingChild {
                request: ToolRequestId::from_uuid(request),
                child: SessionId::from_uuid(child),
            }
        }
        ("awaiting_tool_recovery", None, None, None, None, None, Some(attempt), None, None) => {
            SessionLiveActiveState::AwaitingToolRecovery {
                attempt: ToolAttemptId::from_uuid(attempt),
            }
        }
        (
            "awaiting_runner_recovery",
            None,
            None,
            None,
            None,
            None,
            None,
            Some(runner),
            Some(revision),
        ) => SessionLiveActiveState::AwaitingRunnerRecovery {
            runner: RunnerId::from_uuid(runner),
            placement_revision: decimal_u64(revision, "runner recovery placement revision")?,
        },
        (
            "running"
            | "awaiting_model_call_recovery"
            | "awaiting_tool_approval"
            | "awaiting_child"
            | "awaiting_tool_recovery"
            | "awaiting_runner_recovery",
            ..,
        ) => return Err(SessionLiveRepositoryError::Corruption("active state shape")),
        value => {
            return Err(SessionLiveRepositoryError::Unsupported {
                field: "active phase",
                value: value.0.to_owned(),
            });
        }
    };
    Ok(SessionLiveActiveTurn { turn, state })
}

fn decode_latest_reconciliation(
    row: &PgRow,
) -> Result<Option<SessionLiveReconciliation>, SessionLiveRepositoryError> {
    let state: String = row.try_get("state_kind")?;
    let disposition = row.try_get::<Option<String>, _>("terminal_disposition_kind")?;
    if state != "terminal" || disposition.as_deref() != Some("reconciliation_required") {
        return Ok(None);
    }
    let turn = TurnId::from_uuid(row.try_get("turn_id")?);
    let call = row.try_get::<Option<Uuid>, _>("terminal_model_call_id")?;
    let attempt = row.try_get::<Option<Uuid>, _>("terminal_tool_attempt_id")?;
    match (call, attempt) {
        (Some(call), None) => Ok(Some(SessionLiveReconciliation::ModelCall {
            turn,
            call: ModelCallId::from_uuid(call),
        })),
        (None, Some(attempt)) => Ok(Some(SessionLiveReconciliation::ToolAttempt {
            turn,
            attempt: ToolAttemptId::from_uuid(attempt),
        })),
        _ => Err(SessionLiveRepositoryError::Corruption(
            "reconciliation operation",
        )),
    }
}

fn map_runner(
    runner: &ProcessRunnerProjection,
) -> Result<SessionLiveRunner, SessionLiveRepositoryError> {
    let state = match runner.state() {
        ProcessRunnerProjectionState::Unpinned => SessionLiveRunnerState::Unpinned,
        ProcessRunnerProjectionState::Pinned => SessionLiveRunnerState::Pinned,
        ProcessRunnerProjectionState::RunnerLostBeforePin => {
            SessionLiveRunnerState::RunnerLostBeforePin
        }
        ProcessRunnerProjectionState::RunnerLost => SessionLiveRunnerState::RunnerLost,
        ProcessRunnerProjectionState::RunnerAbandoned => SessionLiveRunnerState::RunnerAbandoned,
    };
    let connection_health = runner.connection_health().map(|health| match health {
        ProcessRunnerConnectionHealth::Connected => SessionLiveRunnerConnectionHealth::Connected,
        ProcessRunnerConnectionHealth::Suspect => SessionLiveRunnerConnectionHealth::Suspect,
        ProcessRunnerConnectionHealth::Shutdown => SessionLiveRunnerConnectionHealth::Shutdown,
        ProcessRunnerConnectionHealth::Lost => SessionLiveRunnerConnectionHealth::Lost,
    });
    Ok(SessionLiveRunner {
        runner: runner.runner(),
        placement_revision: runner.placement_revision().get(),
        state,
        connection_health,
    })
}

fn required_decimal(
    row: &PgRow,
    field: &'static str,
) -> Result<Decimal, SessionLiveRepositoryError> {
    row.try_get::<Option<Decimal>, _>(field)?
        .ok_or(SessionLiveRepositoryError::Corruption(field))
}

fn decimal_u64(value: Decimal, field: &'static str) -> Result<u64, SessionLiveRepositoryError> {
    u64::try_from(value).map_err(|_| SessionLiveRepositoryError::Corruption(field))
}
