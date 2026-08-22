//! Typed transactional-outbox append and dispatch boundaries
//! (docs/spec/persistence-protocol.md).
//!
//! Append functions accept the state-changing caller's existing PostgreSQL
//! connection and never own its transaction. [`OutboxDispatcher`] separately
//! owns the delivery-prefix transaction around one synchronous consumer offer.

use std::{error::Error, fmt};

#[cfg(feature = "postgres-integration")]
use std::num::NonZeroU64;

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputId, BoundChildAction, ContextCompactionId, ContextFrontierId, DelegationMessageId,
    DelegationOutcomeKind, DelegationOutcomeReason, DelegationWaitMode, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, FrozenAliasDefinition, FrozenModelSelection,
    ModelAlias, ModelCallDisposition, ModelCallId, ModelSelectionRequest, RunnerEnrollmentId,
    RunnerGeneration, RunnerId, RunnerSandboxProfile, RunnerWorkingDirectory,
    SemanticTranscriptEntryId, SessionId, SessionInputPosition, SessionModelSettingsChanged,
    ToolApprovalResolution, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
    TurnModelSettingsResolved,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    lock_inventory,
    mapping::{
        CONTEXT_COMPACTED, DelegationPolicyStorageKind, DelegationUpdateStorageKind,
        DelegationWakeStorageKind, GOAL_TURN_RETIRED, INPUT_ACCEPTED, MODEL_CALL_TRANSITION,
        OutboxEventDiscriminator, RUNNER_STATE_TRANSITION, SESSION_CREATED,
        SESSION_MODEL_SETTINGS_CHANGED, TOOL_APPROVAL_DECIDED, TOOL_BATCH_TRANSITION,
        TURN_ACTIVATED, TURN_CANCELLED, TURN_COMPLETED, TURN_FAILED, TURN_MODEL_SETTINGS_RESOLVED,
        TURN_RECONCILIATION_REQUIRED, TURN_REFUSED, accepted_input_id_to_uuid,
        bound_child_action_from_str, defaults_version_from_numeric, defaults_version_to_numeric,
        delegation_outcome_kind_from_str, delegation_outcome_reason_from_str,
        delegation_policy_kind_from_str, delegation_update_kind_from_str,
        delegation_wait_mode_from_str, delegation_wake_subject_from_str,
        dispatched_runner_state_from_str, dispatched_runner_state_to_str,
        durable_command_id_from_uuid, input_position_from_numeric, input_position_to_numeric,
        model_change_adjustments_from_json, model_settings_from_json,
        model_settings_overlay_from_json, outbox_event_discriminator_from_str,
        runner_sandbox_from_str, runner_sandbox_to_str, session_id_from_uuid, session_id_to_uuid,
        turn_id_to_uuid,
    },
};

#[cfg(feature = "postgres-integration")]
use crate::runner_protocol::RunnerConnectionEpoch;

const STORAGE_VERSION: i16 = 1;

type OutboxSlotRow = (
    Decimal,
    bool,
    Option<Decimal>,
    Option<String>,
    Option<i16>,
    Option<Uuid>,
);

type ToolBatchTransitionRow = (Uuid, Uuid, String, Option<Uuid>, Option<Uuid>, bool);

#[derive(sqlx::FromRow)]
struct ToolApprovalDecidedRow {
    turn_id: Uuid,
    request_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct TurnCancelledOutboxRow {
    turn_id: Uuid,
    cancellation_entry_id: Uuid,
    terminal_frontier_id: Uuid,
}

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
    /// A defaults replacement changed the model or model settings.
    SessionModelSettingsChanged(SessionModelSettingsChanged),
    /// An accepted origin froze complete model settings.
    TurnModelSettingsResolved(TurnModelSettingsResolved),
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
    /// A queued goal turn became intentionally ineligible.
    GoalTurnRetired {
        /// Exact immutable queued turn retired by a goal transition.
        turn: TurnId,
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
    /// A tool batch crossed one durable presentation boundary.
    ToolBatchTransition {
        /// Owning turn.
        turn: TurnId,
        /// Model call that proposed the batch.
        producing_call: ModelCallId,
        /// Exact durable batch state.
        state: DispatchedToolBatchState,
    },
    /// One tool approval decision committed with complete provenance.
    ToolApprovalDecided {
        /// Owning turn.
        turn: TurnId,
        /// Exact durable approval resolution.
        approval: ToolApprovalResolution,
        /// Exact explicit actor provenance.
        decider: signalbox_domain::ToolApprovalDecider,
    },
    /// One append-only context compaction committed.
    ContextCompacted {
        /// Exact compaction provenance record.
        compaction: ContextCompactionId,
        /// Dedicated producing model call.
        call: ModelCallId,
        /// One-based final summarized position.
        through_position: u64,
        /// Appended semantic summary entry.
        summary_entry: SemanticTranscriptEntryId,
        /// Complete result frontier.
        result_frontier: ContextFrontierId,
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
        /// Exact ambiguous operation.
        operation: DispatchedReconciliationOperation,
        /// Exact terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
    /// A session-visible runner placement or connection state changed.
    RunnerStateTransition {
        /// Exact runner named by the transition.
        runner: RunnerId,
        /// Positive placement revision whose immutable facts are projected.
        placement_revision: RunnerGeneration,
        /// Placement-selected sandbox profile.
        sandbox: RunnerSandboxProfile,
        /// Caller-selected directory, absent when the runner default was selected.
        working_directory: Option<RunnerWorkingDirectory>,
        /// Closed state projected to followers.
        state: DispatchedRunnerState,
    },
    /// One typed relationship update committed for a parent or message recipient.
    DelegationUpdate(DispatchedDelegationUpdate),
    /// One committed result or message can wake its exact recipient.
    DelegationWake(DispatchedDelegationWake),
}

/// Closed relationship updates admitted by delegation outbox storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationUpdate {
    /// A parent committed one child relationship and its spawn policy.
    ChildSpawned {
        /// Exact spawning tool request and relationship identity.
        spawning_request: ToolRequestId,
        /// Spawned child session.
        child: SessionId,
        /// Parent-chosen relationship lifecycle policy.
        policy: DispatchedDelegationPolicy,
    },
    /// A parent registered one foreground or background wait.
    ChildWaiting {
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Child being awaited.
        child: SessionId,
        /// Exact await tool request.
        awaiting_request: ToolRequestId,
        /// Wait delivery mode.
        mode: DispatchedDelegationWaitMode,
    },
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition {
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Evaluated child.
        child: SessionId,
        /// Relationship-local event ordinal.
        event_ordinal: u64,
        /// Typed relationship outcome.
        outcome: DispatchedDelegationOutcome,
        /// Typed reason for evaluating this relationship edge.
        reason: DispatchedDelegationReason,
        /// Exact parent command provenance.
        provenance: DispatchedDelegationProvenance,
    },
    /// A terminal child result became durable for its parent.
    ChildResult {
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Terminal child.
        child: SessionId,
        /// Typed terminal result outcome.
        outcome: DispatchedDelegationOutcome,
        /// Typed reason for the terminal result.
        reason: DispatchedDelegationReason,
        /// Exact child-turn or parent-command provenance.
        provenance: DispatchedDelegationProvenance,
        /// Delivered content for a successful result only.
        content: Option<String>,
    },
    /// One bidirectional relationship message became durable for its recipient.
    SessionMessage {
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Message identity.
        message: DelegationMessageId,
        /// Sending session.
        sender: SessionId,
        /// Receiving session.
        recipient: SessionId,
        /// Relationship-local message ordinal.
        message_ordinal: u64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: u64,
        /// Exact delivered content.
        content: String,
    },
}

/// Parent-chosen relationship policy carried by a spawn update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationPolicy {
    /// The child outlives parent state changes.
    Background,
    /// The child follows the two explicit parent-state actions.
    Bound {
        /// Action when the parent stops.
        on_parent_stopped: DispatchedBoundChildAction,
        /// Action when the parent is cancelled.
        on_parent_cancelled: DispatchedBoundChildAction,
    },
}

/// Closed action applied to one bound child relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedBoundChildAction {
    /// Leave the child running.
    KeepRunning,
    /// Stop the child through typed parent-policy evidence.
    Stop,
    /// Cancel the child through typed parent-policy evidence.
    Cancel,
}

/// Delivery behavior chosen by one await request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationWaitMode {
    /// Keep the current parent turn open.
    Foreground,
    /// Return registration immediately and deliver through a later wake.
    Background,
}

/// Closed relationship outcome carried by update dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationOutcome {
    /// Child content is available.
    ResultReturned,
    /// Child execution failed or returned unusable content.
    ChildFailed,
    /// Parent policy stopped the child.
    ChildStopped,
    /// Child or parent policy cancelled the child.
    ChildCancelled,
    /// Relationship policy left the child running.
    ContinueRunning,
    /// Parent policy reached an already-terminal child.
    AlreadyTerminal,
}

/// Exact reason carried alongside a relationship outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationReason {
    /// Child completed with delivered content.
    ChildCompleted,
    /// Child execution failed.
    ChildExecutionFailed,
    /// Completed child content could not form a result.
    ChildResultUnavailable,
    /// Child cancelled independently.
    ChildCancelled,
    /// A parent stop selected descendants.
    ParentStoppedWithDescendants,
    /// A parent cancellation selected descendants.
    ParentCancelledWithDescendants,
}

/// Proof source retained by one lifecycle or result update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationProvenance {
    /// Exact terminal child turn.
    ChildTurn {
        /// Child session.
        session: SessionId,
        /// Terminal delegated turn.
        turn: TurnId,
    },
    /// Exact parent turn command.
    ParentTurnCommand {
        /// Parent session.
        session: SessionId,
        /// Parent turn named by the command.
        turn: TurnId,
        /// Durable stop or interrupt command.
        command: DurableCommandId,
    },
    /// Exact parent goal-generation command.
    ParentGoalCommand {
        /// Parent session.
        session: SessionId,
        /// One-based goal generation.
        goal_generation: u64,
        /// Durable goal stop command.
        command: DurableCommandId,
    },
}

