//! Typed transactional-outbox append and dispatch boundaries
//! (docs/spec/persistence-protocol.md).
//!
//! Append functions accept the state-changing caller's existing PostgreSQL
//! connection and never own its transaction. [`OutboxDispatcher`] separately
//! owns the delivery-prefix transaction around one synchronous consumer offer.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ModelCallDisposition, ModelCallId,
    SemanticTranscriptEntryId, SessionId, SessionInputPosition, TurnAttemptId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    lock_inventory,
    mapping::{
        accepted_input_id_to_uuid, input_position_from_numeric, input_position_to_numeric,
        session_id_from_uuid, session_id_to_uuid, turn_id_to_uuid,
    },
};

const SESSION_CREATED: &str = "session_created";
const INPUT_ACCEPTED: &str = "input_accepted";
const TURN_ACTIVATED: &str = "turn_activated";
const TURN_FAILED: &str = "turn_failed";
const MODEL_CALL_TRANSITION: &str = "model_call_transition";
const TURN_COMPLETED: &str = "turn_completed";
const TURN_REFUSED: &str = "turn_refused";
const STORAGE_VERSION: i16 = 1;

/// One committed outbox event offered to the hub's single dispatcher consumer.
///
/// This is a persistence projection, not a domain event or process-protocol
/// frame. Its sequence is the durable global outbox cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchedOutboxEvent {
    sequence: u64,
    session: SessionId,
    kind: DispatchedOutboxEventKind,
}

impl DispatchedOutboxEvent {
    /// Returns the committed global outbox sequence.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the session named by the outbox header.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the decoded typed event record.
    pub const fn kind(&self) -> &DispatchedOutboxEventKind {
        &self.kind
    }
}

/// Closed typed records currently admitted by outbox storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchedOutboxEventKind {
    /// A session creation committed.
    SessionCreated,
    /// An accepted input and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input: AcceptedInputId,
        /// Queued turn created for the input.
        turn: TurnId,
        /// Immutable per-session acceptance position.
        acceptance_position: SessionInputPosition,
        /// Exact accepted text.
        content: String,
    },
    /// A queued turn atomically became active.
    TurnActivated {
        /// Activated turn.
        turn: TurnId,
        /// Initial current attempt.
        current_attempt: TurnAttemptId,
    },
    /// A turn closed as failed with its semantic marker and terminal frontier.
    TurnFailed {
        /// Failed turn.
        turn: TurnId,
        /// Semantic failure marker.
        failure_entry: SemanticTranscriptEntryId,
        /// Exact terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
    /// A model call advanced through one durable lifecycle checkpoint.
    ModelCallTransition {
        /// Owning turn.
        turn: TurnId,
        /// Advancing model call.
        call: ModelCallId,
        /// Exact committed call state.
        state: DispatchedModelCallState,
    },
    /// A turn committed authoritative assistant content and completed.
    TurnCompleted {
        /// Completed turn.
        turn: TurnId,
        /// Outcome-authoritative model call.
        call: ModelCallId,
        /// Final semantic completion marker.
        completion_entry: SemanticTranscriptEntryId,
        /// Exact terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
    /// A turn closed as refused without assistant content.
    TurnRefused {
        /// Refused turn.
        turn: TurnId,
        /// Outcome-authoritative model call.
        call: ModelCallId,
        /// Exact terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
}

/// Durable model-call state carried by one dispatched transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedModelCallState {
    /// Exact call entered Prepared.
    Prepared,
    /// Exact call entered InFlight.
    InFlight,
    /// Exact call reached a terminal disposition.
    Terminal(DispatchedModelCallDisposition),
}

/// Persistence-owned terminal disposition carried by a dispatched call record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedModelCallDisposition {
    /// The call committed authoritative completion.
    Completed,
    /// The call ended in a known failure.
    KnownFailed,
    /// The provider authoritatively refused.
    Refused,
    /// The call was durably cancelled.
    Cancelled,
    /// The physical outcome remained ambiguous.
    Ambiguous,
}

/// Whether the synchronous consumer accepted one offered event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxDeliveryDecision {
    /// The consumer accepted the event; the durable prefix may advance.
    Delivered,
    /// The consumer did not accept the event; the transaction rolls back.
    Retry,
}

