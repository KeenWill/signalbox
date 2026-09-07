//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Reads use one read-only repeatable-read transaction so
//! the hub can map a complete, stable projection explicitly.

mod entry;
mod reader;
mod session;
mod transcript_types;
mod turn_decode;

use entry::decode_transcript_entry;

use session::{PendingSessionSummary, decode_session_defaults_value};

pub use reader::ProcessTranscriptReader;
pub use session::{
    ProcessModelSelection, ProcessRunnerConnectionHealth, ProcessRunnerProjection,
    ProcessRunnerProjectionState, ProcessScopedTranscriptRead, ProcessSessionDefaults,
    ProcessSessionDefaultsRead, ProcessSessionSummary, ProcessSessionSummaryReader,
};
pub use transcript_types::{
    ProcessAttachmentPreparationFailureCause, ProcessCurrentModelCall,
    ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
    ProcessFailedTerminalModelCall, ProcessImportedContentKind, ProcessImportedSourceSpeaker,
    ProcessModelCallInputTokenSemantics, ProcessModelCallRecoveryPrecondition,
    ProcessModelCallTokenUsage, ProcessModelCallUsageProvenance,
    ProcessProviderModelCallFailureCause, ProcessReconciliationOperation, ProcessSessionAncestry,
    ProcessToolApproval, ProcessToolExecutionResultDisposition, ProcessTranscriptEntry,
    ProcessTranscriptItem, ProcessTranscriptModelCallUsage, ProcessTranscriptSnapshot,
    ProcessTranscriptSummary, ProcessTranscriptTurn, ProcessTurnState,
};

use std::collections::VecDeque;

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, CredentialProfileName, DirectModelSelection,
    FrozenAliasDefinition, FrozenModelSelection, ImportedConversationId, ImportedSourceAttestation,
    ImportedTranscriptContent, ImportedTranscriptEntryId, ModelAlias, ModelCallId,
    ModelSelectionRequest, ProviderModelIdentity, ResolvedProviderTarget, RunnerCapabilityClass,
    RunnerGeneration, RunnerId, RunnerSelector, RunnerWorkingDirectory, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionId, SessionReadScopeDecision, ToolApprovalDecider,
    ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput, ToolDecisionRationale,
    ToolDenialReason, ToolRequestId, TurnId, TurnModelSettingsResolved, UserContent,
    VersionedSessionPlacement, WorkspaceRepositoryKey,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    mapping::{
        ToolApprovalDecisionSourceStorageKind, ToolAttemptDispositionStorageKind,
        defaults_version_from_numeric, durable_command_id_from_uuid,
        model_change_adjustments_from_json, model_settings_from_json,
        model_settings_overlay_from_json, runner_sandbox_from_str, session_id_from_uuid,
        session_id_to_uuid, tool_approval_decision_source_from_str,
        tool_attempt_disposition_from_str,
    },
    outbox::{
        DispatchedDelegationOutcome, DispatchedDelegationProvenance, DispatchedDelegationReason,
        decode_delegation_outcome, decode_delegation_provenance, decode_delegation_reason,
    },
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";
/// Hard safety ceiling on session identities read ahead by one summary cursor;
/// it bounds page memory and the number of histories authenticated per query.
const SESSION_SUMMARY_PAGE_SIZE: i64 = 64;

