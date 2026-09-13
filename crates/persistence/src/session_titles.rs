//! Session-title call records and conversation reads.

use futures_util::TryStreamExt;
use rust_decimal::Decimal;
use signalbox_application::UsageTokenAxes;
use signalbox_domain::{
    DirectModelSelection, ImportedSourceAttestation, ImportedTranscriptContent, ModelCallId,
    ResolvedProviderTarget, SessionId, TurnId,
};
use sqlx::{PgPool, Row};

/// Facts frozen before a title model is invoked.
#[derive(Clone, Debug)]
pub struct SessionTitleCall {
    pub call: ModelCallId,
    pub session: SessionId,
    pub selection: DirectModelSelection,
    pub target: ResolvedProviderTarget,
    pub credential_reference: String,
    pub input_includes_cache_tokens: bool,
    pub initial_for_turn: Option<TurnId>,
}

/// Storage for dedicated title calls; no method appends transcript content.
#[derive(Clone, Debug)]
pub struct SessionTitleRepository {
    pool: PgPool,
}

impl SessionTitleRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims an initial call once, only for the first completed turn and an unset title.
    /// On-demand calls require only an existing session.
    pub async fn prepare(&self, call: &SessionTitleCall) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let exists =
            sqlx::query_scalar::<_, uuid::Uuid>(crate::lock_inventory::REPLACE_SESSION_METADATA)
                .bind(call.session.into_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !exists {
            return Ok(false);
        }
        if let Some(turn) = call.initial_for_turn {
            let eligible: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS (SELECT 1 FROM session_metadata WHERE session_id = $1 AND title IS NOT NULL)
                 AND NOT EXISTS (SELECT 1 FROM session_title_model_call WHERE session_id = $1 AND initial_for_turn IS NOT NULL)
                 AND COALESCE($2 = (SELECT turn_id FROM turn_lifecycle WHERE session_id = $1
                     AND state_kind = 'terminal' AND terminal_disposition_kind = 'completed'
                     ORDER BY acceptance_position LIMIT 1), false)")
                .bind(call.session.into_uuid()).bind(turn.into_uuid()).fetch_one(&mut *tx).await?;
            if !eligible {
                return Ok(false);
            }
        }
        sqlx::query("INSERT INTO session_title_model_call
            (model_call_id, session_id, direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, usage_input_includes_cache_tokens, initial_for_turn, state_kind)
            VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')")
            .bind(call.call.into_uuid()).bind(call.session.into_uuid()).bind(call.selection.into_uuid())
            .bind(call.target.identity().into_uuid()).bind(&call.credential_reference)
            .bind(call.input_includes_cache_tokens).bind(call.initial_for_turn.map(TurnId::into_uuid))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Reads recent conversation text within the caller's configured model budget.
    pub async fn conversation(
        &self,
        session: SessionId,
        max_chars: i32,
    ) -> Result<String, sqlx::Error> {
        let mut rows = sqlx::query(
            "WITH frontier AS (
                SELECT context_frontier_id FROM context_frontier WHERE owning_session_id = $1
                ORDER BY member_count DESC LIMIT 1
             )
             SELECT LEFT(COALESCE(entry.assistant_text_value, entry.context_summary_value, part.text_value), $2) AS value,
                    imported.content_encoding
             FROM frontier JOIN context_frontier_member AS member
               ON member.owning_session_id = $1 AND member.context_frontier_id = frontier.context_frontier_id
             JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
             LEFT JOIN accepted_input_content_part AS part
               ON part.accepted_input_id = entry.origin_accepted_input_id AND part.part_kind = 'text'
             LEFT JOIN imported_transcript_entry AS imported
               ON imported.imported_conversation_id = entry.imported_conversation_id
              AND imported.imported_transcript_entry_id = entry.imported_transcript_entry_id
              AND imported.content_kind = 1
             WHERE COALESCE(entry.assistant_text_value, entry.context_summary_value, part.text_value) IS NOT NULL
                OR imported.content_encoding IS NOT NULL
             ORDER BY member.member_position DESC, part.position DESC NULLS LAST")
            .bind(session.into_uuid()).bind(max_chars).fetch(&self.pool);
        let mut remaining = usize::try_from(max_chars).unwrap_or_default();
        let mut parts = Vec::new();
        while remaining > 0 {
            let Some(row) = rows.try_next().await? else {
                break;
            };
            let text = match row.try_get::<Option<String>, _>("value")? {
                Some(text) => text,
                None => {
                    let encoded: Vec<u8> = row.try_get("content_encoding")?;
                    match crate::conversation_import_codec::decode_content(&encoded)
                        .map_err(|_| sqlx::Error::Decode("invalid imported title context".into()))?
                    {
                        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                            text,
                        )) => text.as_str().to_owned(),
                        _ => continue,
                    }
                }
            };
            let text = text.chars().take(remaining).collect::<String>();
            remaining = remaining.saturating_sub(text.chars().count() + 1);
            parts.push(text);
        }
        parts.reverse();
        Ok(parts.join("\n"))
    }

    /// Commits the send boundary before provider execution.
    pub async fn authorize(&self, call: ModelCallId) -> Result<(), sqlx::Error> {
        let affected = sqlx::query("UPDATE session_title_model_call SET state_kind = 'in_flight', in_flight_at = statement_timestamp()
            WHERE model_call_id = $1 AND state_kind = 'prepared'")
            .bind(call.into_uuid()).execute(&self.pool).await?.rows_affected();
        if affected != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }

    /// Records successful or failed completion and its reported usage together.
    pub async fn finish(
        &self,
        call: ModelCallId,
        title: Option<&str>,
        usage: UsageTokenAxes,
    ) -> Result<(), sqlx::Error> {
        let affected = sqlx::query("UPDATE session_title_model_call
            SET state_kind = 'terminal', terminal_at = statement_timestamp(), title = $2,
                input_tokens = $3, output_tokens = $4, cache_creation_input_tokens = $5, cache_read_input_tokens = $6
            WHERE model_call_id = $1 AND state_kind IN ('prepared', 'in_flight')")
            .bind(call.into_uuid()).bind(title).bind(usage.input.map(Decimal::from))
            .bind(usage.output.map(Decimal::from)).bind(usage.cache_creation_input.map(Decimal::from))
            .bind(usage.cache_read_input.map(Decimal::from)).execute(&self.pool).await?.rows_affected();
        if affected != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }
}
