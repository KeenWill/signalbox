use super::entry::decode_transcript_entry;
use super::load::{
    decode_process_session_ancestry, load_process_session_placement, open_transcript_in_transaction,
};
use super::reader::ProcessTranscriptReader;
use super::session::{
    ProcessScopedTranscriptRead, ProcessSessionDefaults, ProcessSessionDefaultsRead,
    ProcessSessionSummary, ProcessSessionSummaryReader, decode_session_defaults_value,
};
use super::transcript_types::{
    ProcessModelCallRecoveryPrecondition, ProcessSessionAncestry, ProcessTranscriptEntry,
    ProcessTranscriptItem, ProcessTranscriptSnapshot,
};
use super::{
    ProcessReadCorruption, ProcessReadError, ProcessReadRepository, REPEATABLE_READ_ONLY,
    decode_positive, required,
};
use crate::mapping::{session_id_from_uuid, session_id_to_uuid};
use rust_decimal::Decimal;
use signalbox_domain::{
    SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId, SessionReadScopeDecision,
    ToolRequestId, TurnId,
};
use sqlx::types::Uuid;
use sqlx::{PgPool, Row};
use std::collections::VecDeque;

impl ProcessReadRepository {
    /// Uses the supplied pool for independent repeatable-read snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            automatic_reconciliation_attempt_budget: None,
        }
    }

    /// Applies the deployment's optional automatic reconciliation budget.
    pub const fn with_automatic_reconciliation_attempt_budget(
        mut self,
        budget: Option<u32>,
    ) -> Self {
        self.automatic_reconciliation_attempt_budget = Some(budget);
        self
    }

    /// Reads one complete current or named immutable session-defaults epoch.
    ///
    /// A `None` version selects the epoch named by the session's current
    /// pointer; a named version selects exactly that immutable epoch. The
    /// read is one statement-consistent SELECT. For an existing session, a
    /// missing current pointer or missing pointed-at epoch fails closed as
    /// corruption; only a named version that was never installed is the typed
    /// absent-version outcome.
    pub async fn read_session_defaults(
        &self,
        session: SessionId,
        version: Option<signalbox_domain::SessionConfigurationDefaultsVersion>,
    ) -> Result<ProcessSessionDefaultsRead, ProcessReadError> {
        let named = version.map(|value| Decimal::from(value.as_u64()));
        let row = sqlx::query(
            "SELECT
                session_row.session_id,
                current_defaults.current_version,
                current_epoch.version AS current_epoch_version,
                selected_defaults.version AS selected_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id,
                selected_defaults.dangerous_tool_auto_approval,
                selected_defaults.system_prompt,
                selected_defaults.model_settings
               FROM session AS session_row
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session_row.session_id
               LEFT JOIN session_defaults_version AS current_epoch
                 ON current_epoch.session_id = session_row.session_id
                AND current_epoch.version = current_defaults.current_version
               LEFT JOIN session_defaults_version AS selected_defaults
                 ON selected_defaults.session_id = session_row.session_id
                AND selected_defaults.version =
                        COALESCE($2, current_defaults.current_version)
              WHERE session_row.session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .bind(named)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(ProcessSessionDefaultsRead::SessionNotFound);
        };
        // An existing session must carry a current pointer that resolves to
        // an installed epoch even when a named historical epoch is selected:
        // a missing pointer or a dangling pointer is corruption, not a
        // servable read.
        let current_version: Option<Decimal> = row.try_get("current_version")?;
        if current_version.is_none() {
            return Err(ProcessReadCorruption::Missing("current defaults pointer").into());
        }
        let current_epoch_version: Option<Decimal> = row.try_get("current_epoch_version")?;
        if current_epoch_version.is_none() {
            return Err(ProcessReadCorruption::Missing("current defaults epoch").into());
        }
        let selected_version: Option<Decimal> = row.try_get("selected_version")?;
        let Some(selected_version) = selected_version else {
            return if named.is_some() {
                Ok(ProcessSessionDefaultsRead::VersionNotFound)
            } else {
                Err(ProcessReadCorruption::Missing("current defaults epoch").into())
            };
        };
        let selected_version = signalbox_domain::SessionConfigurationDefaultsVersion::try_from_u64(
            u64::try_from(selected_version)
                .map_err(|_| ProcessReadCorruption::InvalidOrdinal("selected_version"))?,
        )
        .ok_or(ProcessReadCorruption::InvalidOrdinal("selected_version"))?;
        let defaults = decode_session_defaults_value(&row)?;
        Ok(ProcessSessionDefaultsRead::Read(ProcessSessionDefaults {
            session,
            version: selected_version,
            defaults,
        }))
    }

    /// Collects every current session summary in session-identity order.
    ///
    /// Production process serving uses [`Self::open_session_summaries`] to
    /// avoid retaining the complete catalog in request memory.
    pub async fn list_sessions(&self) -> Result<Vec<ProcessSessionSummary>, ProcessReadError> {
        let mut reader = self.open_session_summaries().await?;
        let mut summaries = Vec::new();
        while let Some(summary) = reader.next_summary().await? {
            summaries.push(summary);
        }
        Ok(summaries)
    }

    /// Opens one repeatable-read session-summary cursor.
    ///
    /// The cursor yields at most one decoded summary at a time.
    pub async fn open_session_summaries(
        &self,
    ) -> Result<ProcessSessionSummaryReader, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        Ok(ProcessSessionSummaryReader {
            transaction: Some(transaction),
            next_session_after: None,
            pending: VecDeque::new(),
            summary_count: 0,
            committed_summary_count: None,
        })
    }

    /// Reads the selected session's immutable ancestry, or `None` when absent.
    ///
    /// This narrow read lets a process adapter reject a representation that
    /// cannot carry imported ancestry before constructing or mutating it.
    pub async fn session_ancestry(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessSessionAncestry>, ProcessReadError> {
        let row = sqlx::query(
            "SELECT ancestry_kind
               FROM session
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_process_session_ancestry(&row))
            .transpose()
    }

    /// Returns whether the selected session has durable tool-only history.
    ///
    /// This narrow read reports whether tool-only transcript evidence exists.
    pub async fn session_has_tool_history(
        &self,
        requested_session: SessionId,
    ) -> Result<bool, ProcessReadError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM tool_request
                  WHERE session_id = $1
             )",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Returns the session owning the named logical tool request, or `None`
    /// when no request has that identity.
    ///
    /// This narrow read lets a process adapter refuse a decision whose named
    /// session does not own the named request before a durable command is
    /// recorded; the canonical decision command remains the authority for
    /// every recorded outcome.
    pub async fn tool_request_session(
        &self,
        request: ToolRequestId,
    ) -> Result<Option<SessionId>, ProcessReadError> {
        let row = sqlx::query_scalar::<_, Uuid>(
            "SELECT session_id
               FROM tool_request
              WHERE request_id = $1",
        )
        .bind(request.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(SessionId::from_uuid))
    }

    /// Reads whether the session exists and, when it does, whether its active
    /// turn is parked on the model-call recovery wait.
    ///
    /// This narrow read lets a process adapter refuse a reconciliation request
    /// whose named turn owes no user decision, before recording a durable
    /// command. It is a precondition, never authority: the authoritative
    /// transaction revalidates the exact expected active turn under the
    /// session lock, and an ended attempt never returns to a live phase, so an
    /// admitted wait can only stay parked or terminalize before that
    /// transaction runs. An absent session is reported separately so the
    /// adapter can leave that case to the authoritative transaction's own
    /// typed rejection instead of collapsing it into a missing wait.
    pub async fn model_call_recovery_precondition(
        &self,
        requested_session: SessionId,
    ) -> Result<ProcessModelCallRecoveryPrecondition, ProcessReadError> {
        let row: Option<(bool, Option<Uuid>)> = sqlx::query_as(
            "SELECT TRUE,
                    (SELECT turn_id
                       FROM turn_lifecycle
                      WHERE session_id = session.session_id
                        AND state_kind = 'active'
                        AND NOT delegation_runtime_terminal
                        AND active_phase_kind = 'awaiting_model_call_recovery')
               FROM session
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            None => ProcessModelCallRecoveryPrecondition::SessionAbsent,
            Some((_, None)) => ProcessModelCallRecoveryPrecondition::NoParkedTurn,
            Some((_, Some(turn))) => ProcessModelCallRecoveryPrecondition::Parked {
                turn: TurnId::from_uuid(turn),
            },
        })
    }

    /// Reads only the exact source-qualified semantic entries selected for a
    /// compaction range, preserving their one-based physical positions and
    /// using admitted context text for completed tool results.
    pub async fn read_selected_transcript_entries(
        &self,
        positions: &[u64],
        references: &[SemanticTranscriptEntryRef],
    ) -> Result<Box<[ProcessTranscriptEntry]>, ProcessReadError> {
        if positions.is_empty() || positions.len() != references.len() {
            return Err(
                ProcessReadCorruption::Inconsistent("selected transcript range shape").into(),
            );
        }
        let stored_positions = positions
            .iter()
            .copied()
            .map(Decimal::from)
            .collect::<Vec<_>>();
        let source_sessions = references
            .iter()
            .map(|reference| session_id_to_uuid(reference.source_session()))
            .collect::<Vec<_>>();
        let entry_ids = references
            .iter()
            .map(|reference| reference.entry().into_uuid())
            .collect::<Vec<_>>();
        let rows = sqlx::query(
            "SELECT
                selected.member_position,
                selected.source_session_id,
                selected.semantic_entry_id,
                entry.payload_kind,
                entry.runner_placement_revision,
                entry.assistant_response_part_ordinal,
                entry.origin_accepted_input_id,
                entry.steering_source_turn_id,
                entry.failed_turn_id,
                entry.assistant_text_value,
                entry.producing_model_call_id,
                entry.assistant_tool_request_id,
                entry.tool_result_request_id,
                entry.tool_result_attempt_id,
                entry.completed_turn_id,
                entry.cancelled_turn_id,
                entry.imported_conversation_id,
                entry.imported_transcript_entry_id,
                entry.model_identity_turn_id,
                entry.model_identity_defaults_version,
                entry.model_identity_direct_selection_id,
                entry.context_summary_value,
                entry.context_summary_producing_call_id,
                entry.context_summary_first_source_session_id,
                entry.context_summary_first_entry_id,
                entry.context_summary_through_source_session_id,
                entry.context_summary_through_entry_id,
                entry.delegated_task_spawning_tool_request_id,
                entry.delegation_message_id,
                entry.delegation_result_awaiting_tool_request_id,
                entry.delegation_result_spawning_tool_request_id,
                delegated_task.task_content AS delegated_task_content,
                task_relation.parent_session_id AS delegated_task_parent_session_id,
                task_relation.parent_turn_id AS delegated_task_parent_turn_id,
                delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
                delegated_message.event_ordinal AS delegation_message_ordinal,
                delegated_message.content_text AS delegation_message_content,
                message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
                message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
                CASE delegated_message.direction
                    WHEN 'parent_to_child' THEN message_relation.parent_session_id
                    WHEN 'child_to_parent' THEN message_relation.child_session_id
                END AS delegation_message_sender_session_id,
                delegated_wait.child_session_id AS delegation_result_child_session_id,
                delegated_wait.wait_mode AS delegation_result_wait_mode,
                result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
                delegated_result.outcome_kind AS delegation_result_outcome_kind,
                delegated_result.content_text AS delegation_result_content,
                result_event.reason_kind AS delegation_result_reason_kind,
                result_event.provenance_kind,
                result_event.provenance_session_id,
                result_event.provenance_turn_id,
                result_event.provenance_goal_generation,
                result_event.provenance_command_id,
                imported.source_speaker_kind AS imported_source_speaker_kind,
                imported.content_encoding AS imported_content_encoding,
                CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                     ELSE accepted_input_content_parts_json(
                        accepted.accepted_input_id)
                END AS origin_content,
                accepted.origin_turn_id,
                call.turn_id AS assistant_turn_id,
                result_attempt.request_id AS result_attempt_request_id,
                transcript_request.tool_name AS transcript_tool_name,
                transcript_request.inadmissible_reason AS transcript_inadmissible_reason,
                transcript_request.arguments_text AS transcript_tool_arguments,
                result_attempt.terminal_disposition_kind AS result_disposition,
                result_attempt.context_result_text AS result_text,
                result_attempt.error_kind AS result_error_kind,
                result_attempt.context_error_detail AS result_error_detail,
                transcript_approval.decision_kind AS transcript_decision_kind,
                EXISTS (
                    SELECT 1 FROM tool_approval_user_override AS recorded
                     WHERE recorded.denied_request_id = transcript_request.request_id
                ) AS transcript_override_recorded,
                transcript_approval.decision_source AS transcript_decision_source,
                transcript_approval.denial_reason AS transcript_denial_reason,
                transcript_approval.user_command_id AS transcript_user_command_id,
                transcript_approval.delegate_model_selection_id AS transcript_delegate_model_selection_id,
                transcript_approval.delegate_model_call_id AS transcript_delegate_model_call_id,
                transcript_approval.rationale AS transcript_decision_rationale,
                transcript_approval.override_denied_request_id
                    AS transcript_override_denied_request_id,
                transcript_override.command_id AS transcript_override_command_id
               FROM UNNEST($1::numeric[], $2::uuid[], $3::uuid[])
                    WITH ORDINALITY AS selected(
                        member_position,
                        source_session_id,
                        semantic_entry_id,
                        selected_ordinal
                    )
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = selected.source_session_id
                AND entry.semantic_entry_id = selected.semantic_entry_id
               LEFT JOIN accepted_input AS accepted
                 ON accepted.session_id = entry.source_session_id
                AND accepted.accepted_input_id = entry.origin_accepted_input_id
               LEFT JOIN model_call AS call
                 ON call.session_id = entry.source_session_id
                AND call.model_call_id = entry.producing_model_call_id
               LEFT JOIN tool_attempt AS result_attempt
                 ON result_attempt.session_id = entry.source_session_id
                AND result_attempt.attempt_id = entry.tool_result_attempt_id
               LEFT JOIN tool_request AS transcript_request
                 ON transcript_request.session_id = entry.source_session_id
                AND transcript_request.request_id = COALESCE(
                    entry.assistant_tool_request_id,
                    entry.tool_result_request_id,
                    result_attempt.request_id
                )
               LEFT JOIN tool_approval_decision AS transcript_approval
                 ON transcript_approval.request_id = transcript_request.request_id
               LEFT JOIN tool_approval_user_override AS transcript_override
                 ON transcript_override.denied_request_id =
                    transcript_approval.override_denied_request_id
               LEFT JOIN imported_transcript_entry AS imported
                 ON imported.imported_conversation_id =
                        entry.imported_conversation_id
                AND imported.imported_transcript_entry_id =
                        entry.imported_transcript_entry_id
               LEFT JOIN session_delegation_initial_task AS delegated_task
                 ON delegated_task.spawning_tool_request_id =
                        entry.delegated_task_spawning_tool_request_id
                AND delegated_task.child_session_id = entry.source_session_id
                AND delegated_task.semantic_entry_id = entry.semantic_entry_id
               LEFT JOIN session_delegation AS task_relation
                 ON task_relation.spawning_tool_request_id =
                        delegated_task.spawning_tool_request_id
               LEFT JOIN session_message_delivery AS message_delivery
                 ON message_delivery.message_id = entry.delegation_message_id
                AND message_delivery.recipient_session_id = entry.source_session_id
               LEFT JOIN session_message AS delegated_message
                 ON delegated_message.message_id = message_delivery.message_id
                AND delegated_message.spawning_tool_request_id =
                        message_delivery.spawning_tool_request_id
               LEFT JOIN session_delegation AS message_relation
                 ON message_relation.spawning_tool_request_id =
                        delegated_message.spawning_tool_request_id
               LEFT JOIN session_child_result_delivery AS result_delivery
                 ON result_delivery.awaiting_tool_request_id =
                        entry.delegation_result_awaiting_tool_request_id
                AND result_delivery.spawning_tool_request_id =
                        entry.delegation_result_spawning_tool_request_id
                AND result_delivery.parent_session_id = entry.source_session_id
               LEFT JOIN session_delegation_wait AS delegated_wait
                 ON delegated_wait.awaiting_tool_request_id =
                        result_delivery.awaiting_tool_request_id
                AND delegated_wait.spawning_tool_request_id =
                        result_delivery.spawning_tool_request_id
                AND delegated_wait.parent_session_id = result_delivery.parent_session_id
               LEFT JOIN session_child_result AS delegated_result
                 ON delegated_result.spawning_tool_request_id =
                        result_delivery.spawning_tool_request_id
               LEFT JOIN session_delegation_event AS result_event
                 ON result_event.spawning_tool_request_id =
                        delegated_result.spawning_tool_request_id
                AND result_event.event_ordinal = delegated_result.event_ordinal
                AND result_event.event_kind = delegated_result.event_kind
              ORDER BY selected.selected_ordinal",
        )
        .bind(&stored_positions)
        .bind(&source_sessions)
        .bind(&entry_ids)
        .fetch_all(&self.pool)
        .await?;
        if rows.len() != references.len() {
            return Err(ProcessReadCorruption::Inconsistent(
                "selected transcript range membership",
            )
            .into());
        }
        let mut entries = Vec::with_capacity(rows.len());
        for ((row, expected_position), expected_reference) in
            rows.iter().zip(positions).zip(references)
        {
            let stored_position = decode_positive(
                required(row, "member_position")?,
                "selected transcript member position",
            )?;
            let source_session = session_id_from_uuid(required(row, "source_session_id")?);
            let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
            if stored_position != *expected_position
                || SemanticTranscriptEntryRef::from_source(source_session, entry)
                    != *expected_reference
            {
                return Err(
                    ProcessReadCorruption::Inconsistent("selected transcript entry order").into(),
                );
            }
            let entry_index =
                stored_position
                    .checked_sub(1)
                    .ok_or(ProcessReadCorruption::InvalidOrdinal(
                        "selected transcript member position",
                    ))?;
            entries.push(decode_transcript_entry(row, entry_index)?);
        }
        Ok(entries.into_boxed_slice())
    }

    /// Reads one complete transcript snapshot, or `None` only when the session
    /// is absent from the shared transaction snapshot.
    pub async fn read_transcript(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessTranscriptSnapshot>, ProcessReadError> {
        let Some(mut reader) = self.open_transcript(requested_session).await? else {
            return Ok(None);
        };
        let mut turns = Vec::new();
        let mut model_call_usage = Vec::new();
        let mut entries = Vec::new();
        while let Some(item) = reader.next_item().await? {
            match item {
                ProcessTranscriptItem::Turn(turn) => turns.push(turn),
                ProcessTranscriptItem::ModelCallUsage(usage) => model_call_usage.push(usage),
                ProcessTranscriptItem::Entry(entry) => entries.push(entry),
            }
        }
        let summary = reader
            .summary()
            .ok_or(ProcessReadCorruption::Missing("process transcript summary"))?;
        Ok(Some(ProcessTranscriptSnapshot {
            session: summary.session(),
            cursor: summary.cursor(),
            runner: reader.runner,
            turns,
            model_call_usage,
            entries,
        }))
    }

    /// Opens one repeatable-read transcript cursor, or `None` only when the
    /// session is absent from that transaction snapshot.
    ///
    /// The cursor yields at most one decoded turn, model-call usage record, or
    /// entry at a time. This is the production boundary for spooling snapshots
    /// without transcript-sized process memory.
    pub async fn open_transcript(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessTranscriptReader>, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let session_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
                .bind(session_id_to_uuid(requested_session))
                .fetch_one(&mut *transaction)
                .await?;
        if !session_exists {
            transaction.commit().await?;
            return Ok(None);
        }

        Ok(Some(
            open_transcript_in_transaction(
                transaction,
                requested_session,
                self.automatic_reconciliation_attempt_budget,
            )
            .await?,
        ))
    }

    /// Checks one requester's parent-directory scope and opens the target
    /// transcript within the same repeatable-read snapshot.
    pub async fn open_scoped_transcript(
        &self,
        requesting_session: SessionId,
        target_session: SessionId,
    ) -> Result<ProcessScopedTranscriptRead, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let Some(requesting_placement) =
            load_process_session_placement(&mut transaction, requesting_session).await?
        else {
            return Err(ProcessReadCorruption::Missing("requesting session placement").into());
        };
        let Some(target_placement) =
            load_process_session_placement(&mut transaction, target_session).await?
        else {
            transaction.commit().await?;
            return Ok(ProcessScopedTranscriptRead::TargetNotFound);
        };
        match requesting_placement
            .placement()
            .decide_cross_session_read(target_placement.placement())
        {
            SessionReadScopeDecision::Allowed => Ok(ProcessScopedTranscriptRead::Opened(Box::new(
                open_transcript_in_transaction(
                    transaction,
                    target_session,
                    self.automatic_reconciliation_attempt_budget,
                )
                .await?,
            ))),
            SessionReadScopeDecision::Refused(refusal) => {
                transaction.commit().await?;
                Ok(ProcessScopedTranscriptRead::Refused(refusal))
            }
        }
    }
}