#[derive(signalbox_derive::OperatorError)]
/// A committed read shape that cannot form the closed process projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessReadCorruption {
    #[error("process read is missing {field_0}")]
    /// One required row or field was absent.
    Missing(&'static str),
    #[error("process read has unsupported {field}: {value}")]
    /// A closed storage discriminator had no admitted mapping.
    Unsupported {
        /// Storage field containing the discriminator.
        field: &'static str,
        /// Unsupported durable spelling.
        value: String,
    },
    #[error("process read has inconsistent {field_0}")]
    /// Related durable fields disagreed.
    Inconsistent(&'static str),
    #[error("process read has invalid {field_0}")]
    /// A stored ordinal was not an admitted unsigned integer.
    InvalidOrdinal(&'static str),
}

#[derive(signalbox_derive::OperatorError)]
/// PostgreSQL failure or fail-closed projection corruption.
#[derive(Debug)]
pub enum ProcessReadError {
    #[error("process read database operation failed")]
    /// PostgreSQL could not complete the repeatable-read transaction.
    Database(#[source] sqlx::Error),
    #[error(transparent)]
    /// Committed rows could not form the closed projection.
    Corruption(#[source] ProcessReadCorruption),
}

impl From<sqlx::Error> for ProcessReadError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProcessReadCorruption> for ProcessReadError {
    fn from(error: ProcessReadCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL-backed process read boundary.
#[derive(Clone, Debug)]
pub struct ProcessReadRepository {
    pool: PgPool,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
}

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
    /// compaction range, preserving their one-based physical positions.
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
                transcript_request.arguments_text AS transcript_tool_arguments,
                result_attempt.terminal_disposition_kind AS result_disposition,
                result_attempt.result_text AS result_text,
                result_attempt.error_kind AS result_error_kind,
                result_attempt.error_detail AS result_error_detail,
                transcript_approval.decision_kind AS transcript_decision_kind,
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

async fn load_process_session_placement(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, ProcessReadError> {
    crate::session_placement::load_current(transaction, session)
        .await
        .map_err(map_session_placement_read_error)
}

fn map_session_placement_read_error(
    error: crate::session_placement::SessionPlacementRepositoryError,
) -> ProcessReadError {
    use crate::session_placement::SessionPlacementRepositoryError;

    match error {
        SessionPlacementRepositoryError::Database(error)
        | SessionPlacementRepositoryError::CommitAmbiguous(error) => {
            ProcessReadError::Database(error)
        }
        SessionPlacementRepositoryError::InvalidCommandId
        | SessionPlacementRepositoryError::Corruption(_) => {
            ProcessReadCorruption::Inconsistent("session placement").into()
        }
    }
}

async fn open_transcript_in_transaction(
    mut transaction: Transaction<'static, Postgres>,
    requested_session: SessionId,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
) -> Result<ProcessTranscriptReader, ProcessReadError> {
    let stored_cursor: Option<Decimal> = sqlx::query_scalar(
        "SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let cursor = decode_nonnegative(
        stored_cursor.ok_or(ProcessReadCorruption::Missing("outbox sequence state"))?,
        "outbox cursor",
    )?;
    let runner = load_process_runner_projection(&mut transaction, requested_session).await?;
    let lineage_tip = load_execution_lineage_tip(&mut transaction, requested_session).await?;
    // Imported seed integrity remains fail-closed on every transcript open:
    // native lineage supersedes the seed as the rendered frontier, not as an
    // integrity fact.
    let imported_seed =
        load_checked_imported_seed_frontier(&mut transaction, requested_session).await?;
    let expected_turn_count =
        load_transcript_turn_count(&mut transaction, requested_session).await?;
    let expected_model_call_count =
        load_terminal_model_call_count(&mut transaction, requested_session).await?;
    Ok(ProcessTranscriptReader {
        transaction: Some(transaction),
        session: requested_session,
        cursor,
        runner,
        lineage_tip,
        latest_frontier: if lineage_tip.is_none() {
            imported_seed
        } else {
            None
        },
        expected_turn_count,
        turn_count: 0,
        next_turn_after: None,
        turns_complete: false,
        expected_model_call_count,
        model_call_count: 0,
        next_model_call_after: None,
        model_calls_complete: false,
        entry_count: None,
        next_entry_index: 0,
        summary: None,
        automatic_reconciliation_attempt_budget,
    })
}

pub(crate) async fn load_process_runner_projection(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ProcessRunnerProjection>, ProcessReadError> {
    let row = sqlx::query(
        "SELECT placement.selector_kind, placement.selector_runner_id,
                placement.selector_capability_class,
                placement.directory_selection_kind,
                placement.requested_working_directory,
                placement.requested_credential_profile_name,
                placement.workspace_requirement_kind,
                placement.requested_repository_key,
                placement.requested_sandbox_profile,
                placement.placement_revision, placement.state_kind,
                placement.pinned_runner_id, placement.lost_runner_id,
                placement.loss_source_kind,
                connection.state_kind AS connection_state_kind
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = current_placement.session_id
            AND placement.event_ordinal = current_placement.event_ordinal
           LEFT JOIN LATERAL (
                SELECT state_kind
                  FROM runner_connection_event
                 WHERE enrollment_id = placement.registration_enrollment_id
                 ORDER BY connection_epoch DESC, event_ordinal DESC
                 LIMIT 1
           ) AS connection ON placement.state_kind = 'pinned'
          WHERE current_placement.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    let selector_kind: String = required(&row, "selector_kind")?;
    let selector_runner: Option<Uuid> = row.try_get("selector_runner_id")?;
    let selector_capability: Option<String> = row.try_get("selector_capability_class")?;
    let selector = match (selector_kind.as_str(), selector_runner, selector_capability) {
        ("identity", Some(runner), None) => RunnerSelector::Identity(RunnerId::from_uuid(runner)),
        ("capability_class", None, Some(capability)) => RunnerSelector::CapabilityClass(
            RunnerCapabilityClass::try_new(capability)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner selector"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner selector").into()),
    };

    let directory_kind: String = required(&row, "directory_selection_kind")?;
    let requested_directory: Option<String> = row.try_get("requested_working_directory")?;
    let working_directory = match (directory_kind.as_str(), requested_directory) {
        ("runner_default", None) => None,
        ("exact", Some(directory)) => Some(
            RunnerWorkingDirectory::try_new(directory)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner working directory"))?,
        ),
        _ => {
            return Err(
                ProcessReadCorruption::Inconsistent("runner working directory selection").into(),
            );
        }
    };

    let workspace_kind: String = required(&row, "workspace_requirement_kind")?;
    let requested_repository: Option<String> = row.try_get("requested_repository_key")?;
    let repository = match (workspace_kind.as_str(), requested_repository) {
        ("none", None) => None,
        ("repository_worktree", Some(repository)) => Some(
            WorkspaceRepositoryKey::try_new(repository)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner repository key"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner workspace request").into()),
    };

    let credential_profile = row
        .try_get::<Option<String>, _>("requested_credential_profile_name")?
        .map(CredentialProfileName::try_new)
        .transpose()
        .map_err(|_| ProcessReadCorruption::Inconsistent("runner credential profile"))?;
    let sandbox_name: String = required(&row, "requested_sandbox_profile")?;
    let sandbox =
        runner_sandbox_from_str(&sandbox_name).ok_or(ProcessReadCorruption::Unsupported {
            field: "runner sandbox profile",
            value: sandbox_name,
        })?;
    let placement_revision = RunnerGeneration::try_from_u64(decode_positive(
        required(&row, "placement_revision")?,
        "runner placement revision",
    )?)
    .ok_or(ProcessReadCorruption::InvalidOrdinal(
        "runner placement revision",
    ))?;

    let state_kind: String = required(&row, "state_kind")?;
    let pinned_runner = row
        .try_get::<Option<Uuid>, _>("pinned_runner_id")?
        .map(RunnerId::from_uuid);
    let lost_runner = row
        .try_get::<Option<Uuid>, _>("lost_runner_id")?
        .map(RunnerId::from_uuid);
    let loss_source: Option<String> = row.try_get("loss_source_kind")?;
    let connection_state: Option<String> = row.try_get("connection_state_kind")?;
    let (runner, state) = match (
        state_kind.as_str(),
        pinned_runner,
        lost_runner,
        loss_source.as_deref(),
    ) {
        ("unpinned", None, None, None) => (None, ProcessRunnerProjectionState::Unpinned),
        ("pinned", Some(runner), None, None) => {
            (Some(runner), ProcessRunnerProjectionState::Pinned)
        }
        ("runner_lost_before_pin", None, Some(runner), None) => (
            Some(runner),
            ProcessRunnerProjectionState::RunnerLostBeforePin,
        ),
        ("runner_lost", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerLost)
        }
        ("runner_abandoned", None, Some(lost), None) => {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        ("runner_abandoned", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        _ => return Err(ProcessReadCorruption::Inconsistent("runner placement state").into()),
    };
    let connection_health = match (state, connection_state.as_deref()) {
        (ProcessRunnerProjectionState::Pinned, Some("connected")) => {
            Some(ProcessRunnerConnectionHealth::Connected)
        }
        (ProcessRunnerProjectionState::Pinned, Some("suspect")) => {
            Some(ProcessRunnerConnectionHealth::Suspect)
        }
        (ProcessRunnerProjectionState::Pinned, Some("shutdown")) => {
            Some(ProcessRunnerConnectionHealth::Shutdown)
        }
        (ProcessRunnerProjectionState::Pinned, Some("lost")) => {
            Some(ProcessRunnerConnectionHealth::Lost)
        }
        (
            ProcessRunnerProjectionState::Unpinned
            | ProcessRunnerProjectionState::RunnerLostBeforePin
            | ProcessRunnerProjectionState::RunnerLost
            | ProcessRunnerProjectionState::RunnerAbandoned,
            None,
        ) => None,
        _ => return Err(ProcessReadCorruption::Inconsistent("runner connection health").into()),
    };

    Ok(Some(ProcessRunnerProjection {
        selector,
        runner,
        placement_revision,
        sandbox,
        credential_profile,
        repository,
        working_directory,
        connection_health,
        state,
    }))
}

fn decode_process_session_ancestry(
    row: &PgRow,
) -> Result<ProcessSessionAncestry, ProcessReadError> {
    let ancestry: String = required(row, "ancestry_kind")?;
    match ancestry.as_str() {
        "none" => Ok(ProcessSessionAncestry::UserInitiated),
        "imported_conversation" => Ok(ProcessSessionAncestry::ImportedConversation),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "session ancestry kind",
            value: ancestry,
        }
        .into()),
    }
}

async fn load_checked_imported_seed_frontier(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    sqlx::query("SELECT assert_imported_session_seed_complete($1)")
        .bind(session_id_to_uuid(session))
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_seed_validation_error)?;

    let row = sqlx::query(
        "SELECT
            session_row.ancestry_kind,
            seed.seed_context_frontier_id
           FROM session AS session_row
           LEFT JOIN imported_session_seed AS seed
             ON seed.session_id = session_row.session_id
          WHERE session_row.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    let ancestry = decode_process_session_ancestry(&row)?;
    let seed: Option<Uuid> = row.try_get("seed_context_frontier_id")?;
    match (ancestry, seed) {
        (ProcessSessionAncestry::UserInitiated, None) => Ok(None),
        (ProcessSessionAncestry::ImportedConversation, Some(frontier)) => {
            Ok(Some(ContextFrontierId::from_uuid(frontier)))
        }
        _ => Err(ProcessReadCorruption::Inconsistent("imported session seed shape").into()),
    }
}

fn map_seed_validation_error(error: sqlx::Error) -> ProcessReadError {
    let is_integrity_failure = error.as_database_error().is_some_and(|database| {
        matches!(
            database.code().as_deref(),
            Some("23000" | "23502" | "23503" | "23505" | "23514")
        )
    });
    if is_integrity_failure {
        ProcessReadCorruption::Inconsistent("imported session seed").into()
    } else {
        error.into()
    }
}

fn decode_pending_session_summary(
    row: &PgRow,
    placement: VersionedSessionPlacement,
) -> Result<PendingSessionSummary, ProcessReadError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let defaults_version = decode_positive(
        required(row, "defaults_version")?,
        "current defaults version",
    )?;
    let kind: String = required(row, "model_selection_kind")?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model_selection = match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => {
            ProcessModelSelection::Direct(DirectModelSelection::from_uuid(selection))
        }
        ("alias", None, Some(alias)) => ProcessModelSelection::Alias(ModelAlias::from_uuid(alias)),
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection shape").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model selection kind",
                value: kind,
            }
            .into());
        }
    };
    Ok(PendingSessionSummary {
        session,
        defaults_version,
        model_selection,
        placement,
    })
}

struct DecodedTurn {
    turn: ProcessTranscriptTurn,
    start_lineage: Option<DecodedStartLineage>,
    latest_frontier: Option<ContextFrontierId>,
}

#[derive(Debug)]
enum DecodedTurnOrigin {
    AcceptedInput {
        accepted_input: AcceptedInputId,
        content: UserContent,
    },
    DelegatedTask {
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: String,
    },
    DelegationWake {
        first_delivery_sequence: u64,
        through_delivery_sequence: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodedStartLineage {
    FirstInSession,
    After(TurnId),
}

async fn load_execution_lineage_tip(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<TurnId>, ProcessReadError> {
    let row = sqlx::query(
        "WITH RECURSIVE
            started AS (
                SELECT
                    turn_id,
                    start_lineage_kind,
                    immediate_predecessor_turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind IN ('active', 'terminal')
                   AND start_lineage_kind IS NOT NULL
            ),
            chain(turn_id) AS (
                SELECT turn_id
                  FROM started
                 WHERE start_lineage_kind = 'first_in_session'
                UNION
                SELECT child.turn_id
                  FROM started AS child
                  JOIN chain AS predecessor
                    ON child.start_lineage_kind = 'after'
                   AND child.immediate_predecessor_turn_id = predecessor.turn_id
            ),
            tips AS (
                SELECT candidate.turn_id
                  FROM started AS candidate
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM started AS successor
                     WHERE successor.start_lineage_kind = 'after'
                       AND successor.immediate_predecessor_turn_id = candidate.turn_id
                 )
            )
         SELECT
            (SELECT count(*) FROM started) AS started_count,
            (SELECT count(*) FROM started
              WHERE start_lineage_kind = 'first_in_session') AS root_count,
            (SELECT count(*) FROM chain) AS visited_count,
            (SELECT count(*) FROM tips) AS tip_count,
            EXISTS (
                SELECT 1
                  FROM started
                 WHERE start_lineage_kind = 'after'
                 GROUP BY immediate_predecessor_turn_id
                HAVING count(*) > 1
            ) AS branched,
            EXISTS (
                SELECT 1
                  FROM started AS child
                  LEFT JOIN started AS predecessor
                    ON predecessor.turn_id = child.immediate_predecessor_turn_id
                 WHERE child.start_lineage_kind = 'after'
                   AND predecessor.turn_id IS NULL
            ) AS missing_predecessor,
            (SELECT turn_id FROM tips LIMIT 1) AS tip_turn_id",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    decode_execution_lineage_tip(
        decode_database_count(&row, "started_count", "started turn count")?,
        decode_database_count(&row, "root_count", "root turn count")?,
        decode_database_count(&row, "visited_count", "visited turn count")?,
        decode_database_count(&row, "tip_count", "tip turn count")?,
        row.try_get("branched")?,
        row.try_get("missing_predecessor")?,
        row.try_get::<Option<Uuid>, _>("tip_turn_id")?
            .map(TurnId::from_uuid),
    )
}

fn decode_execution_lineage_tip(
    started_count: u64,
    root_count: u64,
    visited_count: u64,
    tip_count: u64,
    branched: bool,
    missing_predecessor: bool,
    tip: Option<TurnId>,
) -> Result<Option<TurnId>, ProcessReadError> {
    if started_count == 0 {
        return if root_count == 0
            && visited_count == 0
            && tip_count == 0
            && !branched
            && !missing_predecessor
            && tip.is_none()
        {
            Ok(None)
        } else {
            Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into())
        };
    }
    if root_count != 1
        || visited_count != started_count
        || tip_count != 1
        || branched
        || missing_predecessor
    {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }
    tip.map(Some)
        .ok_or_else(|| ProcessReadCorruption::Inconsistent("turn execution lineage").into())
}

async fn load_transcript_turn_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_logical_terminal AS logical_terminal
                     WHERE logical_terminal.child_session_id = turn.session_id
                       AND logical_terminal.child_turn_id = turn.turn_id
                )
            )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript turn count").into())
}

