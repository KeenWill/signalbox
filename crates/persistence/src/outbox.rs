//! Typed in-transaction appends to the ADR-0040 outbox.
//!
//! The functions in this module accept the caller's existing PostgreSQL
//! connection. They never begin or commit a transaction, so the state-changing
//! adapter retains ownership of the atomic boundary.

use signalbox_domain::{ContextFrontierId, SemanticTranscriptEntryId, SessionId, TurnId};
use sqlx::PgConnection;

use crate::mapping::{session_id_to_uuid, turn_id_to_uuid};

const SESSION_CREATED: &str = "session_created";
const TURN_FAILED: &str = "turn_failed";
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