/// Committed content that can wake an exact delegation recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedDelegationWake {
    /// A child result is available; a late foreground wait may be named.
    Result {
        /// Relationship/result identity.
        spawning_request: ToolRequestId,
        /// Optional late foreground await request.
        awaiting_request: Option<ToolRequestId>,
    },
    /// One message is available to the event's recipient session.
    Message {
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Message identity.
        message: DelegationMessageId,
    },
}

/// Durable tool-batch boundary carried by one dispatched transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedToolBatchState {
    /// The proposal frontier committed.
    Proposed {
        /// Exact yielded frontier containing assistant tool-use entries.
        frontier: ContextFrontierId,
    },
    /// Proposal-ordered results committed for continuation.
    ResultsProjected {
        /// Exact result frontier.
        frontier: ContextFrontierId,
    },
    /// One exact attempt requires a user recovery decision.
    RecoveryRequired {
        /// Ambiguous tool attempt.
        attempt: ToolAttemptId,
    },
}

/// Closed runner state carried by one dispatched session transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedRunnerState {
    /// Initial dispatch pinned the selected runner.
    Pinned,
    /// The runner's current connection missed its first heartbeat.
    Suspect,
    /// A heartbeat acknowledgement recovered that same suspect connection.
    Connected,
    /// An exact runner selection was lost before initial pinning.
    RunnerLostBeforePin,
    /// A pinned runner became unavailable.
    RunnerLost,
    /// A checked successor runner replaced the prior placement.
    Replaced,
    /// Checked recovery retained the runner but changed the selected directory.
    WorkingDirectoryChanged,
    /// The user abandoned a lost runner placement.
    Abandoned,
}