async fn load_terminal_model_call_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT
            (SELECT count(*)
               FROM model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')
            +
            (SELECT count(*)
               FROM tool_approval_judge_model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript model-call count").into())
}

async fn load_next_model_call_usage(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<(u64, ModelCallId)>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "WITH terminal_call AS (
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   usage_input_tokens, usage_output_tokens,
                   usage_cache_creation_input_tokens,
                   usage_cache_read_input_tokens
              FROM model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
            UNION ALL
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   input_tokens, output_tokens,
                   cache_creation_input_tokens, cache_read_input_tokens
              FROM tool_approval_judge_model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
         )
         SELECT
            turn.acceptance_position,
            call.turn_id,
            call.model_call_id,
            call.resolved_provider_model_identity_id,
            call.credential_reference,
            call.usage_provenance_kind,
            call.usage_input_includes_cache_tokens,
            call.usage_input_tokens,
            call.usage_output_tokens,
            call.usage_cache_creation_input_tokens,
            call.usage_cache_read_input_tokens
           FROM terminal_call AS call
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
            AND turn.session_id = call.session_id
          WHERE $2::numeric IS NULL
             OR turn.acceptance_position > $2
             OR (
                 turn.acceptance_position = $2
                 AND call.model_call_id > $3
             )
          ORDER BY turn.acceptance_position, call.model_call_id
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(|(position, _)| Decimal::from(position)))
    .bind(after.map(|(_, call)| call.into_uuid()))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