/// Result of one bounded dispatcher attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxDispatchOutcome {
    /// No event exists immediately after the delivered prefix.
    Idle,
    /// The consumer requested another offer of the same sequence.
    Retry {
        /// Sequence whose durable prefix remains unadvanced.
        sequence: u64,
    },
    /// The consumer accepted the event and its cursor commit succeeded.
    Delivered {
        /// Sequence now included in the durable delivered prefix.
        sequence: u64,
    },
}

/// Fail-closed reason a committed outbox projection could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxCorruption {
    /// The singleton delivery row was absent.
    MissingDeliveryState,
    /// The locked singleton could not be advanced from the observed cursor.
    DeliveryStateChanged,
    /// The singleton allocation row was absent.
    MissingSequenceState,
    /// The delivered cursor exceeded the allocator cursor.
    DeliveryBeyondAllocatedSequence,
    /// The allocator named a committed sequence whose header was absent.
    MissingCommittedEventHeader,
    /// A stored cursor or sequence was not an unsigned 64-bit integer.
    InvalidSequence,
    /// An input-accepted record carried an invalid positive position.
    InvalidAcceptancePosition,
    /// An event header used an unsupported storage version.
    UnsupportedStorageVersion,
    /// An event header named no admitted typed record family.
    UnsupportedEventKind,
    /// The header's required typed record was absent.
    MissingTypedRecord,
    /// A model-call transition had an inconsistent or unknown state shape.
    InvalidModelCallState,
}

impl fmt::Display for OutboxCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingDeliveryState => "outbox delivery state is missing",
            Self::DeliveryStateChanged => "outbox delivery state changed unexpectedly",
            Self::MissingSequenceState => "outbox sequence state is missing",
            Self::DeliveryBeyondAllocatedSequence => {
                "outbox delivery state exceeds the allocated sequence"
            }
            Self::MissingCommittedEventHeader => "outbox committed event header is missing",
            Self::InvalidSequence => "outbox sequence is invalid",
            Self::InvalidAcceptancePosition => "outbox input acceptance position is invalid",
            Self::UnsupportedStorageVersion => "outbox storage version is unsupported",
            Self::UnsupportedEventKind => "outbox event kind is unsupported",
            Self::MissingTypedRecord => "outbox typed event record is missing",
            Self::InvalidModelCallState => "outbox model-call state is invalid",
        })
    }
}

impl Error for OutboxCorruption {}

/// Infrastructure or integrity failure from one dispatcher attempt.
#[derive(Debug)]
pub enum OutboxDispatchError {
    /// PostgreSQL acquisition, query, rollback, or commit failed.
    Database(sqlx::Error),
    /// Committed storage could not be decoded into the closed projection.
    Corruption(OutboxCorruption),
}

impl fmt::Display for OutboxDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("outbox dispatch database operation failed"),
            Self::Corruption(error) => write!(formatter, "outbox dispatch corruption: {error}"),
        }
    }
}

