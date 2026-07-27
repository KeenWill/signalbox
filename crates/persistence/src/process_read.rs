//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Reads use one read-only repeatable-read transaction so
//! the hub can map a complete, stable projection explicitly.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DirectModelSelection, ImportedConversationId,
    ImportedSourceAttestation, ImportedTranscriptContent, ImportedTranscriptEntryId, ModelAlias,
    ModelCallId, SemanticTranscriptEntryId, SessionId, ToolAttemptId, ToolRequestId, TurnAttemptId,
    TurnId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    conversation_import_codec::decode_content,
    mapping::{session_id_from_uuid, session_id_to_uuid},
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// One model-selection request in the process-facing session summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelSelection {
    /// A stable direct-selection identity.
    Direct(DirectModelSelection),
    /// A stable alias identity.
    Alias(ModelAlias),
}

/// One current session summary read from a shared transaction snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSessionSummary {
    session: SessionId,
    defaults_version: u64,
    model_selection: ProcessModelSelection,
}

impl ProcessSessionSummary {
    /// Returns the summarized session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the current positive defaults version.
    pub const fn defaults_version(&self) -> u64 {
        self.defaults_version
    }

    /// Returns the current model-selection request.
    pub const fn model_selection(&self) -> ProcessModelSelection {
        self.model_selection
    }
}

/// One complete immutable session-defaults epoch read for the process
/// boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionDefaults {
    session: SessionId,
    version: signalbox_domain::SessionConfigurationDefaultsVersion,
    defaults: signalbox_domain::SessionConfigurationDefaults,
}

impl ProcessSessionDefaults {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the read immutable epoch's version.
    pub const fn version(&self) -> signalbox_domain::SessionConfigurationDefaultsVersion {
        self.version
    }

    /// Borrows the complete defaults value on that epoch.
    pub const fn defaults(&self) -> &signalbox_domain::SessionConfigurationDefaults {
        &self.defaults
    }
}

/// Typed outcome of one session-defaults epoch read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionDefaultsRead {
    /// The selected epoch with its complete defaults value.
    Read(ProcessSessionDefaults),
    /// The selected session does not exist in the read snapshot.
    SessionNotFound,
    /// The session exists but the named epoch was never installed.
    VersionNotFound,
}

fn decode_session_defaults_value(
    row: &PgRow,
) -> Result<signalbox_domain::SessionConfigurationDefaults, ProcessReadError> {
    let kind: String = row
        .try_get::<Option<String>, _>("model_selection_kind")?
        .ok_or(ProcessReadCorruption::Missing("model_selection_kind"))?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            signalbox_domain::ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => {
            signalbox_domain::ModelSelectionRequest::Alias(ModelAlias::from_uuid(value))
        }
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model_selection_kind",
                value: kind,
            }
            .into());
        }
    };
    let tool_approval: String = row
        .try_get::<Option<String>, _>("dangerous_tool_auto_approval")?
        .ok_or(ProcessReadCorruption::Missing(
            "dangerous_tool_auto_approval",
        ))?;
    let dangerous_tool_auto_approval = crate::mapping::dangerous_tool_auto_approval_from_str(
        &tool_approval,
    )
    .ok_or(ProcessReadCorruption::Unsupported {
        field: "dangerous_tool_auto_approval",
        value: tool_approval,
    })?;
    let system_prompt = row
        .try_get::<Option<String>, _>("system_prompt")?
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value)
                .map_err(|_| ProcessReadCorruption::Inconsistent("system prompt admission"))
        })
        .transpose()?;
    Ok(signalbox_domain::SessionConfigurationDefaults::complete(
        model,
        dangerous_tool_auto_approval,
        system_prompt,
    ))
}

/// One repeatable-read session-summary cursor that owns at most one decoded row.
///
/// Call [`Self::next_summary`] until it returns `None`. That terminal call
/// commits the read-only transaction and makes [`Self::summary_count`]
/// available. Dropping a reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessSessionSummaryReader {
    transaction: Option<Transaction<'static, Postgres>>,
    next_session_after: Option<Uuid>,
    summary_count: u64,
    committed_summary_count: Option<u64>,
}

impl ProcessSessionSummaryReader {
    /// Returns the committed count only after [`Self::next_summary`] returned
    /// `None`.
    pub const fn summary_count(&self) -> Option<u64> {
        self.committed_summary_count
    }