fn decode_model_call_usage(
    row: &PgRow,
) -> Result<(u64, ProcessTranscriptModelCallUsage), ProcessReadError> {
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "model-call turn acceptance position",
    )?;
    let provenance_value = required::<String>(row, "usage_provenance_kind")?;
    let Some(provenance) = ProcessModelCallUsageProvenance::from_storage(provenance_value.as_str())
    else {
        return Err(ProcessReadCorruption::Unsupported {
            field: "usage_provenance_kind",
            value: provenance_value,
        }
        .into());
    };
    let input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call input tokens"))
        .transpose()?;
    let output_tokens = row
        .try_get::<Option<Decimal>, _>("usage_output_tokens")?
        .map(|value| decode_nonnegative(value, "model-call output tokens"))
        .transpose()?;
    let cache_creation_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_creation_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-creation input tokens"))
        .transpose()?;
    let cache_read_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_read_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-read input tokens"))
        .transpose()?;
    Ok((
        acceptance_position,
        ProcessTranscriptModelCallUsage {
            turn: TurnId::from_uuid(required(row, "turn_id")?),
            call: ModelCallId::from_uuid(required(row, "model_call_id")?),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                row,
                "resolved_provider_model_identity_id",
            )?)),
            credential_profile: required(row, "credential_reference")?,
            input_token_semantics: ProcessModelCallInputTokenSemantics::from_storage(
                row.try_get("usage_input_includes_cache_tokens")?,
            ),
            provenance,
            usage: ProcessModelCallTokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            },
        },
    ))
}

async fn load_next_transcript_turn(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<u64>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "SELECT
            turn.turn_id,
            turn.session_id AS turn_session_id,
            turn.acceptance_position,
            turn.origin_kind,
            turn.origin_accepted_input_id,
            turn.state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.child_wait_request_id,
            turn.current_attempt_id,
            turn.terminal_disposition_kind,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.runner_recovery_runner_id,
            turn.runner_recovery_placement_revision,
            turn.runner_recovery_tool_attempt_id,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            terminal_call.terminal_disposition_kind
                AS terminal_model_call_disposition_kind,
            terminal_call.terminal_provider_failure_cause
                AS terminal_model_call_provider_failure_cause,
            terminal_call.terminal_attachment_preparation_failure_cause
                AS terminal_model_call_attachment_preparation_failure_cause,
            accepted.accepted_input_id,
            accepted.acceptance_position AS accepted_position,
            accepted.origin_turn_id,
            CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                 ELSE accepted_input_content_parts_json(
                    accepted.accepted_input_id)
            END AS accepted_content,
            task.spawning_tool_request_id AS delegated_spawning_tool_request_id,
            task.task_content AS delegated_task_content,
            relation.parent_session_id AS delegated_parent_session_id,
            relation.parent_turn_id AS delegated_parent_turn_id,
            wake.first_delivery_sequence AS delegated_wake_first_delivery_sequence,
            wake.through_delivery_sequence AS delegated_wake_through_delivery_sequence,
            child_wait.spawning_tool_request_id AS child_wait_spawning_request_id,
            child_wait.child_session_id AS child_wait_child_session_id,
            accepted.model_settings_override AS accepted_model_settings_override,
            settings.accepted_input_id AS settings_accepted_input_id,
            settings.turn_id AS settings_turn_id,
            settings.session_id AS settings_session_id,
            settings.defaults_version AS settings_defaults_version,
            settings.selected_direct_model_id AS settings_selected_direct_id,
            settings.per_call_model_settings AS settings_per_call_model_settings,
            settings.resolved_model_settings AS settings_resolved_model_settings,
            settings.adjusted_from_selection_id AS settings_adjusted_from_selection_id,
            settings.adjustments AS settings_adjustments,
            configuration_origin.defaults_version AS origin_defaults_version,
            configuration_origin.requested_model_kind AS origin_requested_model_kind,
            configuration_origin.requested_direct_model_selection_id
                AS origin_requested_direct_id,
            configuration_origin.requested_model_alias_id AS origin_requested_alias_id,
            configuration_origin.frozen_model_kind AS origin_frozen_model_kind,
            configuration_origin.frozen_direct_model_selection_id AS origin_frozen_direct_id,
            configuration_origin.frozen_model_alias_id AS origin_frozen_alias_id,
            configuration_origin.frozen_alias_selected_direct_id
                AS origin_frozen_alias_selected_direct_id,
            configuration_origin.model_settings_evidence_required
                AS origin_model_settings_evidence_required,
            origin_accepted.model_settings_override
                AS origin_model_settings_override,
            origin_defaults.model_settings AS origin_defaults_model_settings,
            current_call.model_call_id AS current_model_call_id,
            current_call.state_kind AS current_model_call_state_kind,
            current_call.context_frontier_id AS current_model_call_frontier_id,
            recovery_call.context_frontier_id AS recovery_model_call_frontier_id,
            automatic_reconciliation.state_kind
                AS automatic_reconciliation_state_kind,
            automatic_reconciliation.attempt_count
                AS automatic_reconciliation_attempt_count,
            automatic_reconciliation.model_call_id
                AS automatic_reconciliation_model_call_id,
            automatic_reconciliation.tool_attempt_id
                AS automatic_reconciliation_tool_attempt_id,
            active_tool_round.boundary_frontier_id AS active_tool_round_frontier_id,
            logical_terminal.spawning_tool_request_id
                AS logical_terminal_spawning_request_id,
            logical_terminal.terminal_frontier_id
                AS logical_terminal_frontier_id,
            logical_terminal_event.outcome_kind
                AS logical_terminal_outcome_kind,
            logical_terminal_event.reason_kind
                AS logical_terminal_reason_kind,
            logical_terminal_event.provenance_kind AS provenance_kind,
            logical_terminal_event.provenance_session_id AS provenance_session_id,
            logical_terminal_event.provenance_turn_id AS provenance_turn_id,
            logical_terminal_event.provenance_goal_generation
                AS provenance_goal_generation,
            logical_terminal_event.provenance_command_id AS provenance_command_id
           FROM turn_lifecycle AS turn
           LEFT JOIN accepted_input AS accepted
             ON accepted.accepted_input_id = turn.origin_accepted_input_id
            AND accepted.session_id = turn.session_id
           LEFT JOIN session_delegation_initial_task AS task
             ON task.turn_id = turn.turn_id
            AND task.child_session_id = turn.session_id
           LEFT JOIN session_delegation_wake_turn_origin AS wake
             ON wake.turn_id = turn.turn_id
            AND wake.recipient_session_id = turn.session_id
            AND wake.admission_position = turn.acceptance_position
           LEFT JOIN session_delegation AS relation
             ON relation.spawning_tool_request_id = task.spawning_tool_request_id
            AND relation.child_session_id = task.child_session_id
           LEFT JOIN session_delegation_wait AS child_wait
             ON child_wait.awaiting_tool_request_id = turn.child_wait_request_id
            AND child_wait.parent_turn_id = turn.turn_id
            AND child_wait.parent_session_id = turn.session_id
            AND child_wait.wait_mode = 'foreground'
           LEFT JOIN turn_model_settings_resolved AS settings
             ON settings.accepted_input_id = turn.origin_accepted_input_id
            AND settings.turn_id = turn.turn_id
            AND settings.session_id = turn.session_id
           LEFT JOIN LATERAL (
                WITH RECURSIVE configuration_chain AS (
                    SELECT queued.*
                      FROM queued_input_origin AS queued
                     WHERE queued.accepted_input_id = turn.origin_accepted_input_id
                       AND queued.turn_id = turn.turn_id
                       AND queued.session_id = turn.session_id
                    UNION
                    SELECT source.*
                      FROM configuration_chain AS current
                      JOIN queued_input_origin AS source
                        ON source.turn_id = current.source_configuration_turn_id
                       AND source.session_id = current.session_id
                )
                SELECT *
                  FROM configuration_chain
                 WHERE source_configuration_turn_id IS NULL
           ) AS configuration_origin ON TRUE
           LEFT JOIN accepted_input AS origin_accepted
             ON origin_accepted.accepted_input_id =
                configuration_origin.accepted_input_id
            AND origin_accepted.session_id = configuration_origin.session_id
            AND origin_accepted.origin_turn_id = configuration_origin.turn_id
           LEFT JOIN session_defaults_version AS origin_defaults
             ON origin_defaults.session_id = configuration_origin.session_id
            AND origin_defaults.version = configuration_origin.defaults_version
           LEFT JOIN model_call AS current_call
             ON current_call.turn_attempt_id = turn.current_attempt_id
            AND current_call.turn_id = turn.turn_id
            AND current_call.session_id = turn.session_id
            AND current_call.state_kind <> 'terminal'
           LEFT JOIN model_call AS recovery_call
             ON recovery_call.model_call_id = turn.recovery_model_call_id
            AND recovery_call.turn_attempt_id = turn.current_attempt_id
            AND recovery_call.turn_id = turn.turn_id
            AND recovery_call.session_id = turn.session_id
            AND recovery_call.state_kind = 'terminal'
           LEFT JOIN automatic_reconciliation AS automatic_reconciliation
             ON automatic_reconciliation.turn_id = turn.turn_id
            AND automatic_reconciliation.session_id = turn.session_id
           LEFT JOIN model_call AS terminal_call
             ON terminal_call.model_call_id = turn.terminal_model_call_id
            AND terminal_call.turn_attempt_id = turn.terminal_attempt_id
            AND terminal_call.turn_id = turn.turn_id
            AND terminal_call.session_id = turn.session_id
            AND terminal_call.state_kind = 'terminal'
           LEFT JOIN tool_round AS active_tool_round
             ON active_tool_round.producing_model_call_id =
                turn.active_tool_round_call_id
            AND active_tool_round.turn_id = turn.turn_id
            AND active_tool_round.session_id = turn.session_id
           LEFT JOIN session_delegation_logical_terminal AS logical_terminal
             ON logical_terminal.child_session_id = turn.session_id
            AND logical_terminal.child_turn_id = turn.turn_id
           LEFT JOIN session_delegation_event AS logical_terminal_event
             ON logical_terminal_event.spawning_tool_request_id =
                    logical_terminal.spawning_tool_request_id
            AND logical_terminal_event.event_kind = 'outcome_recorded'
            AND logical_terminal_event.provenance_command_id =
                    logical_terminal.root_command_id
            AND logical_terminal_event.outcome_kind = CASE
                    logical_terminal.disposition_kind
                    WHEN 'stopped' THEN 'child_stopped'
                    WHEN 'cancelled' THEN 'child_cancelled'
                END
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR logical_terminal.child_turn_id IS NOT NULL
            )
            AND ($2::numeric IS NULL OR turn.acceptance_position > $2)
          ORDER BY turn.acceptance_position
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(Decimal::from))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