/// Exact operation that made a turn require reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchedReconciliationOperation {
    /// Ambiguous provider call.
    ModelCall(ModelCallId),
    /// Ambiguous tool attempt.
    ToolAttempt(ToolAttemptId),
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
    /// A delegation update or wake had an inconsistent or unknown typed shape.
    InvalidDelegationEvent,
    /// A settings event disagreed with its immutable referenced records.
    InvalidModelSettingsEvent,
    /// A runner event had an inconsistent or unknown typed shape.
    InvalidRunnerEvent,
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
            Self::InvalidDelegationEvent => "outbox delegation event is invalid",
            Self::InvalidModelSettingsEvent => "outbox model-settings event is invalid",
            Self::InvalidRunnerEvent => "outbox runner event is invalid",
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
        let (allocated, event_beyond_allocated, event) = load_event(&mut transaction, next).await?;
        if allocated < delivered {
            return Err(OutboxCorruption::DeliveryBeyondAllocatedSequence.into());
        }
        if event_beyond_allocated {
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
) -> Result<(u64, bool, Option<DispatchedOutboxEvent>), OutboxDispatchError> {
    let row: Option<OutboxSlotRow> = sqlx::query_as(
        "SELECT
            allocator.last_sequence,
            EXISTS (
                SELECT 1
                  FROM outbox_event AS unallocated
                 WHERE unallocated.event_sequence > allocator.last_sequence
                UNION ALL
                SELECT 1
                  FROM delegation_outbox_event AS unallocated
                 WHERE unallocated.event_sequence > allocator.last_sequence
            ),
            event.event_sequence,
            event.event_kind,
            event.storage_version,
            event.session_id
           FROM outbox_sequence_state AS allocator
           LEFT JOIN (
                SELECT event_sequence, event_kind, storage_version, session_id
                  FROM outbox_event
                UNION ALL
                SELECT event_sequence, event_kind, storage_version, session_id
                  FROM delegation_outbox_event
           ) AS event
             ON event.event_sequence = $1
          WHERE allocator.singleton",
    )
    .bind(Decimal::from(expected_sequence))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((
        allocated,
        event_beyond_allocated,
        stored_sequence,
        event_kind,
        storage_version,
        stored_session,
    )) = row
    else {
        return Err(OutboxCorruption::MissingSequenceState.into());
    };
    let allocated = decode_nonnegative_sequence(allocated)?;
    let (stored_sequence, event_kind, storage_version, stored_session) =
        match (stored_sequence, event_kind, storage_version, stored_session) {
            (None, None, None, None) => return Ok((allocated, event_beyond_allocated, None)),
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
    let discriminator = outbox_event_discriminator_from_str(&event_kind)
        .ok_or(OutboxCorruption::UnsupportedEventKind)?;
    let kind = match discriminator {
        OutboxEventDiscriminator::SessionCreated => {
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
        OutboxEventDiscriminator::SessionModelSettingsChanged => {
            let row = sqlx::query(
                "SELECT changed.command_id, changed.prior_defaults_version,
                        changed.installed_defaults_version,
                        prior.model_selection_kind AS prior_model_kind,
                        prior.direct_model_selection_id AS prior_direct_id,
                        prior.model_alias_id AS prior_alias_id,
                        installed.model_selection_kind AS installed_model_kind,
                        installed.direct_model_selection_id AS installed_direct_id,
                        installed.model_alias_id AS installed_alias_id,
                        changed.prior_model_settings,
                        changed.installed_model_settings,
                        prior.model_settings AS prior_defaults_model_settings,
                        installed.model_settings AS installed_defaults_model_settings,
                        changed.caller_model_settings,
                        command.caller_model_settings AS command_caller_model_settings,
                        changed.adjustments
                   FROM session_model_settings_changed_outbox_event AS event
                   JOIN session_model_settings_changed AS changed
                     ON changed.session_id = event.session_id
                    AND changed.installed_defaults_version =
                        event.installed_defaults_version
                   JOIN session_defaults_version AS prior
                     ON prior.session_id = changed.session_id
                    AND prior.version = changed.prior_defaults_version
                   JOIN session_defaults_version AS installed
                    ON installed.session_id = changed.session_id
                    AND installed.version = changed.installed_defaults_version
                   JOIN replace_session_defaults_command AS command
                     ON command.command_id = changed.command_id
                    AND command.result_session_id = changed.session_id
                    AND command.result_installed_version =
                        changed.installed_defaults_version
                    AND command.result_kind = 'applied'
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let prior_version =
                defaults_version_from_numeric(row.try_get("prior_defaults_version")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?;
            let installed_version =
                defaults_version_from_numeric(row.try_get("installed_defaults_version")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?;
            let prior_model_settings: Value = row.try_get("prior_model_settings")?;
            let installed_model_settings: Value = row.try_get("installed_model_settings")?;
            if prior_model_settings != row.try_get::<Value, _>("prior_defaults_model_settings")?
                || installed_model_settings
                    != row.try_get::<Value, _>("installed_defaults_model_settings")?
                || row.try_get::<Value, _>("caller_model_settings")?
                    != row.try_get::<Value, _>("command_caller_model_settings")?
            {
                return Err(OutboxCorruption::InvalidModelSettingsEvent.into());
            }
            let event = SessionModelSettingsChanged::try_new(
                session,
                durable_command_id_from_uuid(row.try_get("command_id")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                prior_version,
                installed_version,
                decode_settings_model_selection(
                    row.try_get("prior_model_kind")?,
                    row.try_get("prior_direct_id")?,
                    row.try_get("prior_alias_id")?,
                )?,
                decode_settings_model_selection(
                    row.try_get("installed_model_kind")?,
                    row.try_get("installed_direct_id")?,
                    row.try_get("installed_alias_id")?,
                )?,
                model_settings_from_json(prior_model_settings)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                model_settings_from_json(installed_model_settings)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                model_settings_overlay_from_json(row.try_get::<Value, _>("caller_model_settings")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                model_change_adjustments_from_json(row.try_get::<Value, _>("adjustments")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
            )
            .ok_or(OutboxCorruption::InvalidModelSettingsEvent)?;
            DispatchedOutboxEventKind::SessionModelSettingsChanged(event)
        }
        OutboxEventDiscriminator::TurnModelSettingsResolved => {
            let row = sqlx::query(
                "WITH RECURSIVE configuration_origin AS (
                     SELECT queued.*
                       FROM turn_model_settings_resolved_outbox_event AS event
                       JOIN turn_model_settings_resolved AS settings
                         ON settings.accepted_input_id = event.accepted_input_id
                        AND settings.session_id = event.session_id
                       JOIN queued_input_origin AS queued
                         ON queued.accepted_input_id = settings.accepted_input_id
                        AND queued.turn_id = settings.turn_id
                        AND queued.session_id = settings.session_id
                      WHERE event.event_sequence = $1
                        AND event.session_id = $2
                     UNION
                     SELECT source.*
                       FROM configuration_origin AS current
                       JOIN queued_input_origin AS source
                         ON source.turn_id = current.source_configuration_turn_id
                        AND source.session_id = current.session_id
                 )
                 SELECT settings.accepted_input_id, settings.turn_id,
                        settings.defaults_version,
                        settings.selected_direct_model_id,
                        settings.per_call_model_settings,
                        settings.resolved_model_settings,
                        settings.adjusted_from_selection_id,
                        settings.adjustments,
                        queued.requested_model_kind,
                        queued.requested_direct_model_selection_id,
                        queued.requested_model_alias_id,
                        queued.frozen_model_kind,
                        queued.frozen_direct_model_selection_id,
                        queued.frozen_model_alias_id,
                        queued.frozen_alias_selected_direct_id,
                        queued.defaults_version AS origin_defaults_version,
                        origin_accepted.model_settings_override
                            AS origin_per_call_model_settings,
                        defaults.model_settings AS origin_defaults_model_settings
                   FROM turn_model_settings_resolved_outbox_event AS event
                   JOIN turn_model_settings_resolved AS settings
                     ON settings.accepted_input_id = event.accepted_input_id
                    AND settings.session_id = event.session_id
                   JOIN configuration_origin AS queued
                     ON queued.session_id = settings.session_id
                    AND queued.source_configuration_turn_id IS NULL
                   JOIN accepted_input AS origin_accepted
                     ON origin_accepted.accepted_input_id = queued.accepted_input_id
                    AND origin_accepted.session_id = queued.session_id
                    AND origin_accepted.origin_turn_id = queued.turn_id
                   JOIN session_defaults_version AS defaults
                     ON defaults.session_id = queued.session_id
                    AND defaults.version = queued.defaults_version
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let frozen = decode_settings_frozen_model(
                row.try_get("frozen_model_kind")?,
                row.try_get("frozen_direct_model_selection_id")?,
                row.try_get("frozen_model_alias_id")?,
                row.try_get("frozen_alias_selected_direct_id")?,
            )?;
            if frozen.selected_direct().into_uuid()
                != row.try_get::<Uuid, _>("selected_direct_model_id")?
            {
                return Err(OutboxCorruption::InvalidModelSettingsEvent.into());
            }
            let event = TurnModelSettingsResolved::try_new(
                AcceptedInputId::from_uuid(row.try_get("accepted_input_id")?),
                TurnId::from_uuid(row.try_get("turn_id")?),
                defaults_version_from_numeric(row.try_get("defaults_version")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                frozen,
                model_settings_overlay_from_json(
                    row.try_get::<Value, _>("per_call_model_settings")?,
                )
                .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                model_settings_from_json(row.try_get::<Value, _>("resolved_model_settings")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
                row.try_get::<Option<Uuid>, _>("adjusted_from_selection_id")?
                    .map(DirectModelSelection::from_uuid),
                model_change_adjustments_from_json(row.try_get::<Value, _>("adjustments")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?,
            )
            .ok_or(OutboxCorruption::InvalidModelSettingsEvent)?;
            let requested = decode_settings_model_selection(
                row.try_get("requested_model_kind")?,
                row.try_get("requested_direct_model_selection_id")?,
                row.try_get("requested_model_alias_id")?,
            )?;
            if requested != requested_from_frozen(event.selection()) {
                return Err(OutboxCorruption::InvalidModelSettingsEvent.into());
            }
            let origin_defaults_version =
                defaults_version_from_numeric(row.try_get("origin_defaults_version")?)
                    .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?;
            let origin_per_call = model_settings_overlay_from_json(
                row.try_get::<Value, _>("origin_per_call_model_settings")?,
            )
            .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?;
            let origin_defaults = model_settings_from_json(
                row.try_get::<Value, _>("origin_defaults_model_settings")?,
            )
            .map_err(|_| OutboxCorruption::InvalidModelSettingsEvent)?;
            if event.defaults_version() != origin_defaults_version
                || event.per_call_override() != origin_per_call
                || !crate::model_settings_resolution::matches_defaults(&event, origin_defaults)
            {
                return Err(OutboxCorruption::InvalidModelSettingsEvent.into());
            }
            DispatchedOutboxEventKind::TurnModelSettingsResolved(event)
        }
        OutboxEventDiscriminator::InputAccepted => {
            // The two admitted shapes are the two ways an input is authored:
            // by an applied submit command, or by the goal machinery, which
            // mints a commandless input and proves it with a `goal_turn` row.
            // A generation owning the turn does not disqualify the first shape:
            // a goal turn bound to a turn a command already accepted — what
            // repository-watch dispatch commits — is exactly a commanded input
            // that also carries a `goal_turn` row.
            let row = sqlx::query(
                "SELECT event.accepted_input_id, event.turn_id,
                        event.acceptance_position, accepted.content_text
                   FROM input_accepted_outbox_event AS event
                   JOIN accepted_input AS accepted
                     ON accepted.accepted_input_id = event.accepted_input_id
                    AND accepted.session_id = event.session_id
                    AND accepted.acceptance_position = event.acceptance_position
                    AND accepted.origin_turn_id = event.turn_id
                   LEFT JOIN submit_input_command AS command
                     ON command.command_id = accepted.accepting_command_id
                    AND command.session_id = event.session_id
                    AND command.result_session_id = event.session_id
                    AND command.result_kind = 'applied'
                    AND command.result_accepted_input_id = event.accepted_input_id
                    AND command.content_kind = 'text'
                    AND command.content_text = accepted.content_text
                   LEFT JOIN goal_turn AS goal
                     ON goal.session_id = event.session_id
                    AND goal.accepted_input_id = event.accepted_input_id
                    AND goal.turn_id = event.turn_id
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
                            accepted.accepting_command_id IS NOT NULL
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
                            )
                        )
                        OR (
                            accepted.accepting_command_id IS NULL
                            AND command.command_id IS NULL
                            AND accepted.disposition_kind = 'origin_of'
                            AND goal.turn_id = event.turn_id
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
        OutboxEventDiscriminator::GoalTurnRetired => {
            let turn = sqlx::query_scalar::<_, Uuid>(
                "SELECT event.turn_id
                   FROM goal_turn_retired_outbox_event AS event
                   JOIN goal_turn AS goal
                     ON goal.session_id = event.session_id
                    AND goal.turn_id = event.turn_id
                   JOIN turn_lifecycle AS lifecycle
                     ON lifecycle.session_id = goal.session_id
                    AND lifecycle.turn_id = goal.turn_id
                    AND lifecycle.state_kind = 'queued'
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2
                    AND NOT goal_turn_is_runtime_relevant(
                        goal.session_id,
                        goal.turn_id
                    )",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            DispatchedOutboxEventKind::GoalTurnRetired {
                turn: TurnId::from_uuid(turn),
            }
        }
        OutboxEventDiscriminator::TurnActivated => {
            let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
                "SELECT event.turn_id, event.current_attempt_id,
                        (
                            initial_attempt.turn_attempt_id IS NOT NULL
                            AND turn.turn_id IS NOT NULL
                            AND (
                                (
                                    turn.state_kind = 'active'
                                    AND (
                                        turn.active_phase_kind <> 'running'
                                        OR authoritative_attempt.turn_attempt_id
                                            IS NOT NULL
                                    )
                                )
                                OR (
                                    turn.state_kind = 'terminal'
                                    AND authoritative_attempt.turn_attempt_id
                                        IS NOT NULL
                                )
                            )
                        ) AS lifecycle_correlated
                   FROM turn_activated_outbox_event AS event
                   LEFT JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                   LEFT JOIN turn_attempt AS initial_attempt
                     ON initial_attempt.turn_attempt_id =
                        event.current_attempt_id
                    AND initial_attempt.turn_id = event.turn_id
                    AND initial_attempt.session_id = event.session_id
                    AND initial_attempt.continued_from_attempt_id IS NULL
                   LEFT JOIN turn_attempt AS authoritative_attempt
                     ON authoritative_attempt.turn_attempt_id =
                        CASE turn.state_kind
                            WHEN 'active' THEN turn.current_attempt_id
                            WHEN 'terminal' THEN turn.terminal_attempt_id
                        END
                    AND authoritative_attempt.turn_id = event.turn_id
                    AND authoritative_attempt.session_id = event.session_id
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
        OutboxEventDiscriminator::TurnFailed => {
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
                                )
                                OR (
                                    turn.terminal_model_call_id IS NOT NULL
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
        OutboxEventDiscriminator::ModelCallTransition => {
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
            let authoritative_state_kind: String = row.try_get("authoritative_state_kind")?;
            let authoritative_terminal_disposition: Option<String> =
                row.try_get("authoritative_terminal_disposition_kind")?;
            let authoritative_state = decode_model_call_state(
                &authoritative_state_kind,
                authoritative_terminal_disposition.as_deref(),
            )?;
            match state {
                DispatchedModelCallState::Prepared => {}
                DispatchedModelCallState::InFlight
                    if authoritative_state == DispatchedModelCallState::Prepared =>
                {
                    return Err(OutboxCorruption::InvalidModelCallState.into());
                }
                DispatchedModelCallState::CancellationRequested
                    if matches!(
                        authoritative_state,
                        DispatchedModelCallState::Prepared | DispatchedModelCallState::InFlight
                    ) =>
                {
                    return Err(OutboxCorruption::InvalidModelCallState.into());
                }
                DispatchedModelCallState::Terminal(_) if authoritative_state != state => {
                    return Err(OutboxCorruption::InvalidTerminalEventCorrelation.into());
                }
                _ => {}
            }
            DispatchedOutboxEventKind::ModelCallTransition {
                turn: TurnId::from_uuid(row.try_get("turn_id")?),
                call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
                state,
            }
        }
        OutboxEventDiscriminator::ToolBatchTransition => {
            let row: Option<ToolBatchTransitionRow> = sqlx::query_as(
                "SELECT event.turn_id, event.producing_model_call_id,
                            event.transition_kind, event.frontier_id,
                            event.tool_attempt_id,
                            CASE event.transition_kind
                                WHEN 'proposed' THEN
                                    event.frontier_id =
                                        round.boundary_frontier_id
                                    AND event.tool_attempt_id IS NULL
                                WHEN 'results_projected' THEN
                                    event.frontier_id IS NOT NULL
                                    AND event.frontier_id <>
                                        round.boundary_frontier_id
                                    AND event.tool_attempt_id IS NULL
                                    AND result_frontier.member_count =
                                        boundary_frontier.member_count
                                        + round.request_count
                                    AND context_frontier_preserves_prefix(
                                        event.session_id,
                                        round.boundary_frontier_id,
                                        event.frontier_id
                                    )
                                    AND NOT EXISTS (
                                        SELECT 1
                                          FROM tool_request AS request
                                          LEFT JOIN semantic_transcript_entry
                                                    AS result
                                            ON result.source_session_id =
                                               event.session_id
                                           AND result.payload_kind IN (
                                                'tool_execution_result',
                                                'tool_denied',
                                                'tool_closed_by_turn_end',
                                                'delegation_result'
                                           )
                                           AND (
                                                result.tool_result_request_id =
                                                    request.request_id
                                                OR EXISTS (
                                                    SELECT 1
                                                      FROM tool_attempt
                                                           AS result_attempt
                                                     WHERE
                                                        result_attempt.attempt_id =
                                                        result.tool_result_attempt_id
                                                       AND
                                                        result_attempt.request_id =
                                                        request.request_id
                                                )
                                           )
                                          LEFT JOIN context_frontier_member
                                                    AS member
                                            ON member.owning_session_id =
                                               event.session_id
                                           AND member.context_frontier_id =
                                               event.frontier_id
                                           AND member.member_position =
                                               boundary_frontier.member_count
                                               + request.request_ordinal + 1
                                           AND member.source_session_id =
                                               result.source_session_id
                                           AND member.semantic_entry_id =
                                               result.semantic_entry_id
                                         WHERE
                                            request.producing_model_call_id =
                                            event.producing_model_call_id
                                           AND member.semantic_entry_id IS NULL
                                    )
                                WHEN 'recovery_required' THEN
                                    event.frontier_id IS NULL
                                    AND recovery_attempt.attempt_id =
                                        event.tool_attempt_id
                                    AND recovery_request.producing_model_call_id =
                                        event.producing_model_call_id
                                ELSE false
                            END
                       FROM tool_batch_transition_outbox_event AS event
                       JOIN tool_round AS round
                         ON round.producing_model_call_id =
                            event.producing_model_call_id
                        AND round.turn_id = event.turn_id
                        AND round.session_id = event.session_id
                       JOIN context_frontier AS boundary_frontier
                         ON boundary_frontier.owning_session_id =
                            event.session_id
                        AND boundary_frontier.context_frontier_id =
                            round.boundary_frontier_id
                       LEFT JOIN context_frontier AS result_frontier
                         ON result_frontier.owning_session_id =
                            event.session_id
                        AND result_frontier.context_frontier_id =
                            event.frontier_id
                       LEFT JOIN tool_attempt AS recovery_attempt
                         ON recovery_attempt.attempt_id =
                            event.tool_attempt_id
                        AND recovery_attempt.turn_id = event.turn_id
                        AND recovery_attempt.session_id = event.session_id
                        AND recovery_attempt.state_kind = 'terminal'
                        AND recovery_attempt.terminal_disposition_kind =
                            'ambiguous'
                       LEFT JOIN tool_request AS recovery_request
                         ON recovery_request.request_id =
                            recovery_attempt.request_id
                      WHERE event.event_sequence = $1
                        AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, producing_call, transition, frontier, attempt, valid) =
                row.ok_or(OutboxCorruption::MissingTypedRecord)?;
            if !valid {
                return Err(OutboxCorruption::InvalidLifecycleEventCorrelation.into());
            }
            let state = match (transition.as_str(), frontier, attempt) {
                ("proposed", Some(frontier), None) => DispatchedToolBatchState::Proposed {
                    frontier: ContextFrontierId::from_uuid(frontier),
                },
                ("results_projected", Some(frontier), None) => {
                    DispatchedToolBatchState::ResultsProjected {
                        frontier: ContextFrontierId::from_uuid(frontier),
                    }
                }
                ("recovery_required", None, Some(attempt)) => {
                    DispatchedToolBatchState::RecoveryRequired {
                        attempt: ToolAttemptId::from_uuid(attempt),
                    }
                }
                _ => return Err(OutboxCorruption::InvalidLifecycleEventCorrelation.into()),
            };
            DispatchedOutboxEventKind::ToolBatchTransition {
                turn: TurnId::from_uuid(turn),
                producing_call: ModelCallId::from_uuid(producing_call),
                state,
            }
        }
        OutboxEventDiscriminator::ToolApprovalDecided => {
            let row: ToolApprovalDecidedRow = sqlx::query_as(
                "SELECT event.turn_id, event.request_id
                   FROM tool_approval_decided_outbox_event AS event
                   JOIN tool_request AS request
                     ON request.request_id = event.request_id
                    AND request.turn_id = event.turn_id
                    AND request.session_id = event.session_id
                   JOIN tool_approval_decision AS approval
                     ON approval.request_id = event.request_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::MissingTypedRecord)?;
            let request = ToolRequestId::from_uuid(row.request_id);
            let mut approvals =
                match crate::tool_loop::load_approvals_by_request(transaction, &[request]).await {
                    Ok(approvals) => approvals,
                    Err(crate::tool_loop::ToolLoopRepositoryError::Database { source, .. }) => {
                        return Err(source.into());
                    }
                    Err(_) => {
                        return Err(OutboxCorruption::InvalidLifecycleEventCorrelation.into());
                    }
                };
            let approval = approvals
                .remove(&request)
                .ok_or(OutboxCorruption::InvalidLifecycleEventCorrelation)?;
            let Some(decider) = approval.decider().copied() else {
                return Err(OutboxCorruption::InvalidLifecycleEventCorrelation.into());
            };
            DispatchedOutboxEventKind::ToolApprovalDecided {
                turn: TurnId::from_uuid(row.turn_id),
                approval,
                decider,
            }
        }
        OutboxEventDiscriminator::ContextCompacted => {
            let row: (Uuid, Uuid, Decimal, Uuid, Uuid) = sqlx::query_as(
                "SELECT event.context_compaction_id, event.model_call_id,
                        event.through_position, event.summary_entry_id,
                        event.result_frontier_id
                   FROM context_compacted_outbox_event AS event
                   JOIN context_compaction AS compaction
                     ON compaction.context_compaction_id =
                        event.context_compaction_id
                    AND compaction.session_id = event.session_id
                    AND compaction.producing_call_id = event.model_call_id
                    AND compaction.summary_entry_id = event.summary_entry_id
                    AND compaction.result_frontier_id = event.result_frontier_id
                   JOIN context_compaction_model_call AS call
                     ON call.model_call_id = event.model_call_id
                    AND call.session_id = event.session_id
                    AND call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'completed'
                   JOIN semantic_transcript_entry AS summary
                     ON summary.source_session_id = event.session_id
                    AND summary.semantic_entry_id = event.summary_entry_id
                    AND summary.payload_kind = 'context_summary'
                    AND summary.context_summary_producing_call_id =
                        event.model_call_id
                   JOIN context_frontier AS frontier
                     ON frontier.owning_session_id = event.session_id
                    AND frontier.context_frontier_id = event.result_frontier_id
                   JOIN compact_session_command AS command
                     ON command.session_id = event.session_id
                    AND command.result_kind = 'applied'
                    AND command.result_context_compaction_id =
                        event.context_compaction_id
                    AND command.model_call_id = event.model_call_id
                    AND command.result_through_position =
                        event.through_position
                    AND command.result_summary_entry_id = event.summary_entry_id
                    AND command.result_frontier_id = event.result_frontier_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::ContextCompacted {
                compaction: ContextCompactionId::from_uuid(row.0),
                call: ModelCallId::from_uuid(row.1),
                through_position: decode_positive_sequence(row.2)?,
                summary_entry: SemanticTranscriptEntryId::from_uuid(row.3),
                result_frontier: ContextFrontierId::from_uuid(row.4),
            }
        }
        OutboxEventDiscriminator::TurnCompleted => {
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
        OutboxEventDiscriminator::TurnRefused => {
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
        OutboxEventDiscriminator::TurnCancelled => {
            let row: Option<TurnCancelledOutboxRow> = sqlx::query_as(
                "SELECT event.turn_id AS turn_id,
                        event.cancellation_entry_id AS cancellation_entry_id,
                        event.terminal_frontier_id AS terminal_frontier_id
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
                    AND terminal_call.terminal_disposition_kind
                        IN ('cancelled', 'completed')
                   LEFT JOIN tool_round AS terminal_round
                     ON terminal_round.producing_model_call_id =
                        terminal_call.model_call_id
                    AND terminal_round.turn_id = event.turn_id
                    AND terminal_round.session_id = event.session_id
                    AND terminal_round.boundary_kind = 'closed_by_turn_end'
                    AND terminal_round.boundary_frontier_id =
                        event.terminal_frontier_id
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2
                    AND (
                        (
                            turn.terminal_model_call_id IS NULL
                            AND terminal_call.model_call_id IS NULL
                        )
                        OR (
                            turn.terminal_model_call_id IS NOT NULL
                            AND terminal_call.model_call_id =
                                turn.terminal_model_call_id
                            AND (
                                terminal_call.terminal_disposition_kind =
                                    'cancelled'
                                OR terminal_round.producing_model_call_id =
                                    terminal_call.model_call_id
                            )
                        )
                    )",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let row = row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            DispatchedOutboxEventKind::TurnCancelled {
                turn: TurnId::from_uuid(row.turn_id),
                cancellation_entry: SemanticTranscriptEntryId::from_uuid(row.cancellation_entry_id),
                terminal_frontier: ContextFrontierId::from_uuid(row.terminal_frontier_id),
            }
        }
        OutboxEventDiscriminator::TurnReconciliationRequired => {
            let row: Option<(Uuid, Option<Uuid>, Option<Uuid>, Uuid)> = sqlx::query_as(
                "SELECT event.turn_id, event.model_call_id,
                        event.tool_attempt_id, event.terminal_frontier_id
                   FROM turn_reconciliation_required_outbox_event AS event
                   JOIN turn_lifecycle AS turn
                     ON turn.turn_id = event.turn_id
                    AND turn.session_id = event.session_id
                    AND turn.state_kind = 'terminal'
                    AND turn.terminal_disposition_kind = 'reconciliation_required'
                    AND turn.terminal_frontier_id = event.terminal_frontier_id
                    AND (
                        (
                            event.model_call_id IS NOT NULL
                            AND event.tool_attempt_id IS NULL
                            AND turn.terminal_model_call_id =
                                event.model_call_id
                            AND turn.terminal_tool_attempt_id IS NULL
                        )
                        OR (
                            event.model_call_id IS NULL
                            AND event.tool_attempt_id IS NOT NULL
                            AND turn.terminal_model_call_id IS NULL
                            AND turn.terminal_tool_attempt_id =
                                event.tool_attempt_id
                        )
                    )
                  WHERE event.event_sequence = $1
                    AND event.session_id = $2",
            )
            .bind(Decimal::from(expected_sequence))
            .bind(stored_session)
            .fetch_optional(&mut **transaction)
            .await?;
            let (turn, call, tool_attempt, terminal_frontier) =
                row.ok_or(OutboxCorruption::InvalidTerminalEventCorrelation)?;
            let operation = match (call, tool_attempt) {
                (Some(call), None) => {
                    let valid: Option<bool> = sqlx::query_scalar(
                        "SELECT true
                           FROM turn_lifecycle AS turn
                           JOIN model_call AS call
                             ON call.model_call_id = $3
                            AND call.turn_id = turn.turn_id
                            AND call.session_id = turn.session_id
                            AND call.turn_attempt_id =
                                turn.terminal_attempt_id
                            AND call.state_kind = 'terminal'
                            AND call.terminal_disposition_kind = 'ambiguous'
                           JOIN turn_attempt AS terminal_attempt
                             ON terminal_attempt.turn_attempt_id =
                                turn.terminal_attempt_id
                            AND terminal_attempt.turn_id = turn.turn_id
                            AND terminal_attempt.session_id = turn.session_id
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
                                        turn.turn_id
                                )
                                OR (
                                    terminal_attempt.end_variant =
                                        'without_stop'
                                    AND terminal_attempt.interrupt_command_id
                                        IS NULL
                                    AND terminal_attempt.interrupt_predecessor_turn_id
                                        IS NULL
                                )
                            )
                          WHERE turn.turn_id = $1
                            AND turn.session_id = $2
                            AND turn.terminal_model_call_id = $3",
                    )
                    .bind(turn)
                    .bind(stored_session)
                    .bind(call)
                    .fetch_optional(&mut **transaction)
                    .await?;
                    if valid.is_none() {
                        return Err(OutboxCorruption::InvalidTerminalEventCorrelation.into());
                    }
                    DispatchedReconciliationOperation::ModelCall(ModelCallId::from_uuid(call))
                }
                (None, Some(attempt)) => {
                    let valid: Option<bool> = sqlx::query_scalar(
                        "SELECT true
                           FROM turn_lifecycle AS turn
                           JOIN tool_attempt AS attempt
                             ON attempt.attempt_id = $3
                            AND attempt.turn_id = turn.turn_id
                            AND attempt.session_id = turn.session_id
                            AND attempt.issuing_turn_attempt_id =
                                turn.terminal_attempt_id
                            AND attempt.state_kind = 'terminal'
                            AND attempt.terminal_disposition_kind = 'ambiguous'
                           JOIN turn_attempt AS terminal_attempt
                             ON terminal_attempt.turn_attempt_id =
                                turn.terminal_attempt_id
                            AND terminal_attempt.turn_id = turn.turn_id
                            AND terminal_attempt.session_id = turn.session_id
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
                                        turn.turn_id
                                )
                                OR (
                                    terminal_attempt.end_variant =
                                        'without_stop'
                                    AND terminal_attempt.interrupt_command_id
                                        IS NULL
                                    AND terminal_attempt.interrupt_predecessor_turn_id
                                        IS NULL
                                )
                            )
                          WHERE turn.turn_id = $1
                            AND turn.session_id = $2
                            AND turn.terminal_tool_attempt_id = $3",
                    )
                    .bind(turn)
                    .bind(stored_session)
                    .bind(attempt)
                    .fetch_optional(&mut **transaction)
                    .await?;
                    if valid.is_none() {
                        return Err(OutboxCorruption::InvalidTerminalEventCorrelation.into());
                    }
                    DispatchedReconciliationOperation::ToolAttempt(ToolAttemptId::from_uuid(
                        attempt,
                    ))
                }
                (Some(_), Some(_)) | (None, None) => {
                    return Err(OutboxCorruption::InvalidTerminalEventCorrelation.into());
                }
            };
            DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn: TurnId::from_uuid(turn),
                operation,
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
            }
        }
        OutboxEventDiscriminator::RunnerStateTransition => {
            load_runner_state_transition(transaction, expected_sequence, stored_session).await?
        }
        OutboxEventDiscriminator::DelegationUpdate => DispatchedOutboxEventKind::DelegationUpdate(
            load_delegation_update(transaction, expected_sequence, stored_session).await?,
        ),
        OutboxEventDiscriminator::DelegationWake => DispatchedOutboxEventKind::DelegationWake(
            load_delegation_wake(transaction, expected_sequence, stored_session).await?,
        ),
    };

    Ok((
        allocated,
        event_beyond_allocated,
        Some(DispatchedOutboxEvent {
            sequence: expected_sequence,
            session,
            kind,
        }),
    ))
}

async fn load_runner_state_transition(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expected_sequence: u64,
    stored_session: Uuid,
) -> Result<DispatchedOutboxEventKind, OutboxDispatchError> {
    let row = sqlx::query(
        "SELECT event.runner_id, event.placement_revision,
                event.sandbox_profile, event.working_directory,
                event.state_kind, event.connection_enrollment_id,
                event.connection_epoch, event.connection_event_ordinal,
                placement.event_kind AS source_event_kind,
                placement.state_kind AS source_state_kind,
                placement.requested_sandbox_profile AS source_sandbox_profile,
                placement.requested_working_directory AS source_working_directory,
                placement.selector_runner_id AS source_selector_runner_id,
                placement.pinned_runner_id AS source_pinned_runner_id,
                placement.lost_runner_id AS source_lost_runner_id,
                placement.registration_enrollment_id AS source_registration_enrollment_id,
                connection.state_kind AS source_connection_state_kind,
                connection.cause_kind AS source_connection_cause_kind,
                CASE
                    WHEN connection.cause_kind = 'established' THEN
                        connection.event_ordinal = 1
                        AND connection_prior.connection_epoch + 1 =
                            connection.connection_epoch
                        AND connection_prior.state_kind = 'suspect'
                    WHEN connection.cause_kind = 'heartbeat_recovered' THEN
                        connection_prior.connection_epoch =
                            connection.connection_epoch
                        AND connection_prior.event_ordinal + 1 =
                            connection.event_ordinal
                        AND connection_prior.state_kind = 'suspect'
                    ELSE false
                END AS source_connection_predecessor_matches,
                prior.lost_runner_id AS prior_lost_runner_id,
                prior.requested_working_directory AS prior_working_directory
           FROM runner_state_transition_outbox_event AS event
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = event.session_id
            AND placement.event_ordinal = event.placement_event_ordinal
            AND placement.placement_revision = event.placement_revision
           LEFT JOIN runner_connection_event AS connection
             ON connection.enrollment_id = event.connection_enrollment_id
            AND connection.connection_epoch = event.connection_epoch
            AND connection.event_ordinal = event.connection_event_ordinal
           LEFT JOIN LATERAL (
                SELECT earlier.connection_epoch, earlier.event_ordinal,
                       earlier.state_kind
                  FROM runner_connection_event AS earlier
                 WHERE earlier.enrollment_id = connection.enrollment_id
                   AND (
                        earlier.connection_epoch < connection.connection_epoch
                        OR (
                            earlier.connection_epoch = connection.connection_epoch
                            AND earlier.event_ordinal < connection.event_ordinal
                        )
                   )
                 ORDER BY earlier.connection_epoch DESC,
                          earlier.event_ordinal DESC
                 LIMIT 1
           ) AS connection_prior ON true
           LEFT JOIN runner_session_placement_record AS prior
             ON prior.session_id = placement.session_id
            AND prior.event_ordinal + 1 = placement.event_ordinal
          WHERE event.event_sequence = $1
            AND event.event_kind = $2
            AND event.storage_version = $3
            AND event.session_id = $4",
    )
    .bind(Decimal::from(expected_sequence))
    .bind(RUNNER_STATE_TRANSITION)
    .bind(STORAGE_VERSION)
    .bind(stored_session)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(OutboxCorruption::MissingTypedRecord)?;
    let placement_revision = RunnerGeneration::try_from_u64(decode_positive_sequence(
        row.try_get("placement_revision")?,
    )?)
    .ok_or(OutboxCorruption::InvalidRunnerEvent)?;
    let sandbox_text = row.try_get::<String, _>("sandbox_profile")?;
    let sandbox =
        runner_sandbox_from_str(&sandbox_text).ok_or(OutboxCorruption::InvalidRunnerEvent)?;
    let working_directory = row
        .try_get::<Option<String>, _>("working_directory")?
        .map(RunnerWorkingDirectory::try_new)
        .transpose()
        .map_err(|_| OutboxCorruption::InvalidRunnerEvent)?;
    let state_kind = row.try_get::<String, _>("state_kind")?;
    let state = dispatched_runner_state_from_str(&state_kind)
        .ok_or(OutboxCorruption::InvalidRunnerEvent)?;
    let connection_source_shape_matches = match state {
        DispatchedRunnerState::Suspect | DispatchedRunnerState::Connected => {
            row.try_get::<Option<Uuid>, _>("connection_enrollment_id")?
                .is_some()
                && row
                    .try_get::<Option<Decimal>, _>("connection_epoch")?
                    .is_some()
                && row
                    .try_get::<Option<Decimal>, _>("connection_event_ordinal")?
                    .is_some()
        }
        DispatchedRunnerState::Pinned
        | DispatchedRunnerState::RunnerLostBeforePin
        | DispatchedRunnerState::RunnerLost
        | DispatchedRunnerState::Replaced
        | DispatchedRunnerState::WorkingDirectoryChanged
        | DispatchedRunnerState::Abandoned => {
            row.try_get::<Option<Uuid>, _>("connection_enrollment_id")?
                .is_none()
                && row
                    .try_get::<Option<Decimal>, _>("connection_epoch")?
                    .is_none()
                && row
                    .try_get::<Option<Decimal>, _>("connection_event_ordinal")?
                    .is_none()
        }
    };
    let runner_uuid = row.try_get::<Uuid, _>("runner_id")?;
    let source_sandbox = row.try_get::<String, _>("source_sandbox_profile")?;
    let source_working_directory = row.try_get::<Option<String>, _>("source_working_directory")?;
    if source_sandbox != row.try_get::<String, _>("sandbox_profile")?
        || source_working_directory != row.try_get::<Option<String>, _>("working_directory")?
    {
        return Err(OutboxCorruption::InvalidRunnerEvent.into());
    }
    let source_event = row.try_get::<String, _>("source_event_kind")?;
    let source_state = row.try_get::<String, _>("source_state_kind")?;
    let source_selector = row.try_get::<Option<Uuid>, _>("source_selector_runner_id")?;
    let source_pinned = row.try_get::<Option<Uuid>, _>("source_pinned_runner_id")?;
    let source_lost = row.try_get::<Option<Uuid>, _>("source_lost_runner_id")?;
    let source_matches = match state {
        DispatchedRunnerState::Pinned => {
            source_event == "pinned"
                && source_state == "pinned"
                && source_pinned == Some(runner_uuid)
        }
        DispatchedRunnerState::Suspect => {
            source_state == "pinned"
                && source_pinned == Some(runner_uuid)
                && row.try_get::<Option<Uuid>, _>("source_registration_enrollment_id")?
                    == row.try_get::<Option<Uuid>, _>("connection_enrollment_id")?
                && row
                    .try_get::<Option<String>, _>("source_connection_state_kind")?
                    .as_deref()
                    == Some("suspect")
                && row
                    .try_get::<Option<String>, _>("source_connection_cause_kind")?
                    .as_deref()
                    == Some("heartbeat_missed")
        }
        DispatchedRunnerState::Connected => {
            source_state == "pinned"
                && source_pinned == Some(runner_uuid)
                && row.try_get::<Option<Uuid>, _>("source_registration_enrollment_id")?
                    == row.try_get::<Option<Uuid>, _>("connection_enrollment_id")?
                && row
                    .try_get::<Option<String>, _>("source_connection_state_kind")?
                    .as_deref()
                    == Some("connected")
                && matches!(
                    row.try_get::<Option<String>, _>("source_connection_cause_kind")?
                        .as_deref(),
                    Some("established" | "heartbeat_recovered")
                )
                && row.try_get::<Option<bool>, _>("source_connection_predecessor_matches")?
                    == Some(true)
        }
        DispatchedRunnerState::RunnerLostBeforePin => {
            source_event == "runner_lost_before_pin"
                && source_state == "runner_lost_before_pin"
                && source_lost == Some(runner_uuid)
        }
        DispatchedRunnerState::RunnerLost => {
            source_event == "runner_lost"
                && source_state == "runner_lost"
                && source_lost == Some(runner_uuid)
        }
        DispatchedRunnerState::Replaced => {
            (source_event == "pre_pin_replaced"
                && source_state == "unpinned"
                && source_selector == Some(runner_uuid))
                || (source_event == "runner_replaced"
                    && source_state == "pinned"
                    && source_pinned == Some(runner_uuid)
                    && !(row.try_get::<Option<Uuid>, _>("prior_lost_runner_id")?
                        == Some(runner_uuid)
                        && row.try_get::<Option<String>, _>("prior_working_directory")?
                            != source_working_directory))
        }
        DispatchedRunnerState::WorkingDirectoryChanged => {
            source_event == "runner_replaced"
                && source_state == "pinned"
                && source_pinned == Some(runner_uuid)
                && row.try_get::<Option<Uuid>, _>("prior_lost_runner_id")? == Some(runner_uuid)
                && row.try_get::<Option<String>, _>("prior_working_directory")?
                    != source_working_directory
        }
        DispatchedRunnerState::Abandoned => {
            source_event == "abandoned"
                && source_state == "runner_abandoned"
                && source_lost == Some(runner_uuid)
        }
    };
    if !connection_source_shape_matches || !source_matches {
        return Err(OutboxCorruption::InvalidRunnerEvent.into());
    }
    Ok(DispatchedOutboxEventKind::RunnerStateTransition {
        runner: RunnerId::from_uuid(runner_uuid),
        placement_revision,
        sandbox,
        working_directory,
        state,
    })
}

async fn load_delegation_update(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expected_sequence: u64,
    stored_session: Uuid,
) -> Result<DispatchedDelegationUpdate, OutboxDispatchError> {
    let row = sqlx::query(
        "SELECT event.update_kind, event.spawning_tool_request_id,
                event.child_session_id, event.policy_kind,
                event.on_parent_stopped, event.on_parent_cancelled,
                event.awaiting_tool_request_id, event.wait_mode,
                event.delegation_event_ordinal, event.outcome_kind,
                event.reason_kind, event.provenance_kind,
                event.provenance_session_id, event.provenance_turn_id,
                event.provenance_goal_generation, event.provenance_command_id,
                event.message_id, event.sender_session_id,
                event.recipient_session_id, event.message_ordinal,
                event.content_text, delivery.delivery_sequence
           FROM delegation_update_outbox_event AS event
           LEFT JOIN session_message_delivery AS delivery
             ON delivery.message_id = event.message_id
            AND delivery.spawning_tool_request_id =
                event.spawning_tool_request_id
          WHERE event.event_sequence = $1
            AND event.session_id = $2",
    )
    .bind(Decimal::from(expected_sequence))
    .bind(stored_session)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(OutboxCorruption::MissingTypedRecord)?;
    let spawning_request = ToolRequestId::from_uuid(row.try_get("spawning_tool_request_id")?);
    let update_kind: String = row.try_get("update_kind")?;
    match decode_delegation_update_kind(&update_kind)? {
        DelegationUpdateStorageKind::ChildSpawned => {
            let child = required_session(&row, "child_session_id")?;
            let policy_kind: Option<String> = row.try_get("policy_kind")?;
            let stopped: Option<String> = row.try_get("on_parent_stopped")?;
            let cancelled: Option<String> = row.try_get("on_parent_cancelled")?;
            let policy_kind = policy_kind
                .as_deref()
                .map(decode_delegation_policy_kind)
                .transpose()?;
            let policy = match (policy_kind, stopped, cancelled) {
                (Some(DelegationPolicyStorageKind::Background), None, None) => {
                    DispatchedDelegationPolicy::Background
                }
                (Some(DelegationPolicyStorageKind::Bound), Some(stopped), Some(cancelled)) => {
                    DispatchedDelegationPolicy::Bound {
                        on_parent_stopped: decode_bound_action(&stopped)?,
                        on_parent_cancelled: decode_bound_action(&cancelled)?,
                    }
                }
                _ => return Err(OutboxCorruption::InvalidDelegationEvent.into()),
            };
            Ok(DispatchedDelegationUpdate::ChildSpawned {
                spawning_request,
                child,
                policy,
            })
        }
        DelegationUpdateStorageKind::ChildWaiting => Ok(DispatchedDelegationUpdate::ChildWaiting {
            spawning_request,
            child: required_session(&row, "child_session_id")?,
            awaiting_request: ToolRequestId::from_uuid(required_uuid(
                &row,
                "awaiting_tool_request_id",
            )?),
            mode: decode_wait_mode(
                row.try_get::<Option<String>, _>("wait_mode")?
                    .as_deref()
                    .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
            )?,
        }),
        DelegationUpdateStorageKind::ChildLifecycleDisposition => {
            Ok(DispatchedDelegationUpdate::ChildLifecycleDisposition {
                spawning_request,
                child: required_session(&row, "child_session_id")?,
                event_ordinal: required_positive_sequence(&row, "delegation_event_ordinal")?,
                outcome: decode_delegation_outcome(
                    row.try_get::<Option<String>, _>("outcome_kind")?
                        .as_deref()
                        .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
                )?,
                reason: decode_delegation_reason(
                    row.try_get::<Option<String>, _>("reason_kind")?
                        .as_deref()
                        .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
                )?,
                provenance: decode_delegation_provenance(&row)?,
            })
        }
        DelegationUpdateStorageKind::ChildResult => Ok(DispatchedDelegationUpdate::ChildResult {
            spawning_request,
            child: required_session(&row, "child_session_id")?,
            outcome: decode_delegation_outcome(
                row.try_get::<Option<String>, _>("outcome_kind")?
                    .as_deref()
                    .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
            )?,
            reason: decode_delegation_reason(
                row.try_get::<Option<String>, _>("reason_kind")?
                    .as_deref()
                    .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
            )?,
            provenance: decode_delegation_provenance(&row)?,
            content: row.try_get("content_text")?,
        }),
        DelegationUpdateStorageKind::SessionMessage => {
            Ok(DispatchedDelegationUpdate::SessionMessage {
                spawning_request,
                message: DelegationMessageId::from_uuid(required_uuid(&row, "message_id")?),
                sender: required_session(&row, "sender_session_id")?,
                recipient: required_session(&row, "recipient_session_id")?,
                message_ordinal: required_positive_sequence(&row, "message_ordinal")?,
                delivery_sequence: required_positive_sequence(&row, "delivery_sequence")?,
                content: row
                    .try_get::<Option<String>, _>("content_text")?
                    .ok_or(OutboxCorruption::InvalidDelegationEvent)?,
            })
        }
    }
}

async fn load_delegation_wake(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expected_sequence: u64,
    stored_session: Uuid,
) -> Result<DispatchedDelegationWake, OutboxDispatchError> {
    let row = sqlx::query(
        "SELECT subject_kind, spawning_tool_request_id,
                result_spawning_request_id, awaiting_tool_request_id,
                message_id
           FROM delegation_wake_outbox_event
          WHERE event_sequence = $1
            AND session_id = $2",
    )
    .bind(Decimal::from(expected_sequence))
    .bind(stored_session)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(OutboxCorruption::MissingTypedRecord)?;
    let spawning_uuid: Uuid = row.try_get("spawning_tool_request_id")?;
    let spawning_request = ToolRequestId::from_uuid(spawning_uuid);
    let subject: String = row.try_get("subject_kind")?;
    match decode_delegation_wake_subject(&subject)? {
        DelegationWakeStorageKind::Result => {
            if row.try_get::<Option<Uuid>, _>("result_spawning_request_id")? != Some(spawning_uuid)
            {
                return Err(OutboxCorruption::InvalidDelegationEvent.into());
            }
            Ok(DispatchedDelegationWake::Result {
                spawning_request,
                awaiting_request: row
                    .try_get::<Option<Uuid>, _>("awaiting_tool_request_id")?
                    .map(ToolRequestId::from_uuid),
            })
        }
        DelegationWakeStorageKind::Message => Ok(DispatchedDelegationWake::Message {
            spawning_request,
            message: DelegationMessageId::from_uuid(required_uuid(&row, "message_id")?),
        }),
    }
}

fn required_uuid(row: &sqlx::postgres::PgRow, column: &str) -> Result<Uuid, OutboxCorruption> {
    row.try_get::<Option<Uuid>, _>(column)
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?
        .ok_or(OutboxCorruption::InvalidDelegationEvent)
}

fn required_session(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<SessionId, OutboxCorruption> {
    required_uuid(row, column).map(session_id_from_uuid)
}

fn required_positive_sequence(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<u64, OutboxCorruption> {
    let value = row
        .try_get::<Option<Decimal>, _>(column)
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?
        .ok_or(OutboxCorruption::InvalidDelegationEvent)?;
    decode_positive_sequence(value).map_err(|_| OutboxCorruption::InvalidDelegationEvent)
}

/// Decodes the durable `update_kind` spelling.
///
/// The spelling table lives in `mapping.rs`, which owns every durable
/// discriminator; this only lifts an unadmitted spelling into the outbox's own
/// fail-closed corruption. Public so a test can drive it with the spellings the
/// durable `CHECK` constraint actually admits.
pub fn decode_delegation_update_kind(
    value: &str,
) -> Result<DelegationUpdateStorageKind, OutboxCorruption> {
    delegation_update_kind_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)
}

/// Decodes the durable `policy_kind` spelling, lifting as above.
pub fn decode_delegation_policy_kind(
    value: &str,
) -> Result<DelegationPolicyStorageKind, OutboxCorruption> {
    delegation_policy_kind_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)
}

/// Decodes the durable `subject_kind` spelling, lifting as above.
pub fn decode_delegation_wake_subject(
    value: &str,
) -> Result<DelegationWakeStorageKind, OutboxCorruption> {
    delegation_wake_subject_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)
}

/// Decodes the durable `on_parent_stopped` / `on_parent_cancelled` spelling.
///
/// Public so a test can drive it with the spellings the durable `CHECK`
/// constraint actually admits, rather than restating the table beside it.
pub fn decode_bound_action(value: &str) -> Result<DispatchedBoundChildAction, OutboxCorruption> {
    match bound_child_action_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)? {
        BoundChildAction::KeepRunning => Ok(DispatchedBoundChildAction::KeepRunning),
        BoundChildAction::Stop => Ok(DispatchedBoundChildAction::Stop),
        BoundChildAction::Cancel => Ok(DispatchedBoundChildAction::Cancel),
    }
}

/// Decodes the durable `wait_mode` spelling.
///
/// Public so a test can drive it with the spellings the durable `CHECK`
/// constraint actually admits, rather than restating the table beside it.
pub fn decode_wait_mode(value: &str) -> Result<DispatchedDelegationWaitMode, OutboxCorruption> {
    match delegation_wait_mode_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)? {
        DelegationWaitMode::Foreground => Ok(DispatchedDelegationWaitMode::Foreground),
        DelegationWaitMode::Background => Ok(DispatchedDelegationWaitMode::Background),
    }
}

/// Decodes the durable `outcome_kind` spelling.
///
/// Public so a test can drive it with the spellings the durable `CHECK`
/// constraint actually admits, rather than restating the table beside it.
pub fn decode_delegation_outcome(
    value: &str,
) -> Result<DispatchedDelegationOutcome, OutboxCorruption> {
    match delegation_outcome_kind_from_str(value).ok_or(OutboxCorruption::InvalidDelegationEvent)? {
        DelegationOutcomeKind::ResultReturned => Ok(DispatchedDelegationOutcome::ResultReturned),
        DelegationOutcomeKind::ChildFailed => Ok(DispatchedDelegationOutcome::ChildFailed),
        DelegationOutcomeKind::ChildStopped => Ok(DispatchedDelegationOutcome::ChildStopped),
        DelegationOutcomeKind::ChildCancelled => Ok(DispatchedDelegationOutcome::ChildCancelled),
        DelegationOutcomeKind::ContinueRunning => Ok(DispatchedDelegationOutcome::ContinueRunning),
        DelegationOutcomeKind::AlreadyTerminal => Ok(DispatchedDelegationOutcome::AlreadyTerminal),
    }
}

/// Decodes the durable `reason_kind` spelling.
///
/// Public so a test can drive it with the spellings the durable `CHECK`
/// constraint actually admits, rather than restating the table beside it.
pub fn decode_delegation_reason(
    value: &str,
) -> Result<DispatchedDelegationReason, OutboxCorruption> {
    match delegation_outcome_reason_from_str(value)
        .ok_or(OutboxCorruption::InvalidDelegationEvent)?
    {
        DelegationOutcomeReason::ChildCompleted => Ok(DispatchedDelegationReason::ChildCompleted),
        DelegationOutcomeReason::ChildExecutionFailed => {
            Ok(DispatchedDelegationReason::ChildExecutionFailed)
        }
        DelegationOutcomeReason::ChildResultUnavailable => {
            Ok(DispatchedDelegationReason::ChildResultUnavailable)
        }
        DelegationOutcomeReason::ChildCancelled => Ok(DispatchedDelegationReason::ChildCancelled),
        // The durable CHECK admits only the parent-and-descendants spelling for
        // these two, so a parent-alone scope is a spelling storage cannot hold.
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => Ok(DispatchedDelegationReason::ParentStoppedWithDescendants),
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => Ok(DispatchedDelegationReason::ParentCancelledWithDescendants),
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAlone,
        }
        | DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAlone,
        } => Err(OutboxCorruption::InvalidDelegationEvent),
    }
}

pub(crate) fn decode_delegation_provenance(
    row: &sqlx::postgres::PgRow,
) -> Result<DispatchedDelegationProvenance, OutboxCorruption> {
    let kind: Option<String> = row
        .try_get("provenance_kind")
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?;
    let session = required_session(row, "provenance_session_id")?;
    let turn: Option<Uuid> = row
        .try_get("provenance_turn_id")
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?;
    let goal: Option<Decimal> = row
        .try_get("provenance_goal_generation")
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?;
    let command: Option<Uuid> = row
        .try_get("provenance_command_id")
        .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?;
    match (kind.as_deref(), turn, goal, command) {
        (Some("child_turn"), Some(turn), None, None) => {
            Ok(DispatchedDelegationProvenance::ChildTurn {
                session,
                turn: TurnId::from_uuid(turn),
            })
        }
        (Some("parent_turn_command"), Some(turn), None, Some(command)) => {
            Ok(DispatchedDelegationProvenance::ParentTurnCommand {
                session,
                turn: TurnId::from_uuid(turn),
                command: DurableCommandId::from_uuid(command),
            })
        }
        (Some("parent_goal_command"), None, Some(goal), Some(command)) => {
            Ok(DispatchedDelegationProvenance::ParentGoalCommand {
                session,
                goal_generation: decode_positive_sequence(goal)
                    .map_err(|_| OutboxCorruption::InvalidDelegationEvent)?,
                command: DurableCommandId::from_uuid(command),
            })
        }
        _ => Err(OutboxCorruption::InvalidDelegationEvent),
    }
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

fn decode_settings_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
) -> Result<ModelSelectionRequest, OutboxCorruption> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        _ => Err(OutboxCorruption::InvalidModelSettingsEvent),
    }
}

fn decode_settings_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, OutboxCorruption> {
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
        _ => Err(OutboxCorruption::InvalidModelSettingsEvent),
    }
}

fn requested_from_frozen(selection: &FrozenModelSelection) -> ModelSelectionRequest {
    match selection {
        FrozenModelSelection::Direct(selection) => ModelSelectionRequest::Direct(*selection),
        FrozenModelSelection::FrozenAlias { alias, .. } => ModelSelectionRequest::Alias(*alias),
    }
}

pub(crate) struct RunnerConnectionOutboxSource {
    pub(crate) enrollment: RunnerEnrollmentId,
    pub(crate) epoch: u64,
    pub(crate) event_ordinal: u64,
}

pub(crate) struct RunnerStateOutboxSource {
    pub(crate) placement_event_ordinal: u64,
    pub(crate) connection: Option<RunnerConnectionOutboxSource>,
}

pub(crate) struct RunnerStateOutboxEvent {
    pub(crate) session: SessionId,
    pub(crate) runner: RunnerId,
    pub(crate) placement_revision: RunnerGeneration,
    pub(crate) sandbox: RunnerSandboxProfile,
    pub(crate) working_directory: Option<RunnerWorkingDirectory>,
    pub(crate) state: DispatchedRunnerState,
    pub(crate) source: RunnerStateOutboxSource,
}

/// Exact relational source used only by runner PostgreSQL integration tests.
#[cfg(feature = "postgres-integration")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct RunnerStateTransitionOutboxTestSource {
    placement_event_ordinal: u64,
    connection: Option<(RunnerEnrollmentId, RunnerConnectionEpoch, NonZeroU64)>,
}

#[cfg(feature = "postgres-integration")]
impl RunnerStateTransitionOutboxTestSource {
    /// Names one placement-record source.
    pub const fn placement(placement_event_ordinal: u64) -> Self {
        Self {
            placement_event_ordinal,
            connection: None,
        }
    }

    /// Names one placement record plus exact connection event.
    pub const fn connection(
        placement_event_ordinal: u64,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        event_ordinal: NonZeroU64,
    ) -> Self {
        Self {
            placement_event_ordinal,
            connection: Some((enrollment, epoch, event_ordinal)),
        }
    }

    fn into_source(self) -> RunnerStateOutboxSource {
        RunnerStateOutboxSource {
            placement_event_ordinal: self.placement_event_ordinal,
            connection: self.connection.map(|(enrollment, epoch, event_ordinal)| {
                RunnerConnectionOutboxSource {
                    enrollment,
                    epoch: epoch.get(),
                    event_ordinal: event_ordinal.get(),
                }
            }),
        }
    }
}

/// Complete runner transition event used only by PostgreSQL integration tests.
#[cfg(feature = "postgres-integration")]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct RunnerStateTransitionOutboxTestEvent {
    session: SessionId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    sandbox: RunnerSandboxProfile,
    working_directory: Option<RunnerWorkingDirectory>,
    state: DispatchedRunnerState,
    source: RunnerStateTransitionOutboxTestSource,
}

#[cfg(feature = "postgres-integration")]
impl RunnerStateTransitionOutboxTestEvent {
    /// Constructs one complete test-only runner transition event.
    pub const fn new(
        session: SessionId,
        runner: RunnerId,
        placement_revision: RunnerGeneration,
        sandbox: RunnerSandboxProfile,
        working_directory: Option<RunnerWorkingDirectory>,
        state: DispatchedRunnerState,
        source: RunnerStateTransitionOutboxTestSource,
    ) -> Self {
        Self {
            session,
            runner,
            placement_revision,
            sandbox,
            working_directory,
            state,
            source,
        }
    }

    fn into_event(self) -> RunnerStateOutboxEvent {
        RunnerStateOutboxEvent {
            session: self.session,
            runner: self.runner,
            placement_revision: self.placement_revision,
            sandbox: self.sandbox,
            working_directory: self.working_directory,
            state: self.state,
            source: self.source.into_source(),
        }
    }
}

pub(crate) enum OutboxEvent {
    SessionCreated {
        session: SessionId,
    },
    SessionModelSettingsChanged {
        session: SessionId,
        installed_defaults_version: signalbox_domain::SessionConfigurationDefaultsVersion,
    },
    TurnModelSettingsResolved {
        session: SessionId,
        accepted_input: AcceptedInputId,
    },
    InputAccepted {
        session: SessionId,
        accepted_input: AcceptedInputId,
        turn: TurnId,
        acceptance_position: SessionInputPosition,
    },
    GoalTurnRetired {
        session: SessionId,
        turn: TurnId,
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
    ToolBatchTransition {
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        state: ToolBatchOutboxState,
    },
    ToolApprovalDecided {
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
    },
    ContextCompacted {
        session: SessionId,
        compaction: ContextCompactionId,
        call: ModelCallId,
        through_position: u64,
        summary_entry: SemanticTranscriptEntryId,
        result_frontier: ContextFrontierId,
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
    TurnToolReconciliationRequired {
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        terminal_frontier: ContextFrontierId,
    },
    #[allow(
        dead_code,
        reason = "runner transition producers land in the child orchestration transactions"
    )]
    RunnerStateTransition(RunnerStateOutboxEvent),
}

pub(crate) enum ModelCallOutboxState {
    Prepared,
    InFlight,
    CancellationRequested,
    Terminal(ModelCallDisposition),
}

pub(crate) enum ToolBatchOutboxState {
    Proposed(ContextFrontierId),
    ResultsProjected(ContextFrontierId),
    RecoveryRequired(ToolAttemptId),
}

/// Acquires the global append allocator at an explicit transaction boundary.
///
/// Appending an event takes this row through the header trigger. Model-call
/// transactions serialize on their ordering guard before either shared lock
/// class, finish ordinary credential locking first, and call this boundary
/// immediately before their outbox-bearing writes. Counted activation carries
/// the same guard while its atomic activation event necessarily allocates
/// before credential selection.
pub(crate) async fn lock_sequence_allocator(
    connection: &mut PgConnection,
) -> Result<(), sqlx::Error> {
    let _: bool = sqlx::query_scalar(lock_inventory::OUTBOX_SEQUENCE_ALLOCATOR)
        .fetch_one(connection)
        .await?;
    Ok(())
}

pub(crate) async fn append(
    connection: &mut PgConnection,
    event: OutboxEvent,
) -> Result<(), sqlx::Error> {
    match event {
        OutboxEvent::SessionCreated { session } => {
            append_session_created(connection, session).await
        }
        OutboxEvent::SessionModelSettingsChanged {
            session,
            installed_defaults_version,
        } => {
            append_session_model_settings_changed(connection, session, installed_defaults_version)
                .await
        }
        OutboxEvent::TurnModelSettingsResolved {
            session,
            accepted_input,
        } => append_turn_model_settings_resolved(connection, session, accepted_input).await,
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
        OutboxEvent::GoalTurnRetired { session, turn } => {
            append_goal_turn_retired(connection, session, turn).await
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
        OutboxEvent::ToolBatchTransition {
            session,
            turn,
            producing_call,
            state,
        } => append_tool_batch_transition(connection, session, turn, producing_call, state).await,
        OutboxEvent::ToolApprovalDecided {
            session,
            turn,
            request,
        } => append_tool_approval_decided(connection, session, turn, request).await,
        OutboxEvent::ContextCompacted {
            session,
            compaction,
            call,
            through_position,
            summary_entry,
            result_frontier,
        } => {
            append_context_compacted(
                connection,
                session,
                compaction,
                call,
                through_position,
                summary_entry,
                result_frontier,
            )
            .await
        }
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
        OutboxEvent::TurnToolReconciliationRequired {
            session,
            turn,
            attempt,
            terminal_frontier,
        } => {
            append_turn_tool_reconciliation_required(
                connection,
                session,
                turn,
                attempt,
                terminal_frontier,
            )
            .await
        }
        OutboxEvent::RunnerStateTransition(event) => {
            append_runner_state_transition(connection, event).await
        }
    }
}

#[cfg(feature = "postgres-integration")]
#[doc(hidden)]
pub async fn append_runner_state_transition_for_test(
    pool: &PgPool,
    event: RunnerStateTransitionOutboxTestEvent,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    append(
        transaction.as_mut(),
        OutboxEvent::RunnerStateTransition(event.into_event()),
    )
    .await?;
    transaction.commit().await
}

async fn append_runner_state_transition(
    connection: &mut PgConnection,
    event: RunnerStateOutboxEvent,
) -> Result<(), sqlx::Error> {
    let RunnerStateOutboxEvent {
        session,
        runner,
        placement_revision,
        sandbox,
        working_directory,
        state,
        source,
    } = event;
    let sandbox = runner_sandbox_to_str(sandbox);
    let state = dispatched_runner_state_to_str(state);
    let (connection_enrollment, connection_epoch, connection_event_ordinal) =
        match source.connection {
            Some(connection) => (
                Some(connection.enrollment.into_uuid()),
                Some(Decimal::from(connection.epoch)),
                Some(Decimal::from(connection.event_ordinal)),
            ),
            None => (None, None, None),
        };
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO runner_state_transition_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             runner_id, placement_revision, sandbox_profile,
             working_directory, state_kind, placement_event_ordinal,
             connection_enrollment_id, connection_epoch,
             connection_event_ordinal)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7, $8, $9, $10, $11, $12
           FROM header",
    )
    .bind(RUNNER_STATE_TRANSITION)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(sandbox)
    .bind(
        working_directory
            .as_ref()
            .map(RunnerWorkingDirectory::as_str),
    )
    .bind(state)
    .bind(Decimal::from(source.placement_event_ordinal))
    .bind(connection_enrollment)
    .bind(connection_epoch)
    .bind(connection_event_ordinal)
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_tool_approval_decided(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5
           FROM header",
    )
    .bind(TOOL_APPROVAL_DECIDED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(request.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_context_compacted(
    connection: &mut PgConnection,
    session: SessionId,
    compaction: ContextCompactionId,
    call: ModelCallId,
    through_position: u64,
    summary_entry: SemanticTranscriptEntryId,
    result_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO context_compacted_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             context_compaction_id, model_call_id, through_position,
             summary_entry_id, result_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7, $8
           FROM header",
    )
    .bind(CONTEXT_COMPACTED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(compaction.into_uuid())
    .bind(call.into_uuid())
    .bind(Decimal::from(through_position))
    .bind(summary_entry.into_uuid())
    .bind(result_frontier.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_tool_batch_transition(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    state: ToolBatchOutboxState,
) -> Result<(), sqlx::Error> {
    let (transition, frontier, attempt) = match state {
        ToolBatchOutboxState::Proposed(frontier) => ("proposed", Some(frontier), None),
        ToolBatchOutboxState::ResultsProjected(frontier) => {
            ("results_projected", Some(frontier), None)
        }
        ToolBatchOutboxState::RecoveryRequired(attempt) => {
            ("recovery_required", None, Some(attempt))
        }
    };
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_batch_transition_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, producing_model_call_id, transition_kind, frontier_id,
             tool_attempt_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6, $7, $8
           FROM header",
    )
    .bind(TOOL_BATCH_TRANSITION)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(producing_call.into_uuid())
    .bind(transition)
    .bind(frontier.map(ContextFrontierId::into_uuid))
    .bind(attempt.map(ToolAttemptId::into_uuid))
    .execute(connection)
    .await?;
    Ok(())
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

async fn append_session_model_settings_changed(
    connection: &mut PgConnection,
    session: SessionId,
    installed_defaults_version: signalbox_domain::SessionConfigurationDefaultsVersion,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO session_model_settings_changed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             installed_defaults_version)
         SELECT event_sequence, event_kind, storage_version, session_id, $4
           FROM header",
    )
    .bind(SESSION_MODEL_SETTINGS_CHANGED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(defaults_version_to_numeric(installed_defaults_version))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn append_turn_model_settings_resolved(
    connection: &mut PgConnection,
    session: SessionId,
    accepted_input: AcceptedInputId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO turn_model_settings_resolved_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             accepted_input_id)
         SELECT event_sequence, event_kind, storage_version, session_id, $4
           FROM header",
    )
    .bind(TURN_MODEL_SETTINGS_RESOLVED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(accepted_input_id_to_uuid(accepted_input))
    .execute(&mut *connection)
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

async fn append_goal_turn_retired(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ($1, $2, $3)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO goal_turn_retired_outbox_event
            (event_sequence, event_kind, storage_version, session_id, turn_id)
         SELECT event_sequence, event_kind, storage_version, session_id, $4
           FROM header",
    )
    .bind(GOAL_TURN_RETIRED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
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

async fn append_turn_tool_reconciliation_required(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
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
             turn_id, tool_attempt_id, terminal_frontier_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $4, $5, $6
           FROM header",
    )
    .bind(TURN_RECONCILIATION_REQUIRED)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(attempt.into_uuid())
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
