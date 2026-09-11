use rust_decimal::Decimal;
use signalbox_domain::SessionId;
use sqlx::{PgConnection, types::Uuid};

pub(crate) async fn completed_turn_sequences(
    connection: &mut PgConnection,
    sessions: &[SessionId],
) -> Result<Vec<Decimal>, sqlx::Error> {
    let sessions: Vec<Uuid> = sessions.iter().map(|session| session.into_uuid()).collect();
    sqlx::query_scalar(
        "SELECT terminal.event_sequence
         FROM session_lifecycle AS lifecycle
         JOIN LATERAL (
             SELECT event_sequence FROM turn_terminal_outbox_event
             WHERE session_id = lifecycle.session_id
             ORDER BY event_sequence DESC LIMIT 1
         ) AS terminal ON true
         WHERE lifecycle.session_id = ANY($1) AND lifecycle.state_kind = 'active'
           AND NOT EXISTS (SELECT 1 FROM goal_event WHERE session_id = lifecycle.session_id)
           AND NOT EXISTS (SELECT 1 FROM turn_lifecycle
                           WHERE session_id = lifecycle.session_id AND state_kind <> 'terminal')
           AND NOT EXISTS (SELECT 1 FROM accepted_input
                           WHERE session_id = lifecycle.session_id
                             AND disposition_kind = 'pending_steering')
         ORDER BY terminal.event_sequence",
    )
    .bind(sessions)
    .fetch_all(connection)
    .await
}