fn decode_provider_failure_cause(
    value: &str,
) -> Result<ProcessProviderModelCallFailureCause, ProcessReadError> {
    match value {
        "credential_rejected" => Ok(ProcessProviderModelCallFailureCause::CredentialRejected),
        "permission_denied" => Ok(ProcessProviderModelCallFailureCause::PermissionDenied),
        "invalid_request" => Ok(ProcessProviderModelCallFailureCause::InvalidRequest),
        "target_not_found" => Ok(ProcessProviderModelCallFailureCause::TargetNotFound),
        "request_too_large" => Ok(ProcessProviderModelCallFailureCause::RequestTooLarge),
        "rate_limited" => Ok(ProcessProviderModelCallFailureCause::RateLimited),
        "quota_exhausted" => Ok(ProcessProviderModelCallFailureCause::QuotaExhausted),
        "overloaded" => Ok(ProcessProviderModelCallFailureCause::Overloaded),
        "provider_internal" => Ok(ProcessProviderModelCallFailureCause::ProviderInternal),
        "unrecognized" => Ok(ProcessProviderModelCallFailureCause::Unrecognized),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call provider failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_attachment_preparation_failure_cause(
    value: &str,
) -> Result<ProcessAttachmentPreparationFailureCause, ProcessReadError> {
    match value {
        "too_large" => Ok(ProcessAttachmentPreparationFailureCause::TooLarge),
        "missing" => Ok(ProcessAttachmentPreparationFailureCause::Missing),
        "corrupt" => Ok(ProcessAttachmentPreparationFailureCause::Corrupt),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call attachment-preparation failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_database_count(
    row: &PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u64, ProcessReadError> {
    let count: i64 = row.try_get(column)?;
    u64::try_from(count).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field).into())
}

#[allow(clippy::too_many_arguments)]
fn decode_transcript_turn_origin(
    origin_kind: String,
    origin_accepted_input: Option<Uuid>,
    accepted_input: Option<Uuid>,
    accepted_position: Option<Decimal>,
    accepted_origin: Option<Uuid>,
    accepted_content: Option<Value>,
    delegated_spawning_request: Option<Uuid>,
    delegated_parent_session: Option<Uuid>,
    delegated_parent_turn: Option<Uuid>,
    delegated_task_content: Option<String>,
    delegated_wake_first: Option<Decimal>,
    delegated_wake_through: Option<Decimal>,
    turn: TurnId,
    acceptance_position: u64,
) -> Result<DecodedTurnOrigin, ProcessReadError> {
    match (
        origin_kind.as_str(),
        origin_accepted_input,
        accepted_input,
        accepted_position,
        accepted_origin,
        accepted_content,
        delegated_spawning_request,
        delegated_parent_session,
        delegated_parent_turn,
        delegated_task_content,
        delegated_wake_first,
        delegated_wake_through,
    ) {
        (
            "accepted_input",
            Some(origin_accepted_input),
            Some(accepted_input),
            Some(accepted_position),
            Some(accepted_origin),
            Some(content),
            None,
            None,
            None,
            None,
            None,
            None,
        ) => {
            let accepted_position = decode_positive(accepted_position, "accepted input position")?;
            let content = crate::user_content::decode(content)
                .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted-input content"))?;
            if origin_accepted_input != accepted_input
                || accepted_position != acceptance_position
                || accepted_origin != turn.into_uuid()
            {
                return Err(
                    ProcessReadCorruption::Inconsistent("turn accepted-input correlation").into(),
                );
            }
            Ok(DecodedTurnOrigin::AcceptedInput {
                accepted_input: AcceptedInputId::from_uuid(accepted_input),
                content,
            })
        }
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            Some(spawning_request),
            Some(parent_session),
            Some(parent_turn),
            Some(content),
            None,
            None,
        ) if !content.is_empty() => Ok(DecodedTurnOrigin::DelegatedTask {
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            parent_session: SessionId::from_uuid(parent_session),
            parent_turn: TurnId::from_uuid(parent_turn),
            content,
        }),
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(first),
            Some(through),
        ) => {
            let first = decode_positive(first, "delegation wake first delivery sequence")?;
            let through = decode_positive(through, "delegation wake through delivery sequence")?;
            if first > through {
                return Err(
                    ProcessReadCorruption::Inconsistent("delegation wake delivery range").into(),
                );
            }
            Ok(DecodedTurnOrigin::DelegationWake {
                first_delivery_sequence: first,
                through_delivery_sequence: through,
            })
        }
        ("accepted_input" | "delegation", ..) => {
            Err(ProcessReadCorruption::Inconsistent("turn origin correlation").into())
        }
        (value, ..) => Err(ProcessReadCorruption::Unsupported {
            field: "turn origin kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_transcript_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
) -> Result<ModelSelectionRequest, ProcessReadError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => {
            Err(ProcessReadCorruption::Inconsistent("turn requested model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn requested model kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_transcript_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ProcessReadError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(ProcessReadCorruption::Inconsistent("turn frozen model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn frozen model kind",
            value: kind,
        }
        .into()),
    }
}

fn requested_from_transcript_frozen(selection: &FrozenModelSelection) -> ModelSelectionRequest {
    match selection {
        FrozenModelSelection::Direct(selection) => ModelSelectionRequest::Direct(*selection),
        FrozenModelSelection::FrozenAlias { alias, .. } => ModelSelectionRequest::Alias(*alias),
    }
}

fn decode_transcript_turn_model_settings(
    row: &PgRow,
    turn: TurnId,
    accepted_input: AcceptedInputId,
) -> Result<Option<TurnModelSettingsResolved>, ProcessReadError> {
    let stored_accepted: Option<Uuid> = row.try_get("settings_accepted_input_id")?;
    let stored_turn: Option<Uuid> = row.try_get("settings_turn_id")?;
    let stored_session: Option<Uuid> = row.try_get("settings_session_id")?;
    let stored_defaults: Option<Decimal> = row.try_get("settings_defaults_version")?;
    let stored_selected: Option<Uuid> = row.try_get("settings_selected_direct_id")?;
    let stored_per_call: Option<Value> = row.try_get("settings_per_call_model_settings")?;
    let stored_settings: Option<Value> = row.try_get("settings_resolved_model_settings")?;
    let stored_adjustments: Option<Value> = row.try_get("settings_adjustments")?;
    let absent = stored_accepted.is_none()
        && stored_turn.is_none()
        && stored_session.is_none()
        && stored_defaults.is_none()
        && stored_selected.is_none()
        && stored_per_call.is_none()
        && stored_settings.is_none()
        && stored_adjustments.is_none();
    if absent {
        let evidence_required: bool = required(row, "origin_model_settings_evidence_required")?;
        return if evidence_required {
            Err(ProcessReadCorruption::Missing("turn model settings evidence").into())
        } else {
            Ok(None)
        };
    }
    let (Some(stored_accepted), Some(stored_turn), Some(stored_session), Some(stored_defaults)) = (
        stored_accepted,
        stored_turn,
        stored_session,
        stored_defaults,
    ) else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_selected) = stored_selected else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_per_call) = stored_per_call else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_settings) = stored_settings else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_adjustments) = stored_adjustments else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let turn_session: Uuid = required(row, "turn_session_id")?;
    if AcceptedInputId::from_uuid(stored_accepted) != accepted_input
        || TurnId::from_uuid(stored_turn) != turn
        || stored_session != turn_session
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings identity").into());
    }
    let defaults_version = defaults_version_from_numeric(stored_defaults)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn model settings version"))?;
    let origin_defaults = defaults_version_from_numeric(required(row, "origin_defaults_version")?)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn origin defaults version"))?;
    let requested = decode_transcript_model_selection(
        required(row, "origin_requested_model_kind")?,
        row.try_get("origin_requested_direct_id")?,
        row.try_get("origin_requested_alias_id")?,
    )?;
    let frozen = decode_transcript_frozen_model(
        required(row, "origin_frozen_model_kind")?,
        row.try_get("origin_frozen_direct_id")?,
        row.try_get("origin_frozen_alias_id")?,
        row.try_get("origin_frozen_alias_selected_direct_id")?,
    )?;
    let per_call = model_settings_overlay_from_json(stored_per_call)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn per-call model settings"))?;
    let origin_per_call =
        model_settings_overlay_from_json(required(row, "origin_model_settings_override")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted model settings"))?;
    if defaults_version != origin_defaults
        || requested != requested_from_transcript_frozen(&frozen)
        || frozen.selected_direct().into_uuid() != stored_selected
        || per_call != origin_per_call
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings origin").into());
    }
    let event = TurnModelSettingsResolved::try_new(
        accepted_input,
        turn,
        defaults_version,
        frozen,
        per_call,
        model_settings_from_json(stored_settings)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn resolved model settings"))?,
        row.try_get::<Option<Uuid>, _>("settings_adjusted_from_selection_id")?
            .map(DirectModelSelection::from_uuid),
        model_change_adjustments_from_json(stored_adjustments)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn model setting adjustments"))?,
    )
    .ok_or(ProcessReadCorruption::Inconsistent(
        "turn model settings evidence",
    ))?;
    let origin_defaults =
        model_settings_from_json(required(row, "origin_defaults_model_settings")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn defaults model settings"))?;
    if !crate::model_settings_resolution::matches_defaults(&event, origin_defaults) {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings defaults").into());
    }
    Ok(Some(event))
}

