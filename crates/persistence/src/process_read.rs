//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Reads use one read-only repeatable-read transaction so
//! the hub can map a complete, stable projection explicitly.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DirectModelSelection, ModelAlias, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId, TurnId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::mapping::{session_id_from_uuid, session_id_to_uuid};

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
        /// Ambiguous terminal call.
        terminal_call: ModelCallId,
    },
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

/// One ordered member of the latest authoritative semantic frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptEntry {
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
            if self.lineage_tip.is_some() != self.latest_frontier.is_some() {
                return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
            }
            self.turns_complete = true;
            let session = self.session;
            let latest_frontier = self.latest_frontier;
            self.entry_count = Some(
                load_transcript_entry_count(self.transaction_mut()?, session, latest_frontier)
                    .await?,
            );
        }

        let entry_count = self
            .entry_count
            .ok_or(ProcessReadCorruption::Missing("transcript entry count"))?;
        if self.next_entry_index < entry_count {
            let entry_index = self.next_entry_index;
            let session = self.session;
            let frontier = self.latest_frontier.ok_or(ProcessReadCorruption::Missing(
                "context frontier for transcript entries",
            ))?;
            let entry =
                load_transcript_entry(self.transaction_mut()?, session, frontier, entry_index)
                    .await?;
            self.next_entry_index = self.next_entry_index.checked_add(1).ok_or(
                ProcessReadCorruption::InvalidOrdinal("transcript entry index"),
            )?;
            return Ok(Some(ProcessTranscriptItem::Entry(entry)));
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
        let expected_turn_count =
            load_transcript_turn_count(&mut transaction, requested_session).await?;
        Ok(Some(ProcessTranscriptReader {
            transaction: Some(transaction),
            session: requested_session,
            cursor,
            lineage_tip,
            latest_frontier: None,
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
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            terminal_call.terminal_disposition_kind
                AS terminal_model_call_disposition_kind,
            accepted.accepted_input_id,
            accepted.acceptance_position AS accepted_position,
            accepted.origin_turn_id,
            accepted.content_text AS accepted_content,
            current_call.model_call_id AS current_model_call_id,
            current_call.state_kind AS current_model_call_state_kind,
            current_call.context_frontier_id AS current_model_call_frontier_id,
            recovery_call.context_frontier_id AS recovery_model_call_frontier_id
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
    let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
    let terminal_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
    let terminal_call_disposition: Option<String> =
        row.try_get("terminal_model_call_disposition_kind")?;
    let current_model_call: Option<Uuid> = row.try_get("current_model_call_id")?;
    let current_model_call_state: Option<String> = row.try_get("current_model_call_state_kind")?;
    let current_model_call_frontier: Option<Uuid> =
        row.try_get("current_model_call_frontier_id")?;
    let recovery_model_call_frontier: Option<Uuid> =
        row.try_get("recovery_model_call_frontier_id")?;

    if !matches!(state_kind.as_str(), "queued" | "active" | "terminal") {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn state kind",
            value: state_kind,
        }
        .into());
    }
    if let Some(value) = active_phase.as_deref()
        && !matches!(value, "running" | "awaiting_model_call_recovery")
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
                        _ => unreachable!("the pattern closes failed call dispositions"),
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
                terminal_call: ModelCallId::from_uuid(call),
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

async fn load_transcript_entry_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    frontier: Option<ContextFrontierId>,
) -> Result<u64, ProcessReadError> {
    let Some(frontier) = frontier else {
        return Ok(0);
    };
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
    let actual_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    let actual_count = u64::try_from(actual_count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry count"))?;
    if actual_count != member_count {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier declared membership").into(),
        );
    }
    Ok(member_count)
}

async fn load_transcript_entry(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    frontier: ContextFrontierId,
    entry_index: u64,
) -> Result<ProcessTranscriptEntry, ProcessReadError> {
    let member_position =
        entry_index
            .checked_add(1)
            .ok_or(ProcessReadCorruption::InvalidOrdinal(
                "frontier member position",
            ))?;
    let row = sqlx::query(
        "SELECT
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
            entry.completed_turn_id,
            entry.cancelled_turn_id,
            accepted.content_text AS origin_content,
            accepted.origin_turn_id,
            call.turn_id AS assistant_turn_id
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN accepted_input AS accepted
             ON accepted.session_id = entry.source_session_id
            AND accepted.accepted_input_id = entry.origin_accepted_input_id
           LEFT JOIN model_call AS call
             ON call.session_id = entry.source_session_id
            AND call.model_call_id = entry.producing_model_call_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2
            AND member.member_position = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .bind(Decimal::from(member_position))
    .fetch_optional(&mut **transaction)
    .await?;
    let row = row.ok_or(ProcessReadCorruption::Missing("context frontier member"))?;
    let stored_position = decode_positive(
        required(&row, "member_position")?,
        "frontier member position",
    )?;
    if stored_position != member_position {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier contiguous membership").into(),
        );
    }
    decode_transcript_entry(&row, entry_index)
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
    let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
    let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
    let origin_content: Option<String> = row.try_get("origin_content")?;
    let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
    let assistant_turn: Option<Uuid> = row.try_get("assistant_turn_id")?;

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
        ("assistant_tool_use", _, _, _, _, _, _, _, _, _, _, _) => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "semantic transcript payload kind",
                value: payload_kind,
            }
            .into());
        }
        (
            "origin_accepted_input"
            | "steering_accepted_input"
            | "assistant_text"
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
