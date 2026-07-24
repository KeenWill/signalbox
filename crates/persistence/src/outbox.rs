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
const TURN_CANCELLED: &str = "turn_cancelled";
const TURN_RECONCILIATION_REQUIRED: &str = "turn_reconciliation_required";
const STORAGE_VERSION: i16 = 1;

type OutboxSlotRow = (
    Decimal,
    Option<Decimal>,
    Option<String>,
    Option<i16>,
    Option<Uuid>,
);

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
    /// An interrupt-cancelled turn committed its semantic marker.
    TurnCancelled {
        /// Cancelled turn.
        turn: TurnId,
        /// Exact semantic cancellation marker.
        cancellation_entry: SemanticTranscriptEntryId,
        /// Exact terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
    /// A stopped turn terminalized for explicit reconciliation.
    TurnReconciliationRequired {
        /// Turn requiring reconciliation.
        turn: TurnId,
        /// Exact ambiguous model call.
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
    /// Exact issued call received durable cancellation intent.
    CancellationRequested,
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
    /// A committed header existed beyond the allocator cursor.
    EventBeyondAllocatedSequence,
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
    /// A lifecycle transition disagreed with authoritative durable state.
    InvalidLifecycleEventCorrelation,
    /// A terminal typed record disagreed with authoritative durable state.
    InvalidTerminalEventCorrelation,
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
            Self::EventBeyondAllocatedSequence => {
                "outbox event header exceeds the allocated sequence"
            }
            Self::MissingCommittedEventHeader => "outbox committed event header is missing",
            Self::InvalidSequence => "outbox sequence is invalid",
            Self::InvalidAcceptancePosition => "outbox input acceptance position is invalid",
            Self::UnsupportedStorageVersion => "outbox storage version is unsupported",
            Self::UnsupportedEventKind => "outbox event kind is unsupported",
            Self::MissingTypedRecord => "outbox typed event record is missing",
            Self::InvalidLifecycleEventCorrelation => {
                "outbox lifecycle event correlations are invalid"
            }
            Self::InvalidTerminalEventCorrelation => {
                "outbox terminal event correlations are invalid"
            }
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
            let allocated = load_allocated_sequence(&mut transaction).await?;
            if allocated < delivered {
                return Err(OutboxCorruption::DeliveryBeyondAllocatedSequence.into());
            }
            transaction.rollback().await?;
            return Ok(OutboxDispatchOutcome::Idle);
        };
        let (allocated, event) = load_event(&mut transaction, next).await?;
        if allocated < delivered {
            return Err(OutboxCorruption::DeliveryBeyondAllocatedSequence.into());
        }
        if event.is_some() && allocated < next {
            return Err(OutboxCorruption::EventBeyondAllocatedSequence.into());
        }
        let Some(event) = event else {
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

async fn load_allocated_sequence(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, OutboxDispatchError> {
    let allocated: Option<Decimal> =
        sqlx::query_scalar("SELECT last_sequence FROM outbox_sequence_state WHERE singleton")
            .fetch_optional(&mut **transaction)
            .await?;
    decode_nonnegative_sequence(allocated.ok_or(OutboxCorruption::MissingSequenceState)?)
        .map_err(Into::into)
}

async fn load_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expected_sequence: u64,
) -> Result<(u64, Option<DispatchedOutboxEvent>), OutboxDispatchError> {
    let row: Option<OutboxSlotRow> = sqlx::query_as(
        "SELECT
            allocator.last_sequence,
            event.event_sequence,
            event.event_kind,
            event.storage_version,
            event.session_id
           FROM outbox_sequence_state AS allocator
           LEFT JOIN outbox_event AS event
             ON event.event_sequence = $1
          WHERE allocator.singleton",
    )
    .bind(Decimal::from(expected_sequence))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((allocated, stored_sequence, event_kind, storage_version, stored_session)) = row
    else {
        return Err(OutboxCorruption::MissingSequenceState.into());
    };
    let allocated = decode_nonnegative_sequence(allocated)?;
    let (stored_sequence, event_kind, storage_version, stored_session) =
        match (stored_sequence, event_kind, storage_version, stored_session) {
            (None, None, None, None) => return Ok((allocated, None)),
            (Some(sequence), Some(kind), Some(version), Some(session)) => {
                (sequence, kind, version, session)
            }
            _ => return Err(OutboxCorruption::MissingCommittedEventHeader.into()),
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
                  WHERE event_sequence = $1
                    AND session_id = $2",
                expected_sequence,
                stored_session,
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
                   JOIN submit_input_command AS command
                     ON command.command_id = accepted.accepting_command_id
                    AND command.session_id = event.session_id
                    AND command.result_session_id = event.session_id
                    AND command.result_kind = 'applied'
                    AND command.result_accepted_input_id = event.accepted_input_id
                    AND command.content_kind = 'text'
                    AND command.content_text = accepted.content_text
                   JOIN queued_input_origin AS queued
                     ON queued.accepted_input_id = event.accepted_input_id
                    AND queued.turn_id = event.turn_id
                    AND queued.session_id = event.session_id
                    AND queued.acceptance_position = event.acceptance_position
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.origin_accepted_input_id = event.accepted_input_id
                    AND turn.acceptance_position = event.acceptance_position
                   LEFT JOIN turn_lifecycle AS source
                     ON source.turn_id = accepted.expected_active_turn_id
                    AND source.session_id = event.session_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2
                    AND (
                        (
                            accepted.disposition_kind = 'origin_of'
                            AND command.result_turn_id = event.turn_id
                        )
                        OR (
                            accepted.disposition_kind =
                                'reclassified_as_turn_origin'
                            AND command.result_turn_id IS NULL
                            AND accepted.expected_active_turn_id IS NOT NULL
                            AND source.state_kind = 'terminal'
                        )
                    )",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
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
            let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
                "SELECT event.turn_id, event.current_attempt_id,
                        turn.turn_id IS NOT NULL AS lifecycle_correlated
                   FROM turn_activated_outbox_event AS event
                   LEFT JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND (
                        (
                            turn.state_kind = 'active'
                            AND turn.current_attempt_id = event.current_attempt_id
                        )
                        OR (
                            turn.state_kind = 'terminal'
                            AND turn.terminal_attempt_id = event.current_attempt_id
                        )
                    )
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, current_attempt, lifecycle_correlated) =
                row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            if !lifecycle_correlated {
                return Err(OutboxCorruption::InvalidLifecycleEventCorrelation.into());
            }
            DispatchedOutboxEventKind::TurnActivated {
                turn: TurnId::from_uuid(turn),
                current_attempt: TurnAttemptId::from_uuid(current_attempt),
            }
        }
        TURN_FAILED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.failure_entry_id,
                        event.terminal_frontier_id
                   FROM turn_failed_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'failed'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                   JOIN semantic_transcript_entry AS failure
                     ON failure.source_session_id = event.session_id
                    AND failure.semantic_entry_id = event.failure_entry_id
                    AND failure.payload_kind = 'turn_failed'
                    AND failure.failed_turn_id = event.turn_id
                   JOIN context_frontier AS frontier
                     ON frontier.owning_session_id = event.session_id
                    AND frontier.context_frontier_id =
                        event.terminal_frontier_id
                   JOIN context_frontier_member AS terminal_member
                     ON terminal_member.owning_session_id =
                        frontier.owning_session_id
                    AND terminal_member.context_frontier_id =
                        frontier.context_frontier_id
                    AND terminal_member.member_position = frontier.member_count
                    AND terminal_member.source_session_id = event.session_id
                    AND terminal_member.semantic_entry_id =
                        event.failure_entry_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2
                    AND (
                        (
                            turn.terminal_attempt_id IS NULL
                            AND turn.terminal_model_call_id IS NULL
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM turn_attempt AS any_attempt
                                 WHERE any_attempt.turn_id = event.turn_id
                                   AND any_attempt.session_id = event.session_id
                            )
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM model_call AS any_call
                                 WHERE any_call.turn_id = event.turn_id
                                   AND any_call.session_id = event.session_id
                            )
                        )
                        OR (
                            turn.terminal_attempt_id IS NOT NULL
                            AND (
                                SELECT count(*)
                                  FROM turn_attempt AS counted_attempt
                                 WHERE counted_attempt.turn_id = event.turn_id
                                   AND counted_attempt.session_id =
                                       event.session_id
                            ) = 1
                            AND EXISTS (
                                SELECT 1
                                  FROM turn_attempt AS terminal_attempt
                                 WHERE terminal_attempt.turn_attempt_id =
                                       turn.terminal_attempt_id
                                   AND terminal_attempt.turn_id = event.turn_id
                                   AND terminal_attempt.session_id =
                                       event.session_id
                                   AND terminal_attempt.state_kind = 'ended'
                                   AND (
                                        (
                                            terminal_attempt.end_variant =
                                                'without_stop'
                                            AND terminal_attempt.end_disposition
                                                IN ('known_failure', 'lost')
                                        )
                                        OR (
                                            terminal_attempt.end_variant =
                                                'after_cancellation'
                                            AND terminal_attempt.end_disposition
                                                = 'known_failure'
                                            AND terminal_attempt.interrupt_command_id
                                                IS NOT NULL
                                            AND terminal_attempt.interrupt_predecessor_turn_id
                                                = event.turn_id
                                        )
                                   )
                            )
                            AND (
                                (
                                    turn.terminal_model_call_id IS NULL
                                    AND NOT EXISTS (
                                        SELECT 1
                                          FROM model_call AS any_call
                                         WHERE any_call.turn_id = event.turn_id
                                           AND any_call.session_id =
                                               event.session_id
                                    )
                                )
                                OR (
                                    turn.terminal_model_call_id IS NOT NULL
                                    AND (
                                        SELECT count(*)
                                          FROM model_call AS counted_call
                                         WHERE counted_call.turn_id =
                                               event.turn_id
                                           AND counted_call.session_id =
                                               event.session_id
                                    ) = 1
                                    AND EXISTS (
                                        SELECT 1
                                          FROM model_call AS terminal_call
                                          JOIN turn_attempt AS terminal_attempt
                                            ON terminal_attempt.turn_attempt_id =
                                               terminal_call.turn_attempt_id
                                           AND terminal_attempt.turn_id =
                                               terminal_call.turn_id
                                           AND terminal_attempt.session_id =
                                               terminal_call.session_id
                                         WHERE terminal_call.model_call_id =
                                               turn.terminal_model_call_id
                                           AND terminal_call.turn_attempt_id =
                                               turn.terminal_attempt_id
                                           AND terminal_call.turn_id =
                                               event.turn_id
                                           AND terminal_call.session_id =
                                               event.session_id
                                           AND terminal_call.state_kind =
                                               'terminal'
                                           AND (
                                                (
                                                    terminal_attempt.end_variant
                                                        = 'without_stop'
                                                    AND terminal_call.terminal_disposition_kind
                                                        IN (
                                                            'known_failed',
                                                            'cancelled'
                                                        )
                                                )
                                                OR (
                                                    terminal_attempt.end_variant
                                                        = 'after_cancellation'
                                                    AND terminal_call.terminal_disposition_kind
                                                        = 'known_failed'
                                                )
                                           )
                                    )
                                )
                            )
                        )
                    )",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, failure_entry, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnFailed {
                turn: TurnId::from_uuid(turn),
                failure_entry: SemanticTranscriptEntryId::from_uuid(failure_entry),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        MODEL_CALL_TRANSITION => {
            let row = sqlx::query(
                "SELECT event.turn_id, event.model_call_id,
                        event.call_state_kind,
                        event.terminal_disposition_kind,
                        call.state_kind AS authoritative_state_kind,
                        call.terminal_disposition_kind
                            AS authoritative_terminal_disposition_kind
                   FROM model_call_transition_outbox_event AS event
                   JOIN model_call AS call
                     ON call.model_call_id = event.model_call_id
                    AND call.turn_id = event.turn_id
                    AND call.session_id = event.session_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let state_kind: String = row.try_get("call_state_kind")?;
            let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
            let state = decode_model_call_state(&state_kind, terminal_disposition.as_deref())?;
            if matches!(state, DispatchedModelCallState::Terminal(_)) {
                let authoritative_state_kind: Option<String> =
                    row.try_get("authoritative_state_kind")?;
                let authoritative_terminal_disposition: Option<String> =
                    row.try_get("authoritative_terminal_disposition_kind")?;
                if authoritative_state_kind.as_deref() != Some("terminal")
                    || authoritative_terminal_disposition != terminal_disposition
                {
                    return Err(OutboxCorruption::InvalidTerminalEventCorrelation.into());
                }
            }
            DispatchedOutboxEventKind::ModelCallTransition {
                turn: TurnId::from_uuid(row.try_get("turn_id")?),
                call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
                state,
            }
        }
        TURN_COMPLETED => {
            let row: Option<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.model_call_id,
                        event.completion_entry_id, event.terminal_frontier_id
                   FROM turn_completed_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'completed'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                    AND turn.terminal_model_call_id = event.model_call_id
                   JOIN model_call AS call
                     ON call.model_call_id = event.model_call_id
                    AND call.turn_id = event.turn_id
                    AND call.session_id = event.session_id
                    AND call.turn_attempt_id = turn.terminal_attempt_id
                    AND call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'completed'
                   JOIN turn_attempt AS terminal_attempt
                     ON terminal_attempt.turn_attempt_id =
                        turn.terminal_attempt_id
                    AND terminal_attempt.turn_id = event.turn_id
                    AND terminal_attempt.session_id = event.session_id
                    AND terminal_attempt.state_kind = 'ended'
                    AND terminal_attempt.end_disposition
                        IN ('turn_completed', 'lost')
                   JOIN semantic_transcript_entry AS completion
                     ON completion.source_session_id = event.session_id
                    AND completion.semantic_entry_id = event.completion_entry_id
                    AND completion.payload_kind = 'turn_completed'
                    AND completion.completed_turn_id = event.turn_id
                   JOIN context_frontier AS frontier
                     ON frontier.owning_session_id = event.session_id
                    AND frontier.context_frontier_id =
                        event.terminal_frontier_id
                   JOIN context_frontier_member AS terminal_member
                     ON terminal_member.owning_session_id =
                        frontier.owning_session_id
                    AND terminal_member.context_frontier_id =
                        frontier.context_frontier_id
                    AND terminal_member.member_position = frontier.member_count
                    AND terminal_member.source_session_id = event.session_id
                    AND terminal_member.semantic_entry_id =
                        event.completion_entry_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, completion_entry, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnCompleted {
                turn: TurnId::from_uuid(turn),
                call: ModelCallId::from_uuid(call),
                completion_entry: SemanticTranscriptEntryId::from_uuid(completion_entry),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        TURN_REFUSED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.model_call_id,
                        event.terminal_frontier_id
                   FROM turn_refused_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'refused'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                    AND turn.terminal_model_call_id = event.model_call_id
                   JOIN model_call AS call
                     ON call.model_call_id = event.model_call_id
                    AND call.turn_id = event.turn_id
                    AND call.session_id = event.session_id
                    AND call.turn_attempt_id = turn.terminal_attempt_id
                    AND call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'refused'
                   JOIN turn_attempt AS terminal_attempt
                     ON terminal_attempt.turn_attempt_id =
                        turn.terminal_attempt_id
                    AND terminal_attempt.turn_id = event.turn_id
                    AND terminal_attempt.session_id = event.session_id
                    AND terminal_attempt.state_kind = 'ended'
                    AND terminal_attempt.end_disposition
                        IN ('turn_refused', 'lost')
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnRefused {
                turn: TurnId::from_uuid(turn),
                call: ModelCallId::from_uuid(call),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        TURN_CANCELLED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.cancellation_entry_id,
                        event.terminal_frontier_id
                   FROM turn_cancelled_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'cancelled'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                   JOIN semantic_transcript_entry AS cancellation
                     ON cancellation.source_session_id = event.session_id
                    AND cancellation.semantic_entry_id = event.cancellation_entry_id
                    AND cancellation.payload_kind = 'turn_cancelled'
                    AND cancellation.cancelled_turn_id = event.turn_id
                   JOIN context_frontier AS frontier
                     ON frontier.owning_session_id = event.session_id
                    AND frontier.context_frontier_id =
                        event.terminal_frontier_id
                   JOIN context_frontier_member AS terminal_member
                     ON terminal_member.owning_session_id =
                        frontier.owning_session_id
                    AND terminal_member.context_frontier_id =
                        frontier.context_frontier_id
                    AND terminal_member.member_position = frontier.member_count
                    AND terminal_member.source_session_id = event.session_id
                    AND terminal_member.semantic_entry_id =
                        event.cancellation_entry_id
                   JOIN turn_attempt AS terminal_attempt
                     ON terminal_attempt.turn_attempt_id =
                        turn.terminal_attempt_id
                    AND terminal_attempt.turn_id = event.turn_id
                    AND terminal_attempt.session_id = event.session_id
                    AND terminal_attempt.state_kind = 'ended'
                    AND terminal_attempt.end_variant = 'after_cancellation'
                    AND terminal_attempt.end_disposition = 'cancelled'
                    AND terminal_attempt.interrupt_command_id IS NOT NULL
                    AND terminal_attempt.interrupt_predecessor_turn_id =
                        event.turn_id
                   LEFT JOIN model_call AS terminal_call
                     ON terminal_call.model_call_id =
                        turn.terminal_model_call_id
                    AND terminal_call.turn_attempt_id =
                        turn.terminal_attempt_id
                    AND terminal_call.turn_id = event.turn_id
                    AND terminal_call.session_id = event.session_id
                    AND terminal_call.state_kind = 'terminal'
                    AND terminal_call.terminal_disposition_kind = 'cancelled'
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2
                    AND (
                        (
                            turn.terminal_model_call_id IS NULL
                            AND terminal_call.model_call_id IS NULL
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM model_call AS any_call
                                 WHERE any_call.turn_id = event.turn_id
                                   AND any_call.session_id = event.session_id
                            )
                        )
                        OR (
                            turn.terminal_model_call_id IS NOT NULL
                            AND terminal_call.model_call_id =
                                turn.terminal_model_call_id
                            AND (
                                SELECT count(*)
                                  FROM model_call AS counted_call
                                 WHERE counted_call.turn_id = event.turn_id
                                   AND counted_call.session_id =
                                       event.session_id
                            ) = 1
                        )
                    )",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, cancellation_entry, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnCancelled {
                turn: TurnId::from_uuid(turn),
                cancellation_entry: SemanticTranscriptEntryId::from_uuid(cancellation_entry),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        TURN_RECONCILIATION_REQUIRED => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.model_call_id,
                        event.terminal_frontier_id
                   FROM turn_reconciliation_required_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'reconciliation_required'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                    AND turn.terminal_model_call_id = event.model_call_id
                   JOIN model_call AS call
                     ON call.model_call_id = event.model_call_id
                    AND call.turn_id = event.turn_id
                    AND call.session_id = event.session_id
                    AND call.turn_attempt_id = turn.terminal_attempt_id
                    AND call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'ambiguous'
                   JOIN turn_attempt AS terminal_attempt
                     ON terminal_attempt.turn_attempt_id =
                        turn.terminal_attempt_id
                    AND terminal_attempt.turn_id = event.turn_id
                    AND terminal_attempt.session_id = event.session_id
                    AND terminal_attempt.state_kind = 'ended'
                    AND terminal_attempt.end_disposition
                        IN ('ambiguous', 'lost')
                    AND (
                        (
                            terminal_attempt.end_variant =
                                'after_cancellation'
                            AND terminal_attempt.interrupt_command_id
                                IS NOT NULL
                            AND terminal_attempt.interrupt_predecessor_turn_id =
                                event.turn_id
                        )
                        OR (
                            terminal_attempt.end_variant = 'without_stop'
                            AND terminal_attempt.interrupt_command_id IS NULL
                            AND terminal_attempt.interrupt_predecessor_turn_id
                                IS NULL
                        )
                    )
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn: TurnId::from_uuid(turn),
                call: ModelCallId::from_uuid(call),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        _ => return Err(OutboxCorruption::UnsupportedEventKind.into()),
    };

    Ok((
        allocated,
        Some(DispatchedOutboxEvent {
            sequence: expected_sequence,
            session,
            kind,
        }),
    ))
}

async fn require_typed_record(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    statement: &'static str,
    sequence: u64,
    session: Uuid,
) -> Result<(), OutboxDispatchError> {
    let found: Option<Decimal> = sqlx::query_scalar(statement)
        .bind(Decimal::from(sequence))
        .bind(session)
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
        ("cancellation_requested", None) => Ok(DispatchedModelCallState::CancellationRequested),
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
    TurnCancelled {
        session: SessionId,
        turn: TurnId,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    },
    TurnReconciliationRequired {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
}

pub(crate) enum ModelCallOutboxState {
    Prepared,
    InFlight,
    CancellationRequested,
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
        OutboxEvent::TurnCancelled {
            session,
            turn,
            cancellation_entry,
            terminal_frontier,
        } => {
            append_turn_cancelled(
                connection,
                session,
                turn,
                cancellation_entry,
                terminal_frontier,
            )
            .await
        }
        OutboxEvent::TurnReconciliationRequired {
            session,
            turn,
            call,
            terminal_frontier,
        } => {
            append_turn_reconciliation_required(connection, session, turn, call, terminal_frontier)
                .await
        }
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
        ModelCallOutboxState::CancellationRequested => ("cancellation_requested", None),
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

async fn append_turn_cancelled(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_cancelled_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, cancellation_entry_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_CANCELLED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(cancellation_entry.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_turn_reconciliation_required(
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
         INSERT INTO turn_reconciliation_required_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, model_call_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_RECONCILIATION_REQUIRED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(call.into_uuid())
    .bind(terminal_frontier.into_uuid())
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