    /// Yields one summary in session-identity order without retaining prior
    /// decoded rows.
    pub async fn next_summary(
        &mut self,
    ) -> Result<Option<ProcessSessionSummary>, ProcessReadError> {
        if self.committed_summary_count.is_some() {
            return Ok(None);
        }

        let next_session_after = self.next_session_after;
        let transaction = self.transaction_mut()?;
        let row = sqlx::query(
            "SELECT
                session_row.session_id,
                current_defaults.current_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id
               FROM session AS session_row
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session_row.session_id
               LEFT JOIN session_defaults_version AS selected_defaults
                 ON selected_defaults.session_id = current_defaults.session_id
                AND selected_defaults.version = current_defaults.current_version
              WHERE ($1::uuid IS NULL OR session_row.session_id > $1)
              ORDER BY session_row.session_id
              LIMIT 1",
        )
        .bind(next_session_after)
        .fetch_optional(&mut **transaction)
        .await?;

        if let Some(row) = row {
            let summary = decode_session_summary(&row)?;
            self.next_session_after = Some(session_id_to_uuid(summary.session()));
            self.summary_count =
                self.summary_count
                    .checked_add(1)
                    .ok_or(ProcessReadCorruption::InvalidOrdinal(
                        "session summary count",
                    ))?;
            return Ok(Some(summary));
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.committed_summary_count = Some(self.summary_count);
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

/// Durable state of the current model call attached to an active turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessCurrentModelCallState {
    /// Provider work has not been authorized.
    Prepared,
    /// Provider work was authorized and may have happened.
    InFlight,
    /// Cancellation was durably requested for issued provider work.
    CancellationRequested,
}

/// Current model call attached to the active turn attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessCurrentModelCall {
    call: ModelCallId,
    state: ProcessCurrentModelCallState,
}

impl ProcessCurrentModelCall {
    /// Returns the current model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact durable call state.
    pub const fn state(&self) -> ProcessCurrentModelCallState {
        self.state
    }
}

/// Terminal model-call dispositions admitted by a failed turn projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessFailedModelCallDisposition {
    /// The provider interaction definitively failed.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Optional terminal model-call evidence for a failed turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessFailedTerminalModelCall {
    call: ModelCallId,
    disposition: ProcessFailedModelCallDisposition,
}

impl ProcessFailedTerminalModelCall {
    /// Returns the terminal model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact terminal model-call disposition.
    pub const fn disposition(&self) -> ProcessFailedModelCallDisposition {
        self.disposition
    }
}

/// Whether a session can owe an owner reconciliation decision right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallRecoveryPrecondition {
    /// No such session exists in this snapshot.
    SessionAbsent,
    /// The session exists but no active turn is parked on a model call.
    NoParkedTurn,
    /// The session's active turn is parked on this exact ambiguous call.
    Parked {
        /// The active turn holding the slot until reconciliation.
        turn: TurnId,
    },
}

/// Authoritative lifecycle state for one projected turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input: AcceptedInputId,
        /// Exact accepted owner text.
        content: String,
    },
    /// The current attempt is running.
    ActiveRunning {
        /// Current live attempt.
        current_attempt: TurnAttemptId,
        /// Current provider call, when one has been prepared or authorized.
        current_model_call: Option<ProcessCurrentModelCall>,
    },
    /// The ended attempt is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt whose call is ambiguous.
        ended_attempt: TurnAttemptId,
        /// Ambiguous call awaiting recovery.
        recovery_call: ModelCallId,
    },
    /// The yielded tool batch is parked on an owner decision.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        request: ToolRequestId,
    },
    /// The yielded tool batch is parked on an ambiguous external effect.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt: TurnAttemptId,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_attempt: ToolAttemptId,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Terminal physical attempt, absent only for an evidence-free
        /// recovery failure.
        terminal_attempt: Option<TurnAttemptId>,
        /// Terminal call evidence, absent when no call existed.
        terminal_model_call: Option<ProcessFailedTerminalModelCall>,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Terminal call, absent when cancellation preceded preparation.
        terminal_call: Option<ModelCallId>,
    },
    /// The turn terminalized requiring external reconciliation.
    ReconciliationRequired {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Exact ambiguous terminal operation.
        operation: ProcessReconciliationOperation,
    },
}

/// Exact ambiguous operation exposed by a process transcript projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessReconciliationOperation {
    /// Ambiguous provider call.
    ModelCall(ModelCallId),
    /// Ambiguous tool attempt.
    ToolAttempt(ToolAttemptId),
}

/// One turn in acceptance order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptTurn {
    turn: TurnId,
    acceptance_position: u64,
    state: ProcessTurnState,
}

impl ProcessTranscriptTurn {
    /// Returns the immutable turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the immutable positive acceptance position.
    pub const fn acceptance_position(&self) -> u64 {
        self.acceptance_position
    }

    /// Returns the authoritative lifecycle state.
    pub const fn state(&self) -> &ProcessTurnState {
        &self.state
    }
}

/// Session ancestry relevant to process-protocol compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionAncestry {
    /// Owner-initiated native session.
    OwnerInitiated,
    /// Session seeded from one immutable imported frontier.
    ImportedConversation,
}

/// Exact source-speaker attestation in the conservative process projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested,
    /// The source explicitly supplied no speaker.
    AttestedAbsent,
    /// The source attested user authorship.
    User,
    /// The source attested assistant authorship.
    Assistant,
}

/// Conservative imported content kind exposed by the process read boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedContentKind {
    /// One source event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Text whose value is unattested or explicitly absent.
    Text,
    /// One tool call.
    ToolCall,
    /// One tool result.
    ToolResult,
    /// One thinking block.
    Thinking,
    /// One redacted-thinking block.
    RedactedThinking,
    /// One document block.
    Document,
    /// One typed message-content absence.
    MessageContentAbsent,
}