fn admitted_automatic_reconciliation_attempts(
    attempts: i32,
    exhausted: bool,
    budget: Option<Option<u32>>,
) -> Result<u32, ProcessReadError> {
    let attempts = u32::try_from(attempts).map_err(|_| {
        ProcessReadCorruption::Inconsistent("automatic reconciliation attempt count")
    })?;
    let admitted = if exhausted {
        match budget {
            Some(Some(budget)) => attempts == budget,
            Some(None) => false,
            None => attempts > 0,
        }
    } else {
        budget.is_none_or(|budget| budget.is_none_or(|budget| attempts <= budget))
    };
    admitted
        .then_some(attempts)
        .ok_or(ProcessReadCorruption::Inconsistent(
            "automatic reconciliation attempt budget",
        ))
        .map_err(Into::into)
}

#[derive(Clone, Copy)]
struct LogicalDelegationTerminalProjection {
    spawning_request: ToolRequestId,
    terminal_frontier: ContextFrontierId,
    outcome: DispatchedDelegationOutcome,
    reason: DispatchedDelegationReason,
    provenance: DispatchedDelegationProvenance,
}

fn decode_logical_delegation_terminal(
    row: &PgRow,
) -> Result<Option<LogicalDelegationTerminalProjection>, ProcessReadError> {
    let spawning_request: Option<Uuid> = row.try_get("logical_terminal_spawning_request_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("logical_terminal_frontier_id")?;
    let outcome: Option<String> = row.try_get("logical_terminal_outcome_kind")?;
    let reason: Option<String> = row.try_get("logical_terminal_reason_kind")?;
    match (spawning_request, outcome.as_deref(), reason.as_deref()) {
        (None, None, None) => Ok(None),
        (Some(spawning_request), Some(outcome), Some(reason)) => {
            let terminal_frontier = terminal_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("logical delegation terminal frontier"),
            )?;
            let outcome = decode_delegation_outcome(outcome).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal outcome")
            })?;
            let reason = decode_delegation_reason(reason).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal reason")
            })?;
            let provenance = decode_delegation_provenance(row).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal provenance")
            })?;
            if !matches!(
                (outcome, reason),
                (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                )
            ) || !matches!(
                provenance,
                DispatchedDelegationProvenance::ParentTurnCommand { .. }
                    | DispatchedDelegationProvenance::ParentGoalCommand { .. }
                    | DispatchedDelegationProvenance::ParentLifecycleCommand { .. }
            ) {
                return Err(ProcessReadCorruption::Inconsistent(
                    "logical delegation terminal shape",
                )
                .into());
            }
            Ok(Some(LogicalDelegationTerminalProjection {
                spawning_request: ToolRequestId::from_uuid(spawning_request),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                outcome,
                reason,
                provenance,
            }))
        }
        _ => Err(
            ProcessReadCorruption::Inconsistent("logical delegation terminal correlation").into(),
        ),
    }
}

