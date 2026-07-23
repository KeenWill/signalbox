//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Both public reads use one read-only repeatable-read
//! transaction so the hub can map a complete, stable projection explicitly.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DirectModelSelection, ModelAlias, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId, TurnId,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

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

/// Durable state of the current model call attached to an active turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessCurrentModelCallState {
    /// Provider work has not been authorized.
    Prepared,
    /// Provider work was authorized and may have happened.
    InFlight,
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

    /// Reads every current session summary in session-identity order.
    pub async fn list_sessions(&self) -> Result<Vec<ProcessSessionSummary>, ProcessReadError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let rows = sqlx::query(
            "SELECT
                session.session_id,
                current_defaults.current_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id
               FROM session
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session.session_id
               LEFT JOIN session_defaults_version AS selected_defaults
                 ON selected_defaults.session_id = current_defaults.session_id
                AND selected_defaults.version = current_defaults.current_version
              ORDER BY session.session_id",
        )
        .fetch_all(&mut *transaction)
        .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            summaries.push(decode_session_summary(&row)?);
        }
        transaction.commit().await?;
        Ok(summaries)
    }

    /// Reads one complete transcript snapshot, or `None` only when the session
    /// is absent from the shared transaction snapshot.
    pub async fn read_transcript(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<ProcessTranscriptSnapshot>, ProcessReadError> {
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

        let turn_rows = sqlx::query(
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
                accepted.accepted_input_id,
                accepted.acceptance_position AS accepted_position,
                accepted.origin_turn_id,
                accepted.content_text AS accepted_content,
                current_call.model_call_id AS current_model_call_id,
                current_call.state_kind AS current_model_call_state_kind
               FROM turn_lifecycle AS turn
               LEFT JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = turn.origin_accepted_input_id
                AND accepted.session_id = turn.session_id
               LEFT JOIN model_call AS current_call
                 ON current_call.turn_attempt_id = turn.current_attempt_id
                AND current_call.turn_id = turn.turn_id
                AND current_call.session_id = turn.session_id
                AND current_call.state_kind <> 'terminal'
              WHERE turn.session_id = $1
              ORDER BY turn.acceptance_position",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_all(&mut *transaction)
        .await?;

        let mut turns = Vec::with_capacity(turn_rows.len());
        let mut started_turns = BTreeMap::new();
        for row in turn_rows {
            let decoded = decode_transcript_turn(&row)?;
            match (decoded.start_lineage, decoded.latest_frontier) {
                (None, None) => {}
                (Some(lineage), Some(frontier)) => {
                    if started_turns
                        .insert(decoded.turn.turn(), (lineage, frontier))
                        .is_some()
                    {
                        return Err(
                            ProcessReadCorruption::Inconsistent("duplicate started turn").into(),
                        );
                    }
                }
                _ => {
                    return Err(
                        ProcessReadCorruption::Inconsistent("started turn frontier shape").into(),
                    );
                }
            }
            turns.push(decoded.turn);
        }
        let latest_frontier = latest_execution_frontier(&started_turns)?;
        let entries =
            load_transcript_entries(&mut transaction, requested_session, latest_frontier).await?;

        transaction.commit().await?;
        Ok(Some(ProcessTranscriptSnapshot {
            session: requested_session,
            cursor,
            turns,
            entries,
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

fn latest_execution_frontier(
    started_turns: &BTreeMap<TurnId, (DecodedStartLineage, ContextFrontierId)>,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    if started_turns.is_empty() {
        return Ok(None);
    }

    let mut root_count = 0_usize;
    let mut successor_by_predecessor = BTreeMap::new();
    for (turn, (lineage, _)) in started_turns {
        match lineage {
            DecodedStartLineage::FirstInSession => root_count += 1,
            DecodedStartLineage::After(predecessor) => {
                if !started_turns.contains_key(predecessor)
                    || successor_by_predecessor
                        .insert(*predecessor, *turn)
                        .is_some()
                {
                    return Err(
                        ProcessReadCorruption::Inconsistent("turn execution lineage").into(),
                    );
                }
            }
        }
    }
    if root_count != 1 {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }

    let mut tips = started_turns
        .keys()
        .filter(|turn| !successor_by_predecessor.contains_key(turn));
    let tip = *tips.next().ok_or(ProcessReadCorruption::Inconsistent(
        "turn execution lineage",
    ))?;
    if tips.next().is_some() {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }

    let mut visited = BTreeSet::new();
    let mut current = tip;
    loop {
        if !visited.insert(current) {
            return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
        }
        match started_turns
            .get(&current)
            .ok_or(ProcessReadCorruption::Inconsistent(
                "turn execution lineage",
            ))?
            .0
        {
            DecodedStartLineage::FirstInSession => break,
            DecodedStartLineage::After(predecessor) => current = predecessor,
        }
    }
    if visited.len() != started_turns.len() {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }

    Ok(Some(
        started_turns
            .get(&tip)
            .ok_or(ProcessReadCorruption::Inconsistent(
                "turn execution lineage",
            ))?
            .1,
    ))
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
    let current_model_call: Option<Uuid> = row.try_get("current_model_call_id")?;
    let current_model_call_state: Option<String> = row.try_get("current_model_call_state_kind")?;

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
        && !matches!(value, "failed" | "completed" | "refused")
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn terminal disposition",
            value: value.to_owned(),
        }
        .into());
    }
    let current_model_call = match (current_model_call, current_model_call_state.as_deref()) {
        (None, None) => None,
        (Some(call), Some("prepared")) => Some(ProcessCurrentModelCall {
            call: ModelCallId::from_uuid(call),
            state: ProcessCurrentModelCallState::Prepared,
        }),
        (Some(call), Some("in_flight")) => Some(ProcessCurrentModelCall {
            call: ModelCallId::from_uuid(call),
            state: ProcessCurrentModelCallState::InFlight,
        }),
        (Some(_), Some(value)) => {
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
        current_model_call,
    ) {
        ("queued", None, None, None, None, None, None, None, None, None) => (
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
            current_model_call,
        ) => (
            ProcessTurnState::ActiveRunning {
                current_attempt: TurnAttemptId::from_uuid(attempt),
                current_model_call,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "active",
            Some(frontier),
            None,
            Some("awaiting_model_call_recovery"),
            Some(attempt),
            None,
            Some(call),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt: TurnAttemptId::from_uuid(attempt),
                recovery_call: ModelCallId::from_uuid(call),
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
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
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
            Some(_),
            _,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
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
            None,
        ) => (
            ProcessTurnState::Refused {
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

async fn load_transcript_entries(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    frontier: Option<ContextFrontierId>,
) -> Result<Vec<ProcessTranscriptEntry>, ProcessReadError> {
    let Some(frontier) = frontier else {
        return Ok(Vec::new());
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

    let rows = sqlx::query(
        "SELECT
            member.member_position,
            member.source_session_id,
            member.semantic_entry_id,
            entry.payload_kind,
            entry.origin_accepted_input_id,
            entry.failed_turn_id,
            entry.assistant_text_value,
            entry.producing_model_call_id,
            entry.assistant_tool_request_id,
            entry.completed_turn_id,
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
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    let actual_count = u64::try_from(rows.len())
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry count"))?;
    if actual_count != member_count {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier declared membership").into(),
        );
    }

    let mut entries = Vec::with_capacity(rows.len());
    for (zero_based_index, row) in rows.into_iter().enumerate() {
        let expected_index = u64::try_from(zero_based_index)
            .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry index"))?;
        let stored_position = decode_positive(
            required(&row, "member_position")?,
            "frontier member position",
        )?;
        if stored_position != expected_index + 1 {
            return Err(ProcessReadCorruption::Inconsistent(
                "context frontier contiguous membership",
            )
            .into());
        }
        entries.push(decode_transcript_entry(&row, expected_index)?);
    }
    Ok(entries)
}

fn decode_transcript_entry(
    row: &PgRow,
    entry_index: u64,
) -> Result<ProcessTranscriptEntry, ProcessReadError> {
    let source_session = session_id_from_uuid(required(row, "source_session_id")?);
    let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
    let payload_kind: String = required(row, "payload_kind")?;
    let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
    let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
    let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
    let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
    let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
    let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
    let origin_content: Option<String> = row.try_get("origin_content")?;
    let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
    let assistant_turn: Option<Uuid> = row.try_get("assistant_turn_id")?;

    let projected = match (
        payload_kind.as_str(),
        origin,
        failed_turn,
        assistant_text,
        producing_call,
        tool_request,
        completed_turn,
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
            "assistant_text",
            None,
            None,
            Some(content),
            Some(call),
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
        ("turn_failed", None, Some(turn), None, None, None, None, None, None, None) => {
            ProcessTranscriptEntry::TurnFailed {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
            }
        }
        ("turn_completed", None, None, None, None, None, Some(turn), None, None, None) => {
            ProcessTranscriptEntry::TurnCompleted {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
            }
        }
        ("assistant_tool_use", _, _, _, _, _, _, _, _, _) => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "semantic transcript payload kind",
                value: payload_kind,
            }
            .into());
        }
        (
            "origin_accepted_input" | "assistant_text" | "turn_failed" | "turn_completed",
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
    use std::collections::BTreeMap;

    use signalbox_domain::{ContextFrontierId, TurnId};
    use sqlx::types::Uuid;

    use super::{DecodedStartLineage, latest_execution_frontier};

    fn turn(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    fn frontier(value: u128) -> ContextFrontierId {
        ContextFrontierId::from_uuid(Uuid::from_u128(value))
    }

    /// S24 / INV-032: acceptance order A, B, C may execute as A, C, B; the
    /// process snapshot selects B's frontier from persisted start lineage.
    #[test]
    fn s24_inv032_latest_frontier_follows_execution_lineage() {
        let first = turn(1);
        let second = turn(2);
        let interrupt = turn(3);
        let started = BTreeMap::from([
            (first, (DecodedStartLineage::FirstInSession, frontier(11))),
            (
                second,
                (DecodedStartLineage::After(interrupt), frontier(12)),
            ),
            (interrupt, (DecodedStartLineage::After(first), frontier(13))),
        ]);

        assert_eq!(
            latest_execution_frontier(&started).expect("the lineage is one complete chain"),
            Some(frontier(12))
        );
    }

    /// INV-032: a branched persisted execution lineage cannot choose one
    /// authoritative snapshot frontier and therefore fails closed.
    #[test]
    fn inv032_latest_frontier_rejects_branched_execution_lineage() {
        let first = turn(1);
        let started = BTreeMap::from([
            (first, (DecodedStartLineage::FirstInSession, frontier(11))),
            (turn(2), (DecodedStartLineage::After(first), frontier(12))),
            (turn(3), (DecodedStartLineage::After(first), frontier(13))),
        ]);

        assert!(latest_execution_frontier(&started).is_err());
    }
}