/// One ordered member of the latest authoritative semantic frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptEntry {
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Turn whose start first observes the identity.
        turn: TurnId,
        /// Immutable defaults epoch bound by that turn.
        defaults_version: u64,
        /// Exact direct model identity frozen for that turn.
        selected: DirectModelSelection,
    },
    /// Exact accepted owner input.
    User {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Accepted-input identity.
        accepted_input: AcceptedInputId,
        /// Origin turn.
        turn: TurnId,
        /// Exact admitted user text.
        content: String,
    },
    /// Exact committed assistant text.
    Assistant {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact committed assistant text.
        content: String,
    },
    /// Assistant tool proposal.
    AssistantToolUse {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact stored tool name.
        name: String,
        /// Exact stored normalized or scrubbed undecodable arguments.
        arguments: String,
    },
    /// Executed tool result reference.
    ToolExecutionResult {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact physical tool attempt.
        attempt: ToolAttemptId,
        /// Exact provider-visible result content.
        content: String,
    },
    /// Owner or policy denied one tool request.
    ToolDenied {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact denied request.
        request: ToolRequestId,
        /// Exact provider-visible denial content.
        content: String,
    },
    /// The turn ended before one tool request resolved ordinarily.
    ToolClosed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact closed request.
        request: ToolRequestId,
        /// Exact provider-visible terminal-closure content.
        content: String,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Failed turn.
        turn: TurnId,
    },
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Completed turn.
        turn: TurnId,
    },
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Cancelled turn.
        turn: TurnId,
    },
    /// Imported text whose value was explicitly source-attested.
    ImportedText {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Exact source-attested text.
        content: String,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ProcessImportedContentKind,
    },
}

/// One complete transcript and cursor observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSnapshot {
    session: SessionId,
    cursor: u64,
    turns: Vec<ProcessTranscriptTurn>,
    entries: Vec<ProcessTranscriptEntry>,
}

impl ProcessTranscriptSnapshot {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global last committed outbox sequence from this snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Borrows turns in immutable acceptance order.
    pub fn turns(&self) -> &[ProcessTranscriptTurn] {
        &self.turns
    }

    /// Borrows the latest semantic frontier in member order.
    pub fn entries(&self) -> &[ProcessTranscriptEntry] {
        &self.entries
    }
}

/// One bounded-memory item yielded from a repeatable-read transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptItem {
    /// One turn in acceptance order.
    Turn(ProcessTranscriptTurn),
    /// One semantic entry in frontier order.
    Entry(ProcessTranscriptEntry),
}

/// Counts and cursor observed after a transcript reader reaches its committed
/// end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSummary {
    session: SessionId,
    cursor: u64,
    turn_count: u64,
    entry_count: u64,
}

impl ProcessTranscriptSummary {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global outbox cursor from the repeatable-read snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the exact number of yielded turns.
    pub const fn turn_count(&self) -> u64 {
        self.turn_count
    }

    /// Returns the exact number of yielded semantic entries.
    pub const fn entry_count(&self) -> u64 {
        self.entry_count
    }
}

/// One repeatable-read transcript cursor that owns at most one decoded row.
///
/// Call [`Self::next_item`] until it returns `None`. That terminal call commits
/// the read-only transaction and makes [`Self::summary`] available. Dropping a
/// reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessTranscriptReader {
    transaction: Option<Transaction<'static, Postgres>>,
    session: SessionId,
    cursor: u64,
    lineage_tip: Option<TurnId>,
    latest_frontier: Option<ContextFrontierId>,
    expected_turn_count: u64,
    turn_count: u64,
    next_turn_after: Option<u64>,
    turns_complete: bool,
    entry_count: Option<u64>,
    next_entry_index: u64,
    summary: Option<ProcessTranscriptSummary>,
}

impl ProcessTranscriptReader {
    /// Returns the selected session while the reader is active.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the snapshot's global outbox cursor.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the committed summary only after [`Self::next_item`] returned
    /// `None`.
    pub const fn summary(&self) -> Option<ProcessTranscriptSummary> {
        self.summary
    }