fn project_logical_delegation_terminal(
    mut decoded: DecodedTurn,
    logical_terminal: Option<LogicalDelegationTerminalProjection>,
) -> Result<DecodedTurn, ProcessReadError> {
    if let Some(logical_terminal) = logical_terminal {
        decoded.turn.state = ProcessTurnState::DelegationTerminated {
            spawning_request: logical_terminal.spawning_request,
            outcome: logical_terminal.outcome,
            reason: logical_terminal.reason,
            provenance: logical_terminal.provenance,
        };
        // The physical decode observed a mid-execution boundary, but the
        // cascade froze this turn at the logical terminal's frontier and every
        // successor chains from it. Rendering the physical frontier would show
        // execution evidence no successor model call ever saw, and would vanish
        // from the same transcript as soon as a successor activates. A turn
        // terminalized while still queued started no execution lineage at all,
        // so it keeps the absent frontier its start lineage pairs with.
        if decoded.start_lineage.is_some() {
            decoded.latest_frontier = Some(logical_terminal.terminal_frontier);
        }
    }
    Ok(decoded)
}

async fn open_transcript_entry_cursor(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    frontier: ContextFrontierId,
) -> Result<u64, ProcessReadError> {
    let stored_member_count: Option<Decimal> = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let member_count = decode_nonnegative(
        stored_member_count.ok_or(ProcessReadCorruption::Missing("context frontier"))?,
        "context frontier member count",
    )?;
    // The transaction-scoped cursor retains this query's execution state, so
    // every later FETCH advances the same single recursive chain resolution.
    sqlx::query(
        "DECLARE signalbox_process_transcript_entries NO SCROLL CURSOR FOR
         SELECT
            member.actual_member_count,
            member.member_position,
            member.source_session_id,
            member.semantic_entry_id,
            entry.payload_kind,
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
            transcript_request.arguments_text AS transcript_tool_arguments,
            result_attempt.terminal_disposition_kind AS result_disposition,
            result_attempt.result_text AS result_text,
            result_attempt.error_kind AS result_error_kind,
            result_attempt.error_detail AS result_error_detail,
            transcript_approval.decision_kind AS transcript_decision_kind,
            transcript_approval.decision_source AS transcript_decision_source,
            transcript_approval.denial_reason AS transcript_denial_reason,
            transcript_approval.user_command_id AS transcript_user_command_id,
            transcript_approval.delegate_model_selection_id AS transcript_delegate_model_selection_id,
            transcript_approval.delegate_model_call_id AS transcript_delegate_model_call_id,
            transcript_approval.rationale AS transcript_decision_rationale,
            transcript_approval.override_denied_request_id
                AS transcript_override_denied_request_id,
            transcript_override.command_id AS transcript_override_command_id
           FROM (
                SELECT
                    resolved.*,
                    count(*) OVER () AS actual_member_count
                  FROM resolve_context_frontier_members($1, $2) AS resolved
           ) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
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
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(member_count)
}

async fn fetch_next_transcript_entry(
    transaction: &mut Transaction<'static, Postgres>,
    entry_index: u64,
    expected_entry_count: u64,
) -> Result<Option<ProcessTranscriptEntry>, ProcessReadError> {
    let row = sqlx::query("FETCH NEXT FROM signalbox_process_transcript_entries")
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let actual_entry_count: i64 = required(&row, "actual_member_count")?;
    if u64::try_from(actual_entry_count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry count"))?
        != expected_entry_count
    {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier declared membership").into(),
        );
    }
    let member_position =
        entry_index
            .checked_add(1)
            .ok_or(ProcessReadCorruption::InvalidOrdinal(
                "frontier member position",
            ))?;
    let stored_position = decode_positive(
        required(&row, "member_position")?,
        "frontier member position",
    )?;
    if stored_position != member_position {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier contiguous membership").into(),
        );
    }
    decode_transcript_entry(&row, entry_index).map(Some)
}

fn decode_tool_result_disposition(
    value: &str,
) -> Result<ToolAttemptDispositionStorageKind, ProcessReadCorruption> {
    tool_attempt_disposition_from_str(value).ok_or_else(|| ProcessReadCorruption::Unsupported {
        field: "terminal_disposition_kind",
        value: value.to_owned(),
    })
}