impl Error for OutboxDispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for OutboxDispatchError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<OutboxCorruption> for OutboxDispatchError {
    fn from(error: OutboxCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL-backed single-event transactional outbox dispatcher.
///
/// Composition runs exactly one attempt loop. The database lock still
/// serializes accidental concurrent callers, so none can skip or pass another.
#[derive(Clone, Debug)]
pub struct OutboxDispatcher {
    pool: PgPool,
}

impl OutboxDispatcher {
    /// Binds the dispatcher to the shared hub pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Offers exactly the next committed event and advances its cursor only
    /// after the synchronous consumer accepts it.
    ///
    /// The consumer runs while the delivery-state row lock is held. Returning
    /// [`OutboxDeliveryDecision::Retry`] or ending before the commit request
    /// leaves the prefix unchanged, so a later attempt offers the same event.
    /// A lost commit response is resolved by the next locked cursor read: a
    /// committed advance proceeds, while a rolled-back advance redelivers.
    pub async fn dispatch_next<Consumer>(
        &self,
        consume: Consumer,
    ) -> Result<OutboxDispatchOutcome, OutboxDispatchError>
    where
        Consumer: FnOnce(&DispatchedOutboxEvent) -> OutboxDeliveryDecision,
    {
        let mut transaction = self.pool.begin().await?;
        let delivered: Option<Decimal> = sqlx::query_scalar(lock_inventory::OUTBOX_DELIVERY)
            .fetch_optional(&mut *transaction)
            .await?;
        let delivered = delivered.ok_or(OutboxCorruption::MissingDeliveryState)?;
        let delivered = decode_nonnegative_sequence(delivered)?;
        let Some(next) = delivered.checked_add(1) else {
            transaction.rollback().await?;
            return Ok(OutboxDispatchOutcome::Idle);
        };
        let Some(event) = load_event(&mut transaction, next).await? else {
            let allocated: Option<Decimal> = sqlx::query_scalar(
                "SELECT last_sequence
                   FROM outbox_sequence_state
                  WHERE singleton",
            )
            .fetch_optional(&mut *transaction)
            .await?;
            let allocated = allocated.ok_or(OutboxCorruption::MissingSequenceState)?;
            let allocated = decode_nonnegative_sequence(allocated)?;
            if allocated < delivered {
                return Err(OutboxCorruption::DeliveryBeyondAllocatedSequence.into());
            }
            if allocated >= next {
                return Err(OutboxCorruption::MissingCommittedEventHeader.into());
            }
            transaction.rollback().await?;
            return Ok(OutboxDispatchOutcome::Idle);
        };

        if consume(&event) == OutboxDeliveryDecision::Retry {
            transaction.rollback().await?;
            return Ok(OutboxDispatchOutcome::Retry { sequence: next });
        }

        let updated = sqlx::query(
            "UPDATE outbox_delivery_state
                SET delivered_through = $1
              WHERE singleton
                AND delivered_through = $2",
        )
        .bind(Decimal::from(next))
        .bind(Decimal::from(delivered))
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(OutboxCorruption::DeliveryStateChanged.into());
        }
        transaction.commit().await?;
        Ok(OutboxDispatchOutcome::Delivered { sequence: next })
    }
}

async fn load_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expected_sequence: u64,
) -> Result<Option<DispatchedOutboxEvent>, OutboxDispatchError> {
    let header: Option<(Decimal, String, i16, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, event_kind, storage_version, session_id
           FROM outbox_event
          WHERE event_sequence = $1",
    )
    .bind(Decimal::from(expected_sequence))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((stored_sequence, event_kind, storage_version, stored_session)) = header else {
        return Ok(None);
    };
    if decode_positive_sequence(stored_sequence)? != expected_sequence {
        return Err(OutboxCorruption::InvalidSequence.into());
    }
    if storage_version != STORAGE_VERSION {
        return Err(OutboxCorruption::UnsupportedStorageVersion.into());
    }
    let session = session_id_from_uuid(stored_session);
    let kind = match event_kind.as_str() {
        SESSION_CREATED => {
            require_typed_record(
                transaction,
                "SELECT event_sequence
                   FROM session_created_outbox_event
                  WHERE event_sequence = $1",
                expected_sequence,
            )
            .await?;
            DispatchedOutboxEventKind::SessionCreated
        }
        INPUT_ACCEPTED => {
            let row = sqlx::query(
                "SELECT event.accepted_input_id, event.turn_id,
                        event.acceptance_position, accepted.content_text
                   FROM input_accepted_outbox_event AS event
                   JOIN accepted_input AS accepted
                     ON accepted.accepted_input_id = event.accepted_input_id
                    AND accepted.session_id = event.session_id
                    AND accepted.acceptance_position = event.acceptance_position
                    AND accepted.origin_turn_id = event.turn_id
                  WHERE event.event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let acceptance_position: Decimal = row.try_get("acceptance_position")?;
            let acceptance_position = input_position_from_numeric(acceptance_position)
                .map_err(|_| OutboxCorruption::InvalidAcceptancePosition)?;
            DispatchedOutboxEventKind::InputAccepted {
                accepted_input: AcceptedInputId::from_uuid(row.try_get("accepted_input_id")?),
                turn: TurnId::from_uuid(row.try_get("turn_id")?),
                acceptance_position,
                content: row.try_get("content_text")?,
            }
        }
        TURN_ACTIVATED => {
            let row: Option<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT turn_id, current_attempt_id
                   FROM turn_activated_outbox_event
                  WHERE event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, current_attempt) = row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            DispatchedOutboxEventKind::TurnActivated {
                turn: TurnId::from_uuid(turn),
                current_attempt: TurnAttemptId::from_uuid(current_attempt),
            }
        }
        TURN_FAILED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT turn_id, failure_entry_id, terminal_frontier_id
                   FROM turn_failed_outbox_event
                  WHERE event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, failure_entry, terminal_frontier) =
                row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            DispatchedOutboxEventKind::TurnFailed {
                turn: TurnId::from_uuid(turn),
                failure_entry: SemanticTranscriptEntryId::from_uuid(failure_entry),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        MODEL_CALL_TRANSITION => {
            let row = sqlx::query(
                "SELECT turn_id, model_call_id, call_state_kind,
                        terminal_disposition_kind
                   FROM model_call_transition_outbox_event
                  WHERE event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let state_kind: String = row.try_get("call_state_kind")?;
            let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
            DispatchedOutboxEventKind::ModelCallTransition {
                turn: TurnId::from_uuid(row.try_get("turn_id")?),
                call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
                state: decode_model_call_state(&state_kind, terminal_disposition.as_deref())?,
            }
        }
        TURN_COMPLETED => {
            let row: Option<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT turn_id, model_call_id, completion_entry_id,
                        terminal_frontier_id
                   FROM turn_completed_outbox_event
                  WHERE event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, completion_entry, terminal_frontier) =
                row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            DispatchedOutboxEventKind::TurnCompleted {
                turn: TurnId::from_uuid(turn),
                call: ModelCallId::from_uuid(call),
                completion_entry: SemanticTranscriptEntryId::from_uuid(completion_entry),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        TURN_REFUSED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT turn_id, model_call_id, terminal_frontier_id
                   FROM turn_refused_outbox_event
                  WHERE event_sequence = $1",
            )
            .bind(Decimal::from(expected_sequence))
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, terminal_frontier) =
                row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            DispatchedOutboxEventKind::TurnRefused {
                turn: TurnId::from_uuid(turn),
                call: ModelCallId::from_uuid(call),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        _ => return Err(OutboxCorruption::UnsupportedEventKind.into()),
    };

    Ok(Some(DispatchedOutboxEvent {
        sequence: expected_sequence,
        session,
        kind,
    }))
}

async fn require_typed_record(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    statement: &'static str,
    sequence: u64,
) -> Result<(), OutboxDispatchError> {
    let found: Option<Decimal> = sqlx::query_scalar(statement)
        .bind(Decimal::from(sequence))
        .fetch_optional(&mut **transaction)
        .await?;
    if found.is_none() {
        Err(OutboxCorruption::MissingTypedRecord.into())
    } else {
        Ok(())
    }
}

fn decode_nonnegative_sequence(value: Decimal) -> Result<u64, OutboxCorruption> {
    if !value.fract().is_zero() || value.is_sign_negative() {
        return Err(OutboxCorruption::InvalidSequence);
    }
    u64::try_from(value).map_err(|_| OutboxCorruption::InvalidSequence)
}

fn decode_positive_sequence(value: Decimal) -> Result<u64, OutboxCorruption> {
    let sequence = decode_nonnegative_sequence(value)?;
    if sequence == 0 {
        Err(OutboxCorruption::InvalidSequence)
    } else {
        Ok(sequence)
    }
}

fn decode_model_call_state(
    state_kind: &str,
    terminal_disposition: Option<&str>,
) -> Result<DispatchedModelCallState, OutboxCorruption> {
    match (state_kind, terminal_disposition) {
        ("prepared", None) => Ok(DispatchedModelCallState::Prepared),
        ("in_flight", None) => Ok(DispatchedModelCallState::InFlight),
        ("terminal", Some("completed")) => Ok(DispatchedModelCallState::Terminal(
            DispatchedModelCallDisposition::Completed,
        )),
        ("terminal", Some("known_failed")) => Ok(DispatchedModelCallState::Terminal(
            DispatchedModelCallDisposition::KnownFailed,
        )),
        ("terminal", Some("refused")) => Ok(DispatchedModelCallState::Terminal(
            DispatchedModelCallDisposition::Refused,
        )),
        ("terminal", Some("cancelled")) => Ok(DispatchedModelCallState::Terminal(
            DispatchedModelCallDisposition::Cancelled,
        )),
        ("terminal", Some("ambiguous")) => Ok(DispatchedModelCallState::Terminal(
            DispatchedModelCallDisposition::Ambiguous,
        )),
        _ => Err(OutboxCorruption::InvalidModelCallState),
    }
}

pub(crate) enum OutboxEvent {
    SessionCreated {
        session: SessionId,
    },
    InputAccepted {
        session: SessionId,
        accepted_input: AcceptedInputId,
        turn: TurnId,
        acceptance_position: SessionInputPosition,
    },
    TurnActivated {
        session: SessionId,
        turn: TurnId,
        current_attempt: TurnAttemptId,
    },
    TurnFailed {
        session: SessionId,
        turn: TurnId,
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    ModelCallTransition {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        state: ModelCallOutboxState,
    },
    TurnCompleted {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    TurnRefused {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
}

pub(crate) enum ModelCallOutboxState {
    Prepared,
    InFlight,
    Terminal(ModelCallDisposition),
}

pub(crate) async fn append(
    connection: &mut PgConnection,
    event: OutboxEvent,
) -> Result<(), sqlx::Error> {
    match event {
        OutboxEvent::SessionCreated { session } => {
            append_session_created(connection, session).await
        }
        OutboxEvent::InputAccepted {
            session,
            accepted_input,
            turn,
            acceptance_position,
        } => {
            append_input_accepted(
                connection,
                session,
                accepted_input,
                turn,
                acceptance_position,
            )
            .await
        }
        OutboxEvent::TurnActivated {
            session,
            turn,
            current_attempt,
        } => append_turn_activated(connection, session, turn, current_attempt).await,
        OutboxEvent::TurnFailed {
            session,
            turn,
            failure_entry,
            terminal_frontier,
        } => append_turn_failed(connection, session, turn, failure_entry, terminal_frontier).await,
        OutboxEvent::ModelCallTransition {
            session,
            turn,
            call,
            state,
        } => append_model_call_transition(connection, session, turn, call, state).await,
        OutboxEvent::TurnCompleted {
            session,
            turn,
            call,
            completion_entry,
            terminal_frontier,
        } => {
            append_turn_completed(
                connection,
                session,
                turn,
                call,
                completion_entry,
                terminal_frontier,
            )
            .await
        }
        OutboxEvent::TurnRefused {
            session,
            turn,
            call,
            terminal_frontier,
        } => append_turn_refused(connection, session, turn, call, terminal_frontier).await,
    }
}

async fn append_session_created(
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

async fn append_input_accepted(
    connection: &mut PgConnection,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    acceptance_position: SessionInputPosition,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO input_accepted_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             accepted_input_id, turn_id, acceptance_position)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(INPUT_ACCEPTED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(accepted_input_id_to_uuid(accepted_input))
    .bind(turn_id_to_uuid(turn))
    .bind(input_position_to_numeric(acceptance_position))
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_activated(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    current_attempt: TurnAttemptId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_activated_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, current_attempt_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5
           FROM header",
    )
    .bind(TURN_ACTIVATED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(current_attempt.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_failed(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_failed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, failure_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_FAILED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(failure_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;

    Ok(())
}

async fn append_model_call_transition(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    state: ModelCallOutboxState,
) -> Result<(), sqlx::Error> {
    let (state_kind, terminal_disposition) = match state {
        ModelCallOutboxState::Prepared => ("prepared", None),
        ModelCallOutboxState::InFlight => ("in_flight", None),
        ModelCallOutboxState::Terminal(disposition) => {
            ("terminal", Some(encode_model_call_disposition(disposition)))
        }
    };
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO model_call_transition_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             model_call_id, turn_id, call_state_kind, terminal_disposition_kind)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7
           FROM header",
    )
    .bind(MODEL_CALL_TRANSITION)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(state_kind)
    .bind(terminal_disposition)
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_completed(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    completion_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_completed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, completion_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7
           FROM header",
    )
    .bind(TURN_COMPLETED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(completion_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_refused(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_refused_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_REFUSED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

fn encode_model_call_disposition(disposition: ModelCallDisposition) -> &'static str {
    match disposition {
        ModelCallDisposition::Completed => "completed",
        ModelCallDisposition::KnownFailed => "known_failed",
        ModelCallDisposition::Refused => "refused",
        ModelCallDisposition::Cancelled => "cancelled",
        ModelCallDisposition::Ambiguous => "ambiguous",
    }
}