    /// Yields one turn or entry without retaining prior decoded rows.
    pub async fn next_item(&mut self) -> Result<Option<ProcessTranscriptItem>, ProcessReadError> {
        if self.summary.is_some() {
            return Ok(None);
        }

        if !self.turns_complete {
            let session = self.session;
            let next_turn_after = self.next_turn_after;
            let row = load_next_transcript_turn(self.transaction_mut()?, session, next_turn_after)
                .await?;
            if let Some(row) = row {
                let decoded = decode_transcript_turn(&row)?;
                match (decoded.start_lineage, decoded.latest_frontier) {
                    (None, None) => {}
                    (Some(_), Some(frontier)) => {
                        if Some(decoded.turn.turn()) == self.lineage_tip
                            && self.latest_frontier.replace(frontier).is_some()
                        {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "turn execution lineage",
                            )
                            .into());
                        }
                    }
                    _ => {
                        return Err(ProcessReadCorruption::Inconsistent(
                            "started turn frontier shape",
                        )
                        .into());
                    }
                }
                self.next_turn_after = Some(decoded.turn.acceptance_position());
                self.turn_count =
                    self.turn_count
                        .checked_add(1)
                        .ok_or(ProcessReadCorruption::InvalidOrdinal(
                            "transcript turn count",
                        ))?;
                return Ok(Some(ProcessTranscriptItem::Turn(decoded.turn)));
            }
            if self.turn_count != self.expected_turn_count {
                return Err(ProcessReadCorruption::Inconsistent("turn acceptance ordering").into());
            }
            if self.lineage_tip.is_some() && self.latest_frontier.is_none() {
                return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
            }
            self.turns_complete = true;
            let session = self.session;
            let latest_frontier = self.latest_frontier;
            self.entry_count = Some(match latest_frontier {
                Some(frontier) => {
                    open_transcript_entry_cursor(self.transaction_mut()?, session, frontier).await?
                }
                None => 0,
            });
        }

        let entry_count = self
            .entry_count
            .ok_or(ProcessReadCorruption::Missing("transcript entry count"))?;
        if self.latest_frontier.is_some() {
            let entry_index = self.next_entry_index;
            if let Some(entry) =
                fetch_next_transcript_entry(self.transaction_mut()?, entry_index, entry_count)
                    .await?
            {
                if entry_index >= entry_count {
                    return Err(ProcessReadCorruption::Inconsistent(
                        "context frontier declared membership",
                    )
                    .into());
                }
                self.next_entry_index = self.next_entry_index.checked_add(1).ok_or(
                    ProcessReadCorruption::InvalidOrdinal("transcript entry index"),
                )?;
                return Ok(Some(ProcessTranscriptItem::Entry(entry)));
            }
        }
        if self.next_entry_index != entry_count {
            return Err(ProcessReadCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.summary = Some(ProcessTranscriptSummary {
            session: self.session,
            cursor: self.cursor,
            turn_count: self.turn_count,
            entry_count,
        });
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

/// A committed read shape that cannot form the closed process projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessReadCorruption {
    /// One required row or field was absent.
    Missing(&'static str),
    /// A closed storage discriminator had no admitted mapping.
    Unsupported {
        /// Storage field containing the discriminator.
        field: &'static str,
        /// Unsupported durable spelling.
        value: String,
    },
    /// Related durable fields disagreed.
    Inconsistent(&'static str),
    /// A stored ordinal was not an admitted unsigned integer.
    InvalidOrdinal(&'static str),
}

impl fmt::Display for ProcessReadCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "process read is missing {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "process read has unsupported {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "process read has inconsistent {relationship}")
            }
            Self::InvalidOrdinal(field) => {
                write!(formatter, "process read has invalid {field}")
            }
        }
    }
}

impl Error for ProcessReadCorruption {}

/// PostgreSQL failure or fail-closed projection corruption.
#[derive(Debug)]
pub enum ProcessReadError {
    /// PostgreSQL could not complete the repeatable-read transaction.
    Database(sqlx::Error),
    /// Committed rows could not form the closed projection.
    Corruption(ProcessReadCorruption),
}

impl fmt::Display for ProcessReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("process read database operation failed"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProcessReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
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
}

impl ProcessReadRepository {
    /// Uses the supplied pool for independent repeatable-read snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
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
                selected_defaults.version AS selected_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id,
                selected_defaults.dangerous_tool_auto_approval,
                selected_defaults.system_prompt
               FROM session AS session_row
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session_row.session_id
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
    /// This narrow read lets a process adapter reject a retained protocol
    /// version before mutating a session whose transcript that version cannot
    /// represent.
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

    /// Returns whether the selected session has a model-identity boundary.
    pub async fn session_has_model_identity_history(
        &self,
        requested_session: SessionId,
    ) -> Result<bool, ProcessReadError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM semantic_transcript_entry
                  WHERE source_session_id = $1
                    AND payload_kind = 'model_identity_changed'
             )",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Reads whether the session exists and, when it does, whether its active
    /// turn is parked on the model-call recovery wait.
    ///
    /// This narrow read lets a process adapter refuse a reconciliation request
    /// whose named turn owes no owner decision, before recording a durable
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
        let mut entries = Vec::new();
        while let Some(item) = reader.next_item().await? {
            match item {
                ProcessTranscriptItem::Turn(turn) => turns.push(turn),
                ProcessTranscriptItem::Entry(entry) => entries.push(entry),
            }
        }
        let summary = reader
            .summary()
            .ok_or(ProcessReadCorruption::Missing("process transcript summary"))?;
        Ok(Some(ProcessTranscriptSnapshot {
            session: summary.session(),
            cursor: summary.cursor(),
            turns,
            entries,
        }))
    }

    /// Opens one repeatable-read transcript cursor, or `None` only when the
    /// session is absent from that transaction snapshot.
    ///
    /// The cursor yields at most one decoded turn or entry at a time. This is
    /// the production boundary for spooling snapshots without transcript-sized
    /// process memory.
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
        let lineage_tip = load_execution_lineage_tip(&mut transaction, requested_session).await?;
        // INV-039 remains fail-closed on every transcript open: native lineage
        // supersedes the seed as the rendered frontier, not as an integrity fact.
        let imported_seed =
            load_checked_imported_seed_frontier(&mut transaction, requested_session).await?;
        let expected_turn_count =
            load_transcript_turn_count(&mut transaction, requested_session).await?;
        Ok(Some(ProcessTranscriptReader {
            transaction: Some(transaction),
            session: requested_session,
            cursor,
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
            entry_count: None,
            next_entry_index: 0,
            summary: None,
        }))
    }
}

