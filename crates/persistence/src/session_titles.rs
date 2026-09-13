//! Session-title call records and conversation reads.

use futures_util::TryStreamExt;
use rust_decimal::Decimal;
use signalbox_application::UsageTokenAxes;
use signalbox_domain::{
    DirectModelSelection, DurableCommandId, ModelCallId, ResolvedProviderTarget, SessionId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::session_metadata::SessionMetadataRepositoryError;

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

/// Admission result for a session title call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareSessionTitleOutcome {
    /// The call and its invocation reservation committed.
    Prepared,
    /// The session or completed turn is absent, titled, or already claimed.
    Ineligible,
    /// No credential can currently admit the call.
    Unavailable,
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

    /// Finds one completed turn for each untitled session without an initial claim.
    pub async fn unclaimed_initial_turns(&self) -> Result<Vec<(SessionId, TurnId)>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "SELECT DISTINCT ON (turn.session_id) turn.session_id, turn.turn_id
             FROM turn_lifecycle AS turn
             WHERE turn.state_kind = 'terminal' AND turn.terminal_disposition_kind = 'completed'
               AND NOT EXISTS (SELECT 1 FROM session_metadata metadata
                   WHERE metadata.session_id = turn.session_id AND metadata.title IS NOT NULL)
               AND NOT EXISTS (SELECT 1 FROM session_title_model_call title
                   WHERE title.session_id = turn.session_id AND title.initial_for_turn IS NOT NULL
                     AND NOT title.abandoned)
             ORDER BY turn.session_id, turn.turn_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(session, turn)| (SessionId::from_uuid(session), TurnId::from_uuid(turn)))
            .collect())
    }

    /// Claims an initial call for a completed turn and an unset, unclaimed session.
    /// On-demand calls require only an existing session.
    pub async fn prepare(
        &self,
        call: &mut SessionTitleCall,
        pools: &crate::model_execution::CredentialPoolRuntimeCatalog,
    ) -> Result<PrepareSessionTitleOutcome, crate::model_execution::ModelCallRepositoryError> {
        let mut tx = self.pool.begin().await?;
        let exists =
            sqlx::query_scalar::<_, uuid::Uuid>(crate::lock_inventory::REPLACE_SESSION_METADATA)
                .bind(call.session.into_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !exists {
            return Ok(PrepareSessionTitleOutcome::Ineligible);
        }
        if let Some(turn) = call.initial_for_turn {
            let eligible: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS (SELECT 1 FROM session_metadata WHERE session_id = $1 AND title IS NOT NULL)
                 AND NOT EXISTS (SELECT 1 FROM session_title_model_call WHERE session_id = $1 AND initial_for_turn IS NOT NULL AND NOT abandoned)
                 AND EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2
                     AND state_kind = 'terminal' AND terminal_disposition_kind = 'completed')")
                .bind(call.session.into_uuid()).bind(turn.into_uuid()).fetch_one(&mut *tx).await?;
            if !eligible {
                return Ok(PrepareSessionTitleOutcome::Ineligible);
            }
        }
        crate::model_execution::credential_pool::acquire_model_call_outbox_order_guard(&mut tx)
            .await?;
        let Some(credential) =
            crate::model_execution::credential_pool::select_session_pool_credential(
                &mut tx,
                call.session,
                call.target,
                signalbox_application::ModelCallCredentialReference::new(
                    &call.credential_reference,
                ),
                pools,
            )
            .await?
        else {
            return Ok(PrepareSessionTitleOutcome::Unavailable);
        };
        call.credential_reference = credential.as_str().to_owned();
        sqlx::query("INSERT INTO session_title_model_call
            (model_call_id, session_id, direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, usage_input_includes_cache_tokens, initial_for_turn, state_kind)
            VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')")
            .bind(call.call.into_uuid()).bind(call.session.into_uuid()).bind(call.selection.into_uuid())
            .bind(call.target.identity().into_uuid()).bind(&call.credential_reference)
            .bind(call.input_includes_cache_tokens).bind(call.initial_for_turn.map(TurnId::into_uuid))
            .execute(&mut *tx).await?;
        crate::credential_invocations::reserve(&mut tx, call.call, &call.credential_reference)
            .await?;
        tx.commit().await?;
        Ok(PrepareSessionTitleOutcome::Prepared)
    }

    /// Closes abandoned title calls at startup and releases their initial claims.
    /// Registered processes retain capacity until their existing observer confirms cleanup.
    pub async fn abandon_incomplete(&self) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_title_model_call SET state_kind = 'terminal', terminal_at = statement_timestamp(), abandoned = true
            WHERE state_kind IN ('prepared', 'in_flight')").execute(&self.pool).await?;
        Ok(())
    }

    /// Abandons an unsettled call, releasing its initial claim and unregistered capacity.
    /// Already terminal calls retain their immutable completion and claim.
    pub async fn abandon(&self, call: ModelCallId) -> Result<(), sqlx::Error> {
        let affected = sqlx::query(
            "UPDATE session_title_model_call
            SET state_kind = 'terminal', terminal_at = statement_timestamp(), abandoned = true
            WHERE model_call_id = $1 AND state_kind IN ('prepared', 'in_flight')",
        )
        .bind(call.into_uuid())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }

    /// Reads recent conversation text within the caller's configured model budget.
    pub async fn conversation(
        &self,
        session: SessionId,
        max_chars: i32,
    ) -> Result<String, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(source) = crate::context_compaction::load_compaction_source(&mut tx, session)
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
        else {
            return Ok(String::new());
        };
        let mut rows = sqlx::query(
            "SELECT LEFT(COALESCE(entry.assistant_text_value, entry.context_summary_value, part.text_value), $2) AS value,
                    substring(imported.content_encoding FROM 1 FOR $3) AS content_encoding
             FROM context_frontier_member AS member
             JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
             LEFT JOIN accepted_input_content_part AS part
               ON part.accepted_input_id = entry.origin_accepted_input_id AND part.part_kind = 'text'
             LEFT JOIN imported_transcript_entry AS imported
               ON imported.imported_conversation_id = entry.imported_conversation_id
              AND imported.imported_transcript_entry_id = entry.imported_transcript_entry_id
              AND imported.content_kind = 1
             WHERE member.owning_session_id = $1 AND member.context_frontier_id = $4
               AND (COALESCE(entry.assistant_text_value, entry.context_summary_value, part.text_value) IS NOT NULL
                OR imported.content_encoding IS NOT NULL)
             ORDER BY member.member_position DESC, part.position DESC NULLS LAST")
            .bind(session.into_uuid()).bind(max_chars)
            .bind(max_chars.saturating_add(crate::conversation_import_codec::TEXT_CONTENT_HEADER_BYTES))
            .bind(source.frontier.into_uuid()).fetch(&mut *tx);
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
                    match crate::conversation_import_codec::decode_text_prefix(&encoded)
                        .map_err(|_| sqlx::Error::Decode("invalid imported title context".into()))?
                    {
                        Some(text) => text.to_owned(),
                        None => continue,
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
        finish_call(&mut *self.pool.acquire().await?, call, title, usage).await
    }

    /// Validates a generated title against preserved metadata before recording success.
    /// Initial-title installation and terminal usage commit in the same transaction.
    /// An invalid combined snapshot records failed generation and returns no title.
    /// A repeated settlement returns the immutable terminal title.
    pub async fn finish_generated(
        &self,
        command_id: DurableCommandId,
        call: ModelCallId,
        mut title: Option<String>,
        usage: UsageTokenAxes,
    ) -> Result<Option<String>, SessionMetadataRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT session_id, initial_for_turn IS NOT NULL AS initial, state_kind, title
                FROM session_title_model_call WHERE model_call_id = $1 FOR UPDATE",
        )
        .bind(call.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if row.try_get::<String, _>("state_kind")? == "terminal" {
            return Ok(row.try_get("title")?);
        }
        if let Some(value) = title.as_ref() {
            let session = SessionId::from_uuid(row.try_get("session_id")?);
            let initial: bool = row.try_get("initial")?;
            match crate::session_metadata::accept_generated_title(
                &mut transaction,
                initial.then_some(command_id),
                session,
                value.clone(),
            )
            .await
            {
                Ok(()) => {}
                Err(SessionMetadataRepositoryError::InvalidTitleMerge(_)) => title = None,
                Err(error) => return Err(error),
            }
        }
        finish_call(&mut transaction, call, title.as_deref(), usage).await?;
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                SessionMetadataRepositoryError::CommitAmbiguous(error)
            } else {
                error.into()
            }
        })?;
        Ok(title)
    }
}

async fn finish_call(
    connection: &mut PgConnection,
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
            .bind(usage.cache_read_input.map(Decimal::from)).execute(connection).await?.rows_affected();
    if affected != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}
