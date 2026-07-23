//! Typed in-transaction appends to the transactional outbox
//! (docs/spec/persistence-protocol.md).
//!
//! The functions in this module accept the caller's existing PostgreSQL
//! connection. They never begin or commit a transaction, so the state-changing
//! adapter retains ownership of the atomic boundary.

use signalbox_domain::{
    ContextFrontierId, ModelCallDisposition, ModelCallId, SemanticTranscriptEntryId, SessionId,
    TurnId,
};
use sqlx::PgConnection;

use crate::mapping::{session_id_to_uuid, turn_id_to_uuid};

const SESSION_CREATED: &str = "session_created";
const TURN_FAILED: &str = "turn_failed";
const MODEL_CALL_TRANSITION: &str = "model_call_transition";
const TURN_COMPLETED: &str = "turn_completed";
const TURN_REFUSED: &str = "turn_refused";
const TURN_CANCELLED: &str = "turn_cancelled";
const TURN_RECONCILIATION_REQUIRED: &str = "turn_reconciliation_required";
const STORAGE_VERSION: i16 = 1;

pub(crate) enum OutboxEvent {
    SessionCreated {
        session: SessionId,
    },
    TurnFailed {
        session: SessionId,
        turn: TurnId,
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    ModelCallTransition {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        state: ModelCallOutboxState,
    },
    TurnCompleted {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    TurnRefused {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
    TurnCancelled {
        session: SessionId,
        turn: TurnId,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    TurnReconciliationRequired {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
}

pub(crate) enum ModelCallOutboxState {
    Prepared,
    InFlight,
    CancellationRequested,
    Terminal(ModelCallDisposition),
}

pub(crate) async fn append(
    connection: &mut PgConnection,
    event: OutboxEvent,
) -> Result<(), sqlx::Error> {
    match event {
        OutboxEvent::SessionCreated { session } => {
            append_session_created(connection, session).await
        }
        OutboxEvent::TurnFailed {
            session,
            turn,
            failure_entry,
            terminal_frontier,
        } => append_turn_failed(connection, session, turn, failure_entry, terminal_frontier).await,
        OutboxEvent::ModelCallTransition {
            session,
            turn,
            call,
            state,
        } => append_model_call_transition(connection, session, turn, call, state).await,
        OutboxEvent::TurnCompleted {
            session,
            turn,
            call,
            completion_entry,
            terminal_frontier,
        } => {
            append_turn_completed(
                connection,
                session,
                turn,
                call,
                completion_entry,
                terminal_frontier,
            )
            .await
        }
        OutboxEvent::TurnRefused {
            session,
            turn,
            call,
            terminal_frontier,
        } => append_turn_refused(connection, session, turn, call, terminal_frontier).await,
        OutboxEvent::TurnCancelled {
            session,
            turn,
            cancellation_entry,
            terminal_frontier,
        } => {
            append_turn_cancelled(
                connection,
                session,
                turn,
                cancellation_entry,
                terminal_frontier,
            )
            .await
        }
        OutboxEvent::TurnReconciliationRequired {
            session,
            turn,
            call,
            terminal_frontier,
        } => {
            append_turn_reconciliation_required(connection, session, turn, call, terminal_frontier)
                .await
        }
    }
}

async fn append_session_created(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO session_created_outbox_event
            (event_sequence, event_kind, storage_version, session_id)
         SELECT event_sequence, event_kind, storage_version, session_id
           FROM header",
    )
    .bind(SESSION_CREATED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .execute(connection)
    .await?;

    Ok(())
}

async fn append_turn_failed(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_failed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, failure_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_FAILED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(failure_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;

    Ok(())
}

async fn append_model_call_transition(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    state: ModelCallOutboxState,
) -> Result<(), sqlx::Error> {
    let (state_kind, terminal_disposition) = match state {
        ModelCallOutboxState::Prepared => ("prepared", None),
        ModelCallOutboxState::InFlight => ("in_flight", None),
        ModelCallOutboxState::CancellationRequested => ("cancellation_requested", None),
        ModelCallOutboxState::Terminal(disposition) => {
            ("terminal", Some(encode_model_call_disposition(disposition)))
        }
    };
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO model_call_transition_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             model_call_id, turn_id, call_state_kind, terminal_disposition_kind)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7
           FROM header",
    )
    .bind(MODEL_CALL_TRANSITION)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(state_kind)
    .bind(terminal_disposition)
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_cancelled(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_cancelled_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, cancellation_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_CANCELLED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(cancellation_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_reconciliation_required(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_reconciliation_required_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_RECONCILIATION_REQUIRED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_completed(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    completion_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_completed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, completion_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7
           FROM header",
    )
    .bind(TURN_COMPLETED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(completion_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_refused(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_refused_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_REFUSED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

fn encode_model_call_disposition(disposition: ModelCallDisposition) -> &'static str {
    match disposition {
        ModelCallDisposition::Completed => "completed",
        ModelCallDisposition::KnownFailed => "known_failed",
        ModelCallDisposition::Refused => "refused",
        ModelCallDisposition::Cancelled => "cancelled",
        ModelCallDisposition::Ambiguous => "ambiguous",
    }
}