fn decode_process_session_ancestry(
    row: &PgRow,
) -> Result<ProcessSessionAncestry, ProcessReadError> {
    let ancestry: String = required(row, "ancestry_kind")?;
    match ancestry.as_str() {
        "none" => Ok(ProcessSessionAncestry::OwnerInitiated),
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
        (ProcessSessionAncestry::OwnerInitiated, None) => Ok(None),
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

fn decode_session_summary(row: &PgRow) -> Result<ProcessSessionSummary, ProcessReadError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let defaults_version = decode_positive(
        required(row, "current_version")?,
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
    Ok(ProcessSessionSummary {
        session,
        defaults_version,
        model_selection,
    })
}

struct DecodedTurn {
    turn: ProcessTranscriptTurn,
    start_lineage: Option<DecodedStartLineage>,
    latest_frontier: Option<ContextFrontierId>,
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
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_lifecycle WHERE session_id = $1")
            .bind(session_id_to_uuid(session))
            .fetch_one(&mut **transaction)
            .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript turn count").into())
}

async fn load_next_transcript_turn(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<u64>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "SELECT
            turn.turn_id,
            turn.acceptance_position,
            turn.origin_accepted_input_id,
            turn.state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.current_attempt_id,
            turn.terminal_disposition_kind,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            terminal_call.terminal_disposition_kind
                AS terminal_model_call_disposition_kind,
            accepted.accepted_input_id,
            accepted.acceptance_position AS accepted_position,
            accepted.origin_turn_id,
            accepted.content_text AS accepted_content,
            current_call.model_call_id AS current_model_call_id,
            current_call.state_kind AS current_model_call_state_kind,
            current_call.context_frontier_id AS current_model_call_frontier_id,
            recovery_call.context_frontier_id AS recovery_model_call_frontier_id,
            active_tool_round.boundary_frontier_id AS active_tool_round_frontier_id
           FROM turn_lifecycle AS turn
           LEFT JOIN accepted_input AS accepted
             ON accepted.accepted_input_id = turn.origin_accepted_input_id
            AND accepted.session_id = turn.session_id
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
          WHERE turn.session_id = $1
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

fn decode_database_count(
    row: &PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u64, ProcessReadError> {
    let count: i64 = row.try_get(column)?;
    u64::try_from(count).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field).into())
}

fn decode_transcript_turn(row: &PgRow) -> Result<DecodedTurn, ProcessReadError> {
    let turn = TurnId::from_uuid(required(row, "turn_id")?);
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "turn acceptance position",
    )?;
    let origin_accepted_input =
        AcceptedInputId::from_uuid(required(row, "origin_accepted_input_id")?);
    let accepted_input = AcceptedInputId::from_uuid(required(row, "accepted_input_id")?);
    let accepted_position = decode_positive(
        required(row, "accepted_position")?,
        "accepted input position",
    )?;
    let accepted_origin = TurnId::from_uuid(required(row, "origin_turn_id")?);
    let accepted_content: String = required(row, "accepted_content")?;
    if origin_accepted_input != accepted_input
        || accepted_position != acceptance_position
        || accepted_origin != turn
        || accepted_content.is_empty()
    {
        return Err(ProcessReadCorruption::Inconsistent("turn accepted-input correlation").into());
    }
    let state_kind: String = required(row, "state_kind")?;
    let start_lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
    let immediate_predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
    let start_lineage = match (
        state_kind.as_str(),
        start_lineage_kind.as_deref(),
        immediate_predecessor,
    ) {
        ("queued", None, None) => None,
        ("active" | "terminal", Some("first_in_session"), None) => {
            Some(DecodedStartLineage::FirstInSession)
        }
        ("active" | "terminal", Some("after"), Some(predecessor)) => {
            Some(DecodedStartLineage::After(TurnId::from_uuid(predecessor)))
        }
        ("queued" | "active" | "terminal", Some(value), _)
            if !matches!(value, "first_in_session" | "after") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "turn start lineage kind",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn start lineage shape").into());
        }
    };
    let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
    let active_phase: Option<String> = row.try_get("active_phase_kind")?;
    let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
    let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
    let recovery_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
    let active_tool_round_call: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
    let approval_tool_request: Option<Uuid> = row.try_get("approval_tool_request_id")?;
    let recovery_tool_attempt: Option<Uuid> = row.try_get("recovery_tool_attempt_id")?;
    let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
    let terminal_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
    let terminal_tool_attempt: Option<Uuid> = row.try_get("terminal_tool_attempt_id")?;
    let terminal_call_disposition: Option<String> =
        row.try_get("terminal_model_call_disposition_kind")?;
    let current_model_call: Option<Uuid> = row.try_get("current_model_call_id")?;
    let current_model_call_state: Option<String> = row.try_get("current_model_call_state_kind")?;
    let current_model_call_frontier: Option<Uuid> =
        row.try_get("current_model_call_frontier_id")?;
    let recovery_model_call_frontier: Option<Uuid> =
        row.try_get("recovery_model_call_frontier_id")?;
    let active_tool_round_frontier: Option<Uuid> = row.try_get("active_tool_round_frontier_id")?;

    if !matches!(state_kind.as_str(), "queued" | "active" | "terminal") {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn state kind",
            value: state_kind,
        }
        .into());
    }
    if let Some(value) = active_phase.as_deref()
        && !matches!(
            value,
            "running"
                | "awaiting_model_call_recovery"
                | "awaiting_tool_approval"
                | "awaiting_tool_recovery"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn active phase",
            value: value.to_owned(),
        }
        .into());
    }
    if let Some(value) = terminal_disposition.as_deref()
        && !matches!(
            value,
            "failed" | "completed" | "refused" | "cancelled" | "reconciliation_required"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn terminal disposition",
            value: value.to_owned(),
        }
        .into());
    }
    let (current_model_call, current_model_call_frontier) = match (
        current_model_call,
        current_model_call_state.as_deref(),
        current_model_call_frontier,
    ) {
        (None, None, None) => (None, None),
        (Some(call), Some("prepared"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::Prepared,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("in_flight"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::InFlight,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("cancellation_requested"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::CancellationRequested,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(_), Some(value), _)
            if !matches!(value, "prepared" | "in_flight" | "cancellation_requested") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "current model call state",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("current model call shape").into());
        }
    };
    let recovery_model_call_frontier =
        recovery_model_call_frontier.map(ContextFrontierId::from_uuid);

    if matches!(active_phase.as_deref(), Some("awaiting_tool_approval")) {
        let (Some(starting_frontier), Some(_producing_call), Some(request), Some(tool_frontier)) = (
            starting_frontier,
            active_tool_round_call,
            approval_tool_request,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool approval frontier").into());
        }
        return Ok(DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                state: ProcessTurnState::ActiveAwaitingToolApproval {
                    request: ToolRequestId::from_uuid(request),
                },
            },
            start_lineage,
            latest_frontier: Some(latest_frontier),
        });
    }

    if matches!(active_phase.as_deref(), Some("awaiting_tool_recovery")) {
        let (
            Some(starting_frontier),
            Some(ended_attempt),
            Some(_producing_call),
            Some(recovery_attempt),
            Some(tool_frontier),
        ) = (
            starting_frontier,
            current_attempt,
            active_tool_round_call,
            recovery_tool_attempt,
            active_tool_round_frontier,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery frontier").into());
        }
        return Ok(DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                state: ProcessTurnState::ActiveAwaitingToolRecovery {
                    ended_attempt: TurnAttemptId::from_uuid(ended_attempt),
                    recovery_attempt: ToolAttemptId::from_uuid(recovery_attempt),
                },
            },
            start_lineage,
            latest_frontier: Some(latest_frontier),
        });
    }

    if matches!(active_phase.as_deref(), Some("running")) && active_tool_round_call.is_some() {
        let (Some(starting_frontier), Some(attempt), Some(tool_frontier)) = (
            starting_frontier,
            current_attempt,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("running tool frontier").into());
        }
        return Ok(DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                state: ProcessTurnState::ActiveRunning {
                    current_attempt: TurnAttemptId::from_uuid(attempt),
                    current_model_call: None,
                },
            },
            start_lineage,
            latest_frontier: Some(latest_frontier),
        });
    }

    if state_kind == "terminal"
        && terminal_disposition.as_deref() == Some("reconciliation_required")
        && terminal_call.is_none()
        && terminal_tool_attempt.is_some()
    {
        let (Some(frontier), Some(attempt), Some(tool_attempt)) =
            (terminal_frontier, terminal_attempt, terminal_tool_attempt)
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        };
        if active_phase.is_some()
            || current_attempt.is_some()
            || recovery_call.is_some()
            || active_tool_round_call.is_some()
            || approval_tool_request.is_some()
            || recovery_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
            || active_tool_round_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        }
        return Ok(DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                state: ProcessTurnState::ReconciliationRequired {
                    terminal_frontier: ContextFrontierId::from_uuid(frontier),
                    terminal_attempt: TurnAttemptId::from_uuid(attempt),
                    operation: ProcessReconciliationOperation::ToolAttempt(
                        ToolAttemptId::from_uuid(tool_attempt),
                    ),
                },
            },
            start_lineage,
            latest_frontier: Some(ContextFrontierId::from_uuid(frontier)),
        });
    }

    if active_tool_round_call.is_some()
        || approval_tool_request.is_some()
        || recovery_tool_attempt.is_some()
        || terminal_tool_attempt.is_some()
        || active_tool_round_frontier.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("tool lifecycle authority shape").into());
    }

    let (state, latest_frontier) = match (
        state_kind.as_str(),
        starting_frontier,
        terminal_frontier,
        active_phase.as_deref(),
        current_attempt,
        terminal_disposition.as_deref(),
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_call_disposition.as_deref(),
        current_model_call,
    ) {
        ("queued", None, None, None, None, None, None, None, None, None, None) => (
            ProcessTurnState::Queued {
                accepted_input,
                content: accepted_content,
            },
            None,
        ),
        (
            "active",
            Some(frontier),
            None,
            Some("running"),
            Some(attempt),
            None,
            None,
            None,
            None,
            None,
            current_model_call,
        ) => (
            ProcessTurnState::ActiveRunning {
                current_attempt: TurnAttemptId::from_uuid(attempt),
                current_model_call,
            },
            Some(
                current_model_call_frontier
                    .unwrap_or_else(|| ContextFrontierId::from_uuid(frontier)),
            ),
        ),
        (
            "active",
            Some(_),
            None,
            Some("awaiting_model_call_recovery"),
            Some(attempt),
            None,
            Some(call),
            None,
            None,
            None,
            None,
        ) => {
            let call_frontier = recovery_model_call_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("recovery model call frontier"),
            )?;
            (
                ProcessTurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt: TurnAttemptId::from_uuid(attempt),
                    recovery_call: ModelCallId::from_uuid(call),
                },
                Some(call_frontier),
            )
        }
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: None,
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            Some(call),
            Some(disposition @ ("known_failed" | "cancelled")),
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: Some(ProcessFailedTerminalModelCall {
                    call: ModelCallId::from_uuid(call),
                    disposition: match disposition {
                        "known_failed" => ProcessFailedModelCallDisposition::KnownFailed,
                        "cancelled" => ProcessFailedModelCallDisposition::Cancelled,
                        _ => {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "failed terminal model call disposition",
                            )
                            .into());
                        }
                    },
                }),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("completed"),
            None,
            Some(attempt),
            Some(call),
            Some("completed"),
            None,
        ) => (
            ProcessTurnState::Completed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("refused"),
            None,
            Some(attempt),
            Some(call),
            Some("refused"),
            None,
        ) => (
            ProcessTurnState::Refused {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            Some(call),
            Some("cancelled"),
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: Some(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("reconciliation_required"),
            None,
            Some(attempt),
            Some(call),
            Some("ambiguous"),
            None,
        ) => (
            ProcessTurnState::ReconciliationRequired {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                operation: ProcessReconciliationOperation::ModelCall(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn lifecycle state shape").into());
        }
    };

    Ok(DecodedTurn {
        turn: ProcessTranscriptTurn {
            turn,
            acceptance_position,
            state,
        },
        start_lineage,
        latest_frontier,
    })
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
            imported.source_speaker_kind AS imported_source_speaker_kind,
            imported.content_encoding AS imported_content_encoding,
            accepted.content_text AS origin_content,
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
            transcript_approval.denial_reason AS transcript_denial_reason
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
           LEFT JOIN imported_transcript_entry AS imported
             ON imported.imported_conversation_id =
                    entry.imported_conversation_id
            AND imported.imported_transcript_entry_id =
                    entry.imported_transcript_entry_id
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

fn decode_transcript_entry(
    row: &PgRow,
    entry_index: u64,
) -> Result<ProcessTranscriptEntry, ProcessReadError> {
    let source_session = session_id_from_uuid(required(row, "source_session_id")?);
    let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
    let payload_kind: String = required(row, "payload_kind")?;
    let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
    let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
    let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
    let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
    let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
    let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
    let tool_result_request: Option<Uuid> = row.try_get("tool_result_request_id")?;
    let tool_result_attempt: Option<Uuid> = row.try_get("tool_result_attempt_id")?;
    let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
    let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
    let imported_conversation: Option<Uuid> = row.try_get("imported_conversation_id")?;
    let imported_entry: Option<Uuid> = row.try_get("imported_transcript_entry_id")?;
    let model_identity_turn: Option<Uuid> = row.try_get("model_identity_turn_id")?;
    let model_identity_defaults_version: Option<Decimal> =
        row.try_get("model_identity_defaults_version")?;
    let model_identity_direct_selection: Option<Uuid> =
        row.try_get("model_identity_direct_selection_id")?;
    let imported_source_speaker: Option<String> = row.try_get("imported_source_speaker_kind")?;
    let imported_content: Option<Vec<u8>> = row.try_get("imported_content_encoding")?;
    let origin_content: Option<String> = row.try_get("origin_content")?;
    let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
    let assistant_turn: Option<Uuid> = row.try_get("assistant_turn_id")?;
    let result_attempt_request: Option<Uuid> = row.try_get("result_attempt_request_id")?;
    let transcript_tool_name: Option<String> = row.try_get("transcript_tool_name")?;
    let transcript_tool_arguments: Option<String> = row.try_get("transcript_tool_arguments")?;
    let result_disposition: Option<String> = row.try_get("result_disposition")?;
    let result_text: Option<String> = row.try_get("result_text")?;
    let result_error_kind: Option<String> = row.try_get("result_error_kind")?;
    let result_error_detail: Option<String> = row.try_get("result_error_detail")?;
    let transcript_decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let transcript_denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;

    if payload_kind == "model_identity_changed" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || imported_conversation.is_some()
            || imported_entry.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("model identity semantic entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(
                model_identity_turn.ok_or(ProcessReadCorruption::Missing("model identity turn"))?,
            ),
            defaults_version: decode_positive(
                model_identity_defaults_version.ok_or(ProcessReadCorruption::Missing(
                    "model identity defaults version",
                ))?,
                "model identity defaults version",
            )?,
            selected: DirectModelSelection::from_uuid(model_identity_direct_selection.ok_or(
                ProcessReadCorruption::Missing("model identity direct selection"),
            )?),
        });
    }
    if model_identity_turn.is_some()
        || model_identity_defaults_version.is_some()
        || model_identity_direct_selection.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("native semantic model identity fields").into(),
        );
    }

    if payload_kind == "assistant_tool_use" {
        let (Some(call), Some(request), Some(turn), Some(name), Some(arguments)) = (
            producing_call,
            tool_request,
            assistant_turn,
            transcript_tool_name,
            transcript_tool_arguments,
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            request: ToolRequestId::from_uuid(request),
            name,
            arguments,
        });
    }

    if payload_kind == "tool_execution_result" {
        let (Some(attempt), Some(request), Some(disposition)) = (
            tool_result_attempt,
            result_attempt_request,
            result_disposition.as_deref(),
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        };
        let content = match (
            disposition,
            result_text,
            result_error_kind,
            result_error_detail,
        ) {
            ("completed", Some(text), None, None) => text,
            ("known_failed", None, Some(kind), detail) => serde_json::json!({
                "error": {
                    "kind": kind,
                    "detail": detail,
                }
            })
            .to_string(),
            _ => {
                return Err(
                    ProcessReadCorruption::Inconsistent("tool execution-result evidence").into(),
                );
            }
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request: ToolRequestId::from_uuid(request),
            attempt: ToolAttemptId::from_uuid(attempt),
            content,
        });
    }

    if matches!(
        payload_kind.as_str(),
        "tool_denied" | "tool_closed_by_turn_end"
    ) {
        let Some(request) = tool_result_request else {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        }
        return Ok(if payload_kind == "tool_denied" {
            if transcript_decision_kind.as_deref() != Some("deny") {
                return Err(ProcessReadCorruption::Inconsistent("tool denial decision").into());
            }
            ProcessTranscriptEntry::ToolDenied {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: serde_json::json!({
                    "error": {
                        "kind": "denied",
                        "detail": transcript_denial_reason,
                    }
                })
                .to_string(),
            }
        } else {
            ProcessTranscriptEntry::ToolClosed {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: String::from(r#"{"error":{"detail":null,"kind":"closed_by_turn_end"}}"#),
            }
        });
    }

    if tool_result_request.is_some()
        || tool_result_attempt.is_some()
        || result_attempt_request.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("semantic transcript tool fields").into());
    }

    if payload_kind == "imported_entry" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("imported semantic entry shape").into(),
            );
        }
        let imported_conversation =
            ImportedConversationId::from_uuid(imported_conversation.ok_or(
                ProcessReadCorruption::Missing("imported conversation identity"),
            )?);
        let imported_entry = ImportedTranscriptEntryId::from_uuid(
            imported_entry.ok_or(ProcessReadCorruption::Missing("imported entry identity"))?,
        );
        let source_speaker = decode_imported_source_speaker(
            imported_source_speaker
                .ok_or(ProcessReadCorruption::Missing("imported source speaker"))?,
        )?;
        let content = decode_content(
            imported_content
                .as_deref()
                .ok_or(ProcessReadCorruption::Missing("imported content encoding"))?,
        )
        .map_err(|_| ProcessReadCorruption::Inconsistent("imported content encoding"))?;
        return Ok(project_imported_entry(
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        ));
    }

    if imported_conversation.is_some()
        || imported_entry.is_some()
        || imported_source_speaker.is_some()
        || imported_content.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("native semantic entry shape").into());
    }

    let projected = match (
        payload_kind.as_str(),
        origin,
        steering_source_turn,
        failed_turn,
        assistant_text,
        producing_call,
        tool_request,
        completed_turn,
        cancelled_turn,
        origin_content,
        origin_turn,
        assistant_turn,
    ) {
        (
            "origin_accepted_input",
            Some(accepted_input),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            Some(turn),
            None,
        ) if !content.is_empty() => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content,
        },
        (
            "steering_accepted_input",
            Some(accepted_input),
            Some(turn),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            None,
            None,
        ) if !content.is_empty() => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content,
        },
        (
            "assistant_text",
            None,
            None,
            None,
            Some(content),
            Some(call),
            None,
            None,
            None,
            None,
            None,
            Some(turn),
        ) if !content.is_empty() => ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            content,
        },
        ("turn_failed", None, None, Some(turn), None, None, None, None, None, None, None, None) => {
            ProcessTranscriptEntry::TurnFailed {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
            }
        }
        (
            "turn_completed",
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "turn_cancelled",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "origin_accepted_input"
            | "steering_accepted_input"
            | "assistant_text"
            | "assistant_tool_use"
            | "tool_execution_result"
            | "tool_denied"
            | "tool_closed_by_turn_end"
            | "turn_failed"
            | "turn_completed"
            | "turn_cancelled",
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
        ) => {
            return Err(
                ProcessReadCorruption::Inconsistent("semantic transcript entry shape").into(),
            );
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "semantic transcript payload kind",
                value: payload_kind,
            }
            .into());
        }
    };
    Ok(projected)
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

#[cfg(test)]
mod tests {
    use signalbox_domain::TurnId;
    use sqlx::types::Uuid;

    use super::decode_execution_lineage_tip;

    fn turn(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    /// S24 / INV-032: acceptance order A, B, C may execute as A, C, B; the
    /// database lineage diagnostic selects B as the one complete-chain tip.
    #[test]
    fn s24_inv032_latest_tip_follows_execution_lineage() {
        let second = turn(2);

        assert_eq!(
            decode_execution_lineage_tip(3, 1, 3, 1, false, false, Some(second))
                .expect("the lineage is one complete chain"),
            Some(second)
        );
    }

    /// INV-032: a branched persisted execution lineage cannot choose one
    /// authoritative snapshot frontier and therefore fails closed.
    #[test]
    fn inv032_latest_frontier_rejects_branched_execution_lineage() {
        assert!(decode_execution_lineage_tip(3, 1, 3, 2, true, false, Some(turn(2))).is_err());
    }
}
