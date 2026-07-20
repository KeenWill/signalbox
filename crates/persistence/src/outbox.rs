//! Typed in-transaction appends to the ADR-0040 outbox.
//!
//! The functions in this module accept the caller's existing PostgreSQL
//! connection. They never begin or commit a transaction, so the state-changing
//! adapter retains ownership of the atomic boundary.

use signalbox_domain::SessionId;
use sqlx::PgConnection;

use crate::mapping::session_id_to_uuid;

const SESSION_CREATED: &str = "session_created";
const STORAGE_VERSION: i16 = 1;

pub(crate) async fn append_session_created(
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
