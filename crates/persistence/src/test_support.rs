//! Typed persistence observations and seeds used only by composed integration
//! tests.
//!
//! Composed tests above this crate state the durable state they need in domain
//! vocabulary and leave the table and column names here, so a schema change is
//! contained in — and exercised by — the crate that owns the schema.

use signalbox_domain::{ModelCallId, SessionId};
use sqlx::{FromRow, PgPool, types::Uuid};

#[derive(signalbox_derive::Accessors)]
/// Durable fleet state observed by the process-runtime soak harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FleetSoakCensus {
    /// Number of active turns in the isolated test database.
    #[get(copy)]
    active_turns: i64,
    /// Number of terminal turns in the isolated test database.
    #[get(copy)]
    terminal_turns: i64,
    /// Active turns parked for a user model-call recovery decision.
    #[get(copy)]
    awaiting_model_call_recovery_turns: i64,
    /// Scoped model calls carrying any terminal disposition.
    #[get(copy)]
    terminal_model_calls: i64,
    /// Scoped model calls carrying the ambiguity disposition.
    #[get(copy)]
    ambiguous_model_calls: i64,
}

#[derive(FromRow)]
struct FleetLifecycleCensusRow {
    active_turns: i64,
    terminal_turns: i64,
    awaiting_model_call_recovery_turns: i64,
}

#[derive(FromRow)]
struct FleetModelCallCensusRow {
    terminal_model_calls: i64,
    ambiguous_model_calls: i64,
}

/// Persistence-owned durable census for an isolated fleet-soak database.
#[derive(Clone, Debug)]
pub struct FleetSoakCensusRepository {
    pool: PgPool,
}

impl FleetSoakCensusRepository {
    /// Uses the supplied isolated integration-test pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Lists ordinary model-call identities in deterministic order.
    pub async fn model_call_ids(&self) -> Result<Box<[ModelCallId]>, sqlx::Error> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT model_call_id FROM model_call ORDER BY model_call_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(ModelCallId::from_uuid)
            .collect::<Vec<_>>()
            .into_boxed_slice())
    }

    /// Finds the ordinary model call belonging to exactly `session`.
    pub async fn model_call_id_for_session(
        &self,
        session: SessionId,
    ) -> Result<Option<ModelCallId>, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT model_call_id FROM model_call WHERE session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .map(ModelCallId::from_uuid))
    }

    /// Whether exactly `model_call` and its owning turn carry the ambiguity park.
    pub async fn has_ambiguous_recovery_park(
        &self,
        model_call: ModelCallId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM model_call AS call
                  JOIN turn_lifecycle AS turn
                    ON turn.turn_id = call.turn_id
                   AND turn.session_id = call.session_id
                 WHERE call.model_call_id = $1
                   AND call.terminal_disposition_kind = 'ambiguous'
                   AND turn.state_kind = 'active'
                   AND turn.active_phase_kind = 'awaiting_model_call_recovery'
            )",
        )
        .bind(model_call.into_uuid())
        .fetch_one(&self.pool)
        .await
    }

    /// Reads lifecycle state and dispositions for exactly `model_calls`.
    pub async fn census_for(
        &self,
        model_calls: &[ModelCallId],
    ) -> Result<FleetSoakCensus, sqlx::Error> {
        let model_call_ids: Vec<Uuid> = model_calls.iter().map(|call| call.into_uuid()).collect();
        let lifecycle: FleetLifecycleCensusRow = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE state_kind = 'active') AS active_turns,
                    count(*) FILTER (WHERE state_kind = 'terminal') AS terminal_turns,
                    count(*) FILTER (
                        WHERE state_kind = 'active'
                          AND active_phase_kind = 'awaiting_model_call_recovery'
                    ) AS awaiting_model_call_recovery_turns
               FROM turn_lifecycle
              WHERE turn_id IN (
                    SELECT turn_id
                      FROM model_call
                     WHERE model_call_id = ANY($1)
              )",
        )
        .bind(&model_call_ids)
        .fetch_one(&self.pool)
        .await?;
        let calls: FleetModelCallCensusRow = sqlx::query_as(
            "SELECT count(*) FILTER (
                        WHERE terminal_disposition_kind IS NOT NULL
                    ) AS terminal_model_calls,
                    count(*) FILTER (
                        WHERE terminal_disposition_kind = 'ambiguous'
                    ) AS ambiguous_model_calls
               FROM model_call
              WHERE model_call_id = ANY($1)",
        )
        .bind(&model_call_ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(FleetSoakCensus {
            active_turns: lifecycle.active_turns,
            terminal_turns: lifecycle.terminal_turns,
            awaiting_model_call_recovery_turns: lifecycle.awaiting_model_call_recovery_turns,
            terminal_model_calls: calls.terminal_model_calls,
            ambiguous_model_calls: calls.ambiguous_model_calls,
        })
    }
}

/// Makes deadline candidate reads raise a PostgreSQL exception with private
/// message, detail, hint, and statement context in an isolated test database.
pub async fn inject_deadline_diagnostic_failure(pool: &PgPool) -> Result<(), sqlx::Error> {
    // A candidate view makes the real deadline query receive a PostgreSQL
    // exception containing each payload-bearing diagnostic field.
    sqlx::raw_sql(
        "ALTER TABLE session_deadline RENAME TO saved_session_deadline;
         CREATE FUNCTION private_deadline_source() RETURNS uuid LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION USING
                 MESSAGE = 'private-deadline-message',
                 DETAIL = 'private-deadline-detail',
                 HINT = 'private-deadline-hint';
         END;
         $$;
         CREATE VIEW session_deadline AS
         SELECT private_deadline_source() AS session_id,
                'admission'::text AS deadline_kind,
                clock_timestamp() AS armed_at,
                clock_timestamp() - INTERVAL '1 hour' AS expires_at;",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Reads the complete accepted input queued by one exact goal resumption event.
pub async fn goal_resumption_input(
    pool: &PgPool,
    session: SessionId,
    event: signalbox_domain::GoalEventOrdinal,
) -> Result<signalbox_domain::UserContent, crate::goal::GoalRepositoryError> {
    let stored = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT accepted_input_content_parts_json(turn.accepted_input_id)
           FROM goal_turn AS turn
          WHERE turn.session_id = $1 AND turn.source_event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(rust_decimal::Decimal::from(event.get()))
    .fetch_one(pool)
    .await?;
    crate::user_content::decode(stored)
        .map_err(|_| crate::goal::GoalCorruption::Inconsistent("resumption input content").into())
}