fn decode_process_tool_approval(
    row: &PgRow,
) -> Result<Option<ProcessToolApproval>, ProcessReadError> {
    let decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let source: Option<String> = row.try_get("transcript_decision_source")?;
    let denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;
    let user_command: Option<Uuid> = row.try_get("transcript_user_command_id")?;
    let delegate_model: Option<Uuid> = row.try_get("transcript_delegate_model_selection_id")?;
    let delegate_call: Option<Uuid> = row.try_get("transcript_delegate_model_call_id")?;
    let rationale: Option<String> = row.try_get("transcript_decision_rationale")?;
    let override_denied: Option<Uuid> = row.try_get("transcript_override_denied_request_id")?;
    let override_command: Option<Uuid> = row.try_get("transcript_override_command_id")?;
    let Some(source) = source else {
        if decision_kind.is_some()
            || denial_reason.is_some()
            || user_command.is_some()
            || delegate_model.is_some()
            || delegate_call.is_some()
            || rationale.is_some()
            || override_denied.is_some()
            || override_command.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "tool approval projection without source",
            )
            .into());
        }
        return Ok(None);
    };
    let source_kind = tool_approval_decision_source_from_str(&source).ok_or_else(|| {
        ProcessReadError::from(ProcessReadCorruption::Unsupported {
            field: "tool approval decision source",
            value: source,
        })
    })?;
    let decision = match decision_kind.as_deref() {
        Some("approve") if denial_reason.is_none() => ToolApprovalDecision::Approve,
        Some("deny") => ToolApprovalDecision::Deny {
            reason: denial_reason
                .map(ToolDenialReason::try_new)
                .transpose()
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool denial reason"))?,
        },
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("tool approval decision kind").into());
        }
    };
    if source_kind != ToolApprovalDecisionSourceStorageKind::UserOverride
        && (override_denied.is_some() || override_command.is_some())
    {
        return Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into());
    }
    let runtime_safety_decision = ToolApprovalResolutionReconstitutionInput::runtime_safety(
        ToolRequestId::from_uuid(Uuid::nil()),
    )
    .reconstitute()
    .map_err(|_| ProcessReadCorruption::Inconsistent("runtime safety approval evidence"))?;
    match (
        source_kind,
        user_command,
        delegate_model,
        delegate_call,
        rationale,
    ) {
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket,
            None,
            None,
            None,
            None,
        ) if decision == ToolApprovalDecision::Approve => Ok(None),
        (ToolApprovalDecisionSourceStorageKind::RuntimeSafety, None, None, None, None)
            if runtime_safety_decision.decision() == &decision =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::LifecycleClosure, Some(_), None, None, None)
            if decision == (ToolApprovalDecision::Deny { reason: None }) =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::UserCommand, Some(command), None, None, None) => {
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::User {
                    command: durable_command_id_from_uuid(command).map_err(|_| {
                        ProcessReadCorruption::Inconsistent("tool approval user command")
                    })?,
                },
                rationale: None,
            }))
        }
        (
            ToolApprovalDecisionSourceStorageKind::Delegate,
            None,
            Some(model),
            Some(call),
            Some(rationale),
        ) => {
            let rationale = ToolDecisionRationale::try_new(rationale)
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool decision rationale"))?;
            // A delegate denial's stored reason equals the derivation from
            // its rationale — null exactly when the rationale derives
            // nothing — so missing current evidence reads as corruption.
            if let ToolApprovalDecision::Deny { ref reason } = decision
                && *reason != ToolDenialReason::from_rationale(&rationale)
            {
                return Err(ProcessReadCorruption::Inconsistent("delegate denial payload").into());
            }
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::Delegate {
                    model: DirectModelSelection::from_uuid(model),
                    call: ModelCallId::from_uuid(call),
                },
                rationale: Some(rationale),
            }))
        }
        (ToolApprovalDecisionSourceStorageKind::UserOverride, None, None, None, None)
            if decision == ToolApprovalDecision::Approve =>
        {
            match (override_denied, override_command) {
                (Some(denied_request), Some(command)) => Ok(Some(ProcessToolApproval {
                    decision,
                    decider: ToolApprovalDecider::UserOverride {
                        command: durable_command_id_from_uuid(command).map_err(|_| {
                            ProcessReadCorruption::Inconsistent("tool approval override command")
                        })?,
                        denied_request: ToolRequestId::from_uuid(denied_request),
                    },
                    rationale: None,
                })),
                (None, _) | (_, None) => Err(ProcessReadCorruption::Inconsistent(
                    "tool approval provenance shape",
                )
                .into()),
            }
        }
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket
            | ToolApprovalDecisionSourceStorageKind::UserCommand
            | ToolApprovalDecisionSourceStorageKind::Delegate
            | ToolApprovalDecisionSourceStorageKind::RuntimeSafety
            | ToolApprovalDecisionSourceStorageKind::LifecycleClosure
            | ToolApprovalDecisionSourceStorageKind::UserOverride,
            ..,
        ) => Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into()),
    }
}

fn decode_imported_source_speaker(
    value: String,
) -> Result<ProcessImportedSourceSpeaker, ProcessReadError> {
    match value.as_str() {
        "not_attested" => Ok(ProcessImportedSourceSpeaker::NotAttested),
        "attested_absent" => Ok(ProcessImportedSourceSpeaker::AttestedAbsent),
        "attested_user" => Ok(ProcessImportedSourceSpeaker::User),
        "attested_assistant" => Ok(ProcessImportedSourceSpeaker::Assistant),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "imported source speaker",
            value,
        }
        .into()),
    }
}

fn project_imported_entry(
    entry_index: u64,
    source_session: SessionId,
    entry: SemanticTranscriptEntryId,
    imported_conversation: ImportedConversationId,
    imported_entry: ImportedTranscriptEntryId,
    source_speaker: ProcessImportedSourceSpeaker,
    content: ImportedTranscriptContent,
) -> ProcessTranscriptEntry {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)) => {
            ProcessTranscriptEntry::ImportedText {
                entry_index,
                source_session,
                entry,
                imported_conversation,
                imported_entry,
                source_speaker,
                content: content.into_string(),
            }
        }
        content => ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind: match content {
                ImportedTranscriptContent::SourceEvent { .. } => {
                    ProcessImportedContentKind::SourceEvent
                }
                ImportedTranscriptContent::SourceMessageBlock { .. } => {
                    ProcessImportedContentKind::SourceMessageBlock
                }
                ImportedTranscriptContent::Text(_) => ProcessImportedContentKind::Text,
                ImportedTranscriptContent::ToolCall { .. } => ProcessImportedContentKind::ToolCall,
                ImportedTranscriptContent::ToolResult { .. } => {
                    ProcessImportedContentKind::ToolResult
                }
                ImportedTranscriptContent::Thinking { .. } => ProcessImportedContentKind::Thinking,
                ImportedTranscriptContent::RedactedThinking { .. } => {
                    ProcessImportedContentKind::RedactedThinking
                }
                ImportedTranscriptContent::Document { .. } => ProcessImportedContentKind::Document,
                ImportedTranscriptContent::MessageContentAbsent(_) => {
                    ProcessImportedContentKind::MessageContentAbsent
                }
            },
        },
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ProcessReadError>
where
    for<'row> T: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ProcessReadCorruption::Missing(field).into())
}

fn decode_nonnegative(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    if !value.fract().is_zero() || value.is_sign_negative() {
        return Err(ProcessReadCorruption::InvalidOrdinal(field));
    }
    u64::try_from(value).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field))
}

fn decode_positive(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    let value = decode_nonnegative(value, field)?;
    if value == 0 {
        Err(ProcessReadCorruption::InvalidOrdinal(field))
    } else {
        Ok(value)
    }
}

fn decode_runner_generation(
    value: Decimal,
    field: &'static str,
) -> Result<RunnerGeneration, ProcessReadCorruption> {
    RunnerGeneration::try_from_u64(decode_nonnegative(value, field)?)
        .ok_or(ProcessReadCorruption::InvalidOrdinal(field))
}

#[cfg(test)]
mod tests;
