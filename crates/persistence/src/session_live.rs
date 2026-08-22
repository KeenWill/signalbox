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
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let base = sqlx::query(
            "SELECT state.last_sequence, facts.queued_turn_count
               FROM session
               JOIN session_timeline_fact AS facts USING (session_id)
               JOIN outbox_sequence_state AS state ON state.singleton
              WHERE session.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(base) = base else {
            transaction.commit().await?;
            return Ok(None);
        };
        let observed_through = decimal_u64(base.try_get("last_sequence")?, "outbox cursor")?;
        let queued_turn_count =
            decimal_u64(base.try_get("queued_turn_count")?, "queued turn count")?;
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
        let queued_rows = sqlx::query(
            "SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1
                AND state_kind = 'queued'
                AND goal_turn_is_runtime_relevant(session_id, turn_id)
              ORDER BY acceptance_position
              LIMIT $2",
        )
        .bind(session.into_uuid())
        .bind(i64::from(max_session_live_queued_turns()))
        .fetch_all(&mut *transaction)
        .await?;
        let queued_turns = queued_rows
            .iter()
            .map(|row| row.try_get::<Uuid, _>("turn_id").map(TurnId::from_uuid))
            .collect::<Result<Vec<_>, _>>()?;
        if u64::try_from(queued_turns.len()).unwrap_or(u64::MAX) > queued_turn_count {
            return Err(SessionLiveRepositoryError::Corruption("queued turn count"));
        }
        let reconciliation = if active.is_none() {
            sqlx::query(RECONCILIATION_SQL)
                .bind(session.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?
                .as_ref()
                .map(decode_reconciliation)
                .transpose()?
        } else {
            None
        };
        let runner = load_process_runner_projection(&mut transaction, session)
            .await?
            .as_ref()
            .map(map_runner)
            .transpose()?;
        transaction.commit().await?;
        Ok(Some(SessionLiveSnapshot {
            session,
            observed_through,
            active,
            queued_turn_count,
            queued_turns,
            reconciliation,
            runner,
        }))
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

const RECONCILIATION_SQL: &str = r#"
SELECT turn_id, terminal_model_call_id, terminal_tool_attempt_id
  FROM turn_lifecycle
 WHERE session_id = $1
   AND state_kind = 'terminal'
   AND terminal_disposition_kind = 'reconciliation_required'
   AND goal_turn_is_runtime_relevant(session_id, turn_id)
 ORDER BY acceptance_position DESC
 LIMIT 1
"#;

fn decode_active(row: &PgRow) -> Result<SessionLiveActiveTurn, SessionLiveRepositoryError> {
    let turn = TurnId::from_uuid(row.try_get("turn_id")?);
    let phase: String = row.try_get("active_phase_kind")?;
    let state = match phase.as_str() {
        "running" => SessionLiveActiveState::Running {
            model_call: row
                .try_get::<Option<Uuid>, _>("model_call_id")?
                .map(ModelCallId::from_uuid),
        },
        "awaiting_model_call_recovery" => SessionLiveActiveState::AwaitingModelCallRecovery {
            call: required_uuid(row, "recovery_model_call_id").map(ModelCallId::from_uuid)?,
        },
        "awaiting_tool_approval" => SessionLiveActiveState::AwaitingToolApproval {
            request: required_uuid(row, "approval_tool_request_id")
                .map(ToolRequestId::from_uuid)?,
        },
        "awaiting_child" => SessionLiveActiveState::AwaitingChild {
            request: required_uuid(row, "child_wait_request_id").map(ToolRequestId::from_uuid)?,
            child: required_uuid(row, "child_session_id").map(SessionId::from_uuid)?,
        },
        "awaiting_tool_recovery" => SessionLiveActiveState::AwaitingToolRecovery {
            attempt: required_uuid(row, "recovery_tool_attempt_id")
                .map(ToolAttemptId::from_uuid)?,
        },
        "awaiting_runner_recovery" => SessionLiveActiveState::AwaitingRunnerRecovery {
            runner: required_uuid(row, "runner_recovery_runner_id").map(RunnerId::from_uuid)?,
            placement_revision: decimal_u64(
                row.try_get("runner_recovery_placement_revision")?,
                "runner recovery placement revision",
            )?,
        },
        value => {
            return Err(SessionLiveRepositoryError::Unsupported {
                field: "active phase",
                value: value.to_owned(),
            });
        }
    };
    Ok(SessionLiveActiveTurn { turn, state })
}

fn decode_reconciliation(
    row: &PgRow,
) -> Result<SessionLiveReconciliation, SessionLiveRepositoryError> {
    let turn = TurnId::from_uuid(row.try_get("turn_id")?);
    let call = row.try_get::<Option<Uuid>, _>("terminal_model_call_id")?;
    let attempt = row.try_get::<Option<Uuid>, _>("terminal_tool_attempt_id")?;
    match (call, attempt) {
        (Some(call), None) => Ok(SessionLiveReconciliation::ModelCall {
            turn,
            call: ModelCallId::from_uuid(call),
        }),
        (None, Some(attempt)) => Ok(SessionLiveReconciliation::ToolAttempt {
            turn,
            attempt: ToolAttemptId::from_uuid(attempt),
        }),
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

fn required_uuid(row: &PgRow, field: &'static str) -> Result<Uuid, SessionLiveRepositoryError> {
    row.try_get::<Option<Uuid>, _>(field)?
        .ok_or(SessionLiveRepositoryError::Corruption(field))
}

fn decimal_u64(value: Decimal, field: &'static str) -> Result<u64, SessionLiveRepositoryError> {
    u64::try_from(value).map_err(|_| SessionLiveRepositoryError::Corruption(field))
}
