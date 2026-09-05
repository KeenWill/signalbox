//! Atomic delegated-session await and peer-message persistence.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::DelegationMessageDeliveryProjection;
use signalbox_domain::{
    BoundChildAction, ChildRelationshipPolicy, DelegatedSpawnRequest, DelegationAwaitRequest,
    DelegationContent, DelegationEvent, DelegationEventOrdinal, DelegationMessage,
    DelegationMessageDirection, DelegationMessageEndpoints, DelegationMessageId,
    DelegationMessageRequest, DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationProvenance, DelegationProvenanceReconstitutionInput, DelegationRequestFailure,
    DelegationTransitionFailure, DelegationWait, DelegationWaitMode, DurableCommandId,
    GoalGeneration, ReconstitutedToolAttempt, SessionDelegation,
    SessionDelegationReconstitutionFailure, SessionDelegationReconstitutionInput, SessionId,
    ToolAttemptEnd, ToolAttemptObservation, ToolDispatchAuthority, ToolEffectClass,
    ToolExecutionError, ToolExecutionErrorKind, ToolRequestId, ToolResultContent, ToolResultText,
    TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    lock_inventory::{
        DELEGATION_DELIVERY_SESSION, DELEGATION_FIND_RELATION_FOR_MESSAGE,
        DELEGATION_FIND_RELATION_FOR_WAIT, DELEGATION_LOAD_RELATION, ordered_session_pair,
    },
    mapping::{
        DelegationPolicyStorageKind, DelegationRejectionStorageKind, DelegationUpdateStorageKind,
        DelegationWakeStorageKind, bound_child_action_from_str,
        delegation_message_direction_from_str, delegation_message_direction_to_str,
        delegation_outcome_kind_from_str, delegation_outcome_reason_from_str,
        delegation_policy_kind_from_str, delegation_rejection_kind_from_str,
        delegation_rejection_kind_to_str, delegation_transition_failure_from_str,
        delegation_transition_failure_to_str, delegation_update_kind_to_str,
        delegation_wait_mode_from_str, delegation_wait_mode_to_str, delegation_wake_subject_to_str,
        durable_command_id_from_uuid, session_id_from_uuid, session_id_to_uuid,
        tool_request_id_from_uuid, tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    tool_loop::{
        ToolLoopRepositoryError, decode_attempt, load_active_batch_from_connection,
        load_attempts_by_id, load_optional_foreground_delegation_outcome, load_request_by_id,
        load_requests_by_id, lock_tool_session, persist_ended_attempt,
    },
};

const STORAGE_VERSION: i16 = 1;

/// One successful await registration, equal replay included.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedDelegationWait {
    wait: DelegationWait,
}

impl RecordedDelegationWait {
    pub const fn wait(self) -> DelegationWait {
        self.wait
    }
}

/// One successful peer-message receipt, equal replay included.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedDelegationMessage {
    tool_request: ToolRequestId,
    message: DelegationMessageId,
    direction: DelegationMessageDirection,
    ordinal: DelegationEventOrdinal,
    delivery_sequence: NonZeroU64,
}

/// One exact foreground delivery selected from durable relationship state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedDelegationDelivery {
    outcome: DelegationOutcome,
}

impl RecordedDelegationDelivery {
    pub const fn outcome(&self) -> &DelegationOutcome {
        &self.outcome
    }
}

impl RecordedDelegationMessage {
    pub const fn tool_request(&self) -> ToolRequestId {
        self.tool_request
    }

    pub const fn message(&self) -> DelegationMessageId {
        self.message
    }

    pub const fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }

    pub const fn ordinal(&self) -> DelegationEventOrdinal {
        self.ordinal
    }

    pub const fn delivery_sequence(&self) -> NonZeroU64 {
        self.delivery_sequence
    }
}

impl DelegationMessageDeliveryProjection for RecordedDelegationMessage {
    fn tool_request(&self) -> ToolRequestId {
        self.tool_request
    }

    fn message(&self) -> DelegationMessageId {
        self.message
    }

    fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }

    fn ordinal(&self) -> DelegationEventOrdinal {
        self.ordinal
    }

    fn delivery_sequence(&self) -> NonZeroU64 {
        self.delivery_sequence
    }
}

/// Expected rejection of a checked delegation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationOperationRejection {
    RelationshipNotFound,
    StaleDispatch {
        state: DelegationRequestExecutionState,
    },
    MessageIdentityCollision,
    DeliverySequenceExhausted,
    Transition {
        spawning_request: ToolRequestId,
        failure: DelegationTransitionFailure,
    },
}

/// Durable state explaining why a delegation request cannot execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationRequestExecutionState {
    AwaitingApproval,
    Denied,
    Approved,
    Prepared,
    Closed,
    AttemptEnded,
}

/// Typed precondition or durable-state rejection for one process request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessDelegationRequestRejection {
    SessionNotFound,
    ToolRequestNotFound,
    ToolRequestNotInSession,
    RequestNotInTurn,
    AwaitConflict,
    MessageConflict,
    MessageIdentityCollision { message: DelegationMessageId },
    Operation(DelegationOperationRejection),
}

/// Process adapter outcome before projection into the versioned wire surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessDelegationOutcome<T> {
    Applied(T),
    InvalidRequest,
    Rejected(ProcessDelegationRequestRejection),
}

#[derive(Clone, Copy)]
enum DispatchSource<'a> {
    Issued(&'a ToolDispatchAuthority),
    Reconstitute,
}

impl DispatchSource<'_> {
    fn matches_request(self, request: &signalbox_domain::ToolRequest) -> bool {
        match self {
            Self::Issued(dispatch) => dispatch.request() == request,
            Self::Reconstitute => true,
        }
    }

    const fn definitive_rejection_persistence(self) -> DefinitiveRejectionPersistence {
        match self {
            Self::Issued(_) => DefinitiveRejectionPersistence::ReturnOnly,
            Self::Reconstitute => DefinitiveRejectionPersistence::Persist,
        }
    }
}

#[derive(Clone, Copy)]
enum DefinitiveRejectionPersistence {
    ReturnOnly,
    Persist,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordDelegationWaitOutcome {
    Recorded(RecordedDelegationWait),
    Rejected(DelegationOperationRejection),
    DurablyRejected(DelegationOperationRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordDelegationMessageOutcome {
    Recorded(Box<RecordedDelegationMessage>),
    Rejected(DelegationOperationRejection),
    DurablyRejected(DelegationOperationRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordedDelegationMessageRejection {
    rejection: DelegationOperationRejection,
    message: DelegationMessageId,
    durable: bool,
}

enum RecordDelegationMessageWithSourceOutcome {
    Recorded(Box<RecordedDelegationMessage>),
    Rejected(RecordedDelegationMessageRejection),
}

/// Stored facts failed checked reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDelegationCorruption {
    Missing(&'static str),
    InvalidColumn(&'static str),
    Inconsistent(&'static str),
    Unsupported { field: &'static str, value: String },
    Reconstitution(SessionDelegationReconstitutionFailure),
}

/// Database or fail-closed delegated-session persistence failure.
#[derive(Debug)]
pub enum SessionDelegationRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    ToolLoop(ToolLoopRepositoryError),
    InvalidTransition(&'static str),
    Corruption(SessionDelegationCorruption),
}

impl fmt::Display for SessionDelegationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "delegation database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(formatter, "delegation commit is ambiguous: {error}")
            }
            Self::ToolLoop(error) => write!(formatter, "delegation tool-loop failure: {error}"),
            Self::InvalidTransition(reason) => {
                write!(formatter, "delegation transition is invalid: {reason}")
            }
            Self::Corruption(reason) => {
                write!(formatter, "delegation storage is corrupt: {reason:?}")
            }
        }
    }
}

impl Error for SessionDelegationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::ToolLoop(error) => Some(error),
            Self::InvalidTransition(_) | Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionDelegationRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ToolLoopRepositoryError> for SessionDelegationRepositoryError {
    fn from(error: ToolLoopRepositoryError) -> Self {
        Self::ToolLoop(error)
    }
}

impl From<SessionDelegationCorruption> for SessionDelegationRepositoryError {
    fn from(value: SessionDelegationCorruption) -> Self {
        Self::Corruption(value)
    }
}

/// PostgreSQL adapter for atomic delegated-session tool effects.
#[derive(Clone, Debug)]
pub struct SessionDelegationRepository {
    pool: PgPool,
}

impl SessionDelegationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records one foreground/background await and its exact tool transition.
    pub async fn record_wait(
        &self,
        request: DelegationAwaitRequest,
        dispatch: &ToolDispatchAuthority,
    ) -> Result<RecordDelegationWaitOutcome, SessionDelegationRepositoryError> {
        self.record_wait_with_source(request, DispatchSource::Issued(dispatch))
            .await
    }

    async fn record_wait_with_source(
        &self,
        request: DelegationAwaitRequest,
        dispatch: DispatchSource<'_>,
    ) -> Result<RecordDelegationWaitOutcome, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            let rejection_persistence = dispatch.definitive_rejection_persistence();
            lock_delivery_session(&mut transaction, request.request().session()).await?;
            lock_tool_session(&mut transaction, request.request().session()).await?;
            if !dispatch.matches_request(request.request()) {
                return Ok(RecordDelegationWaitOutcome::Rejected(
                    DelegationOperationRejection::StaleDispatch {
                        state: DelegationRequestExecutionState::AttemptEnded,
                    },
                ));
            }
            if let Some(stored) = load_wait_replay(&mut transaction, request.request().id()).await?
            {
                let wait = DelegationWait::reconstitute_stored(
                    &request,
                    stored.spawning_request,
                    stored.parent,
                    stored.child,
                    stored.mode,
                )
                .ok_or(SessionDelegationCorruption::Inconsistent(
                    "stored wait purpose",
                ))?;
                if stored.parent_turn != request.request().turn() {
                    return Err(SessionDelegationCorruption::Inconsistent("stored wait row").into());
                }
                validate_wait_replay_update(&mut transaction, wait).await?;
                validate_wait_replay_attempt(&mut transaction, request.request(), dispatch, wait)
                    .await?;
                validate_wait_replay_delivery(&mut transaction, wait).await?;
                return Ok(RecordDelegationWaitOutcome::Recorded(
                    RecordedDelegationWait { wait },
                ));
            }
            if let Some(rejection) =
                load_wait_rejection(&mut transaction, request.request().id()).await?
            {
                return Ok(RecordDelegationWaitOutcome::DurablyRejected(rejection));
            }

            let dispatch =
                match resolve_dispatch(&mut transaction, request.request(), dispatch).await? {
                    ResolvedDelegationDispatch::Executable(dispatch) => *dispatch,
                    ResolvedDelegationDispatch::NonExecutable(state) => {
                        return Ok(RecordDelegationWaitOutcome::Rejected(
                            DelegationOperationRejection::StaleDispatch { state },
                        ));
                    }
                };
            let Some(spawning_request) = find_relation_for_wait(&mut transaction, &request).await?
            else {
                return reject_wait_operation(
                    &mut transaction,
                    &dispatch,
                    DelegationOperationRejection::RelationshipNotFound,
                    rejection_persistence,
                )
                .await;
            };
            let relation = load_relation(&mut transaction, spawning_request).await?;
            let wait = match relation.register_wait(&request, &dispatch) {
                Ok(wait) => wait,
                Err(error) => {
                    return reject_wait_operation(
                        &mut transaction,
                        &dispatch,
                        DelegationOperationRejection::Transition {
                            spawning_request: error.spawning_request(),
                            failure: error.failure(),
                        },
                        rejection_persistence,
                    )
                    .await;
                }
            };
            if dispatch.attempt().effect_class() != ToolEffectClass::EffectFree {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "await_session requires an effect-free attempt",
                ));
            }

            let result_exists =
                child_result_exists(&mut transaction, wait.spawning_request()).await?;
            let reserved_background_results =
                pending_background_result_reservations(&mut transaction, wait.parent()).await?;
            let background_delivery_sequence = match (result_exists, wait.mode()) {
                (true, DelegationWaitMode::Background) => {
                    let Some(sequence) = next_delivery_sequence_preserving(
                        &mut transaction,
                        wait.parent(),
                        reserved_background_results,
                    )
                    .await?
                    else {
                        return reject_wait_operation(
                            &mut transaction,
                            &dispatch,
                            DelegationOperationRejection::DeliverySequenceExhausted,
                            rejection_persistence,
                        )
                        .await;
                    };
                    Some(sequence)
                }
                (false, DelegationWaitMode::Background) => {
                    if !delivery_reservation_available(
                        &mut transaction,
                        wait.parent(),
                        reserved_background_results.saturating_add(1),
                    )
                    .await?
                    {
                        return reject_wait_operation(
                            &mut transaction,
                            &dispatch,
                            DelegationOperationRejection::DeliverySequenceExhausted,
                            rejection_persistence,
                        )
                        .await;
                    }
                    None
                }
                (_, DelegationWaitMode::Foreground) => None,
            };
            insert_wait(&mut transaction, wait, request.request().turn()).await?;
            append_wait_update(&mut transaction, wait).await?;
            if result_exists {
                insert_result_delivery(&mut transaction, wait, background_delivery_sequence)
                    .await?;
            }

            match wait.mode() {
                DelegationWaitMode::Background => {
                    let ended = complete_attempt(&dispatch, wait_receipt(wait)?)?;
                    persist_ended_attempt(&mut transaction, &ended).await?;
                }
                DelegationWaitMode::Foreground => {
                    let subject = wait.foreground_subject().ok_or(
                        SessionDelegationRepositoryError::InvalidTransition(
                            "foreground wait subject is absent",
                        ),
                    )?;
                    let ended = dispatch
                        .attempt()
                        .clone()
                        .end_foreground_child_wait(subject)
                        .map_err(|_| {
                            SessionDelegationRepositoryError::InvalidTransition(
                                "foreground wait cannot end the current attempt",
                            )
                        })?;
                    persist_ended_attempt(&mut transaction, &ended).await?;
                    park_foreground_turn(&mut transaction, &ended, wait).await?;
                    if result_exists {
                        append_result_wake(
                            &mut transaction,
                            wait.parent(),
                            wait.spawning_request(),
                            Some(wait.awaiting_request()),
                        )
                        .await?;
                    }
                }
            }
            Ok(RecordDelegationWaitOutcome::Recorded(
                RecordedDelegationWait { wait },
            ))
        }
        .await;
        finish(transaction, result).await
    }

    /// Appends one bidirectional message, delivery, receipt, update, and wake.
    pub async fn record_message(
        &self,
        request: DelegationMessageRequest,
        message: DelegationMessageId,
        dispatch: &ToolDispatchAuthority,
    ) -> Result<RecordDelegationMessageOutcome, SessionDelegationRepositoryError> {
        Ok(
            match self
                .record_message_with_source(request, message, DispatchSource::Issued(dispatch))
                .await?
            {
                RecordDelegationMessageWithSourceOutcome::Recorded(recorded) => {
                    RecordDelegationMessageOutcome::Recorded(recorded)
                }
                RecordDelegationMessageWithSourceOutcome::Rejected(recorded) => {
                    if recorded.durable {
                        RecordDelegationMessageOutcome::DurablyRejected(recorded.rejection)
                    } else {
                        RecordDelegationMessageOutcome::Rejected(recorded.rejection)
                    }
                }
            },
        )
    }

    async fn record_message_with_source(
        &self,
        request: DelegationMessageRequest,
        message: DelegationMessageId,
        dispatch: DispatchSource<'_>,
    ) -> Result<RecordDelegationMessageWithSourceOutcome, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            let rejection_persistence = dispatch.definitive_rejection_persistence();
            let peer_exists = session_exists(&mut transaction, request.peer()).await?;
            if peer_exists {
                lock_message_sessions(
                    &mut transaction,
                    request.request().session(),
                    request.peer(),
                )
                .await?;
            } else {
                lock_delivery_session(&mut transaction, request.request().session()).await?;
                lock_tool_session(&mut transaction, request.request().session()).await?;
            }
            if !dispatch.matches_request(request.request()) {
                return Ok(RecordDelegationMessageWithSourceOutcome::Rejected(
                    RecordedDelegationMessageRejection {
                        rejection: DelegationOperationRejection::StaleDispatch {
                            state: DelegationRequestExecutionState::AttemptEnded,
                        },
                        message,
                        durable: false,
                    },
                ));
            }
            if let Some(receipt) = load_message_replay(&mut transaction, &request).await? {
                validate_message_replay_attempt(
                    &mut transaction,
                    request.request(),
                    dispatch,
                    &receipt,
                )
                .await?;
                return Ok(RecordDelegationMessageWithSourceOutcome::Recorded(
                    Box::new(receipt),
                ));
            }
            if let Some(recorded) =
                load_message_rejection(&mut transaction, request.request().id()).await?
            {
                return Ok(RecordDelegationMessageWithSourceOutcome::Rejected(recorded));
            }
            let dispatch =
                match resolve_dispatch(&mut transaction, request.request(), dispatch).await? {
                    ResolvedDelegationDispatch::Executable(dispatch) => *dispatch,
                    ResolvedDelegationDispatch::NonExecutable(state) => {
                        return Ok(RecordDelegationMessageWithSourceOutcome::Rejected(
                            RecordedDelegationMessageRejection {
                                rejection: DelegationOperationRejection::StaleDispatch { state },
                                message,
                                durable: false,
                            },
                        ));
                    }
                };
            if dispatch.attempt().effect_class() != ToolEffectClass::ExternalEffect {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "send_session_message requires an external-effect attempt",
                ));
            }
            if !peer_exists {
                return reject_message_operation(
                    &mut transaction,
                    &dispatch,
                    message,
                    DelegationOperationRejection::RelationshipNotFound,
                    rejection_persistence,
                )
                .await;
            }
            let Some(spawning_request) =
                find_relation_for_message(&mut transaction, &request).await?
            else {
                return reject_message_operation(
                    &mut transaction,
                    &dispatch,
                    message,
                    DelegationOperationRejection::RelationshipNotFound,
                    rejection_persistence,
                )
                .await;
            };
            let tool_request = request.request().id();
            let (event, endpoints) = match prepare_fresh_message_event(
                &mut transaction,
                spawning_request,
                &request,
                message,
                &dispatch,
            )
            .await?
            {
                Ok(recorded) => recorded,
                Err(failure) => {
                    return reject_message_operation(
                        &mut transaction,
                        &dispatch,
                        message,
                        DelegationOperationRejection::Transition {
                            spawning_request,
                            failure,
                        },
                        rejection_persistence,
                    )
                    .await;
                }
            };
            let stored_message =
                event
                    .message()
                    .ok_or(SessionDelegationCorruption::Inconsistent(
                        "message transition event",
                    ))?;
            let recipient = message_recipient(stored_message.direction(), endpoints);
            let reserved_background_results =
                pending_background_result_reservations(&mut transaction, recipient).await?;
            let Some(delivery_sequence) = next_delivery_sequence_preserving(
                &mut transaction,
                recipient,
                reserved_background_results,
            )
            .await?
            else {
                return reject_message_operation(
                    &mut transaction,
                    &dispatch,
                    message,
                    DelegationOperationRejection::DeliverySequenceExhausted,
                    rejection_persistence,
                )
                .await;
            };
            if !insert_message_state(
                &mut transaction,
                spawning_request,
                event.ordinal(),
                stored_message,
                recipient,
                delivery_sequence,
            )
            .await?
            {
                return reject_message_operation(
                    &mut transaction,
                    &dispatch,
                    message,
                    DelegationOperationRejection::MessageIdentityCollision,
                    rejection_persistence,
                )
                .await;
            }
            append_message_update(
                &mut transaction,
                spawning_request,
                event.ordinal(),
                stored_message,
                recipient,
            )
            .await?;
            append_message_wake(
                &mut transaction,
                recipient,
                spawning_request,
                stored_message.id(),
            )
            .await?;
            let message = stored_message.id();
            let direction = stored_message.direction();
            let ordinal = event.ordinal();
            let receipt = RecordedDelegationMessage {
                tool_request,
                message,
                direction,
                ordinal,
                delivery_sequence,
            };
            let ended = complete_attempt(
                &dispatch,
                message_receipt(dispatch.request().id(), &receipt)?,
            )?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            Ok(RecordDelegationMessageWithSourceOutcome::Recorded(
                Box::new(receipt),
            ))
        }
        .await;
        finish(transaction, result).await
    }

    /// Executes one exact process-protocol await from its stored tool request.
    pub async fn record_process_wait(
        &self,
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Result<
        ProcessDelegationOutcome<(DelegationAwaitRequest, RecordedDelegationWait)>,
        SessionDelegationRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        if !session_exists(&mut connection, session).await? {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::SessionNotFound,
            ));
        }
        let Some(stored) = load_request_by_id(&mut connection, request).await? else {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotFound,
            ));
        };
        if stored.session() != session {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotInSession,
            ));
        }
        if stored.turn() != turn {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::RequestNotInTurn,
            ));
        }
        let logical = match DelegationAwaitRequest::parse(stored, child, mode) {
            Ok(logical) => logical,
            Err(_) if load_wait_replay(&mut connection, request).await?.is_some() => {
                return Ok(ProcessDelegationOutcome::Rejected(
                    ProcessDelegationRequestRejection::AwaitConflict,
                ));
            }
            Err(_) => return Ok(ProcessDelegationOutcome::InvalidRequest),
        };
        drop(connection);
        let outcome = self
            .record_wait_with_source(logical.clone(), DispatchSource::Reconstitute)
            .await?;
        Ok(match outcome {
            RecordDelegationWaitOutcome::Recorded(recorded) => {
                ProcessDelegationOutcome::Applied((logical, recorded))
            }
            RecordDelegationWaitOutcome::Rejected(rejection) => ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::Operation(rejection),
            ),
            RecordDelegationWaitOutcome::DurablyRejected(rejection) => {
                ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
                    rejection,
                ))
            }
        })
    }

    /// Executes one exact process-protocol message from its stored tool request.
    pub async fn record_process_message(
        &self,
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
        peer: SessionId,
        content: String,
        message: DelegationMessageId,
    ) -> Result<
        ProcessDelegationOutcome<(DelegationMessageRequest, Box<RecordedDelegationMessage>)>,
        SessionDelegationRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        if !session_exists(&mut connection, session).await? {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::SessionNotFound,
            ));
        }
        let Some(stored) = load_request_by_id(&mut connection, request).await? else {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotFound,
            ));
        };
        if stored.session() != session {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotInSession,
            ));
        }
        if stored.turn() != turn {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::RequestNotInTurn,
            ));
        }
        let logical = match DelegationMessageRequest::parse(stored, peer, content) {
            Ok(logical) => logical,
            Err(error) => match error.failure() {
                DelegationRequestFailure::InvalidContent(_) => {
                    return Ok(ProcessDelegationOutcome::InvalidRequest);
                }
                DelegationRequestFailure::InvalidToolRequestPurpose => {
                    if message_outcome_exists(&mut connection, request).await? {
                        return Ok(ProcessDelegationOutcome::Rejected(
                            ProcessDelegationRequestRejection::MessageConflict,
                        ));
                    }
                    return Ok(ProcessDelegationOutcome::InvalidRequest);
                }
            },
        };
        drop(connection);
        let outcome = self
            .record_message_with_source(logical.clone(), message, DispatchSource::Reconstitute)
            .await?;
        Ok(match outcome {
            RecordDelegationMessageWithSourceOutcome::Recorded(recorded) => {
                ProcessDelegationOutcome::Applied((logical, recorded))
            }
            RecordDelegationMessageWithSourceOutcome::Rejected(recorded) => {
                match recorded.rejection {
                    DelegationOperationRejection::MessageIdentityCollision => {
                        ProcessDelegationOutcome::Rejected(
                            ProcessDelegationRequestRejection::MessageIdentityCollision {
                                message: recorded.message,
                            },
                        )
                    }
                    rejection => ProcessDelegationOutcome::Rejected(
                        ProcessDelegationRequestRejection::Operation(rejection),
                    ),
                }
            }
        })
    }

    /// Reads the immutable result selected by one exact foreground delivery.
    pub async fn load_foreground_delivery(
        &self,
        wait: DelegationWait,
    ) -> Result<Option<RecordedDelegationDelivery>, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let outcome = load_optional_foreground_delegation_outcome(
            &mut transaction,
            wait.parent(),
            wait.awaiting_request(),
            wait.spawning_request(),
            wait.child(),
        )
        .await?;
        let Some(outcome) = outcome else {
            transaction.rollback().await?;
            return Ok(None);
        };
        transaction.rollback().await?;
        Ok(Some(RecordedDelegationDelivery { outcome }))
    }
}

async fn finish<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, SessionDelegationRepositoryError>,
) -> Result<T, SessionDelegationRepositoryError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|error| {
                if commit_failure_is_ambiguous(&error) {
                    SessionDelegationRepositoryError::CommitAmbiguous(error)
                } else {
                    SessionDelegationRepositoryError::Database(error)
                }
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn resolve_dispatch(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    source: DispatchSource<'_>,
) -> Result<ResolvedDelegationDispatch, SessionDelegationRepositoryError> {
    let batch =
        load_active_batch_from_connection(connection, request.session(), request.turn()).await?;
    let executable = match (&source, batch.as_ref()) {
        (DispatchSource::Issued(dispatch), Some(batch)) => batch
            .resume_in_flight_dispatch(dispatch.attempt().attempt())
            .ok()
            .filter(|stored| stored == *dispatch),
        (DispatchSource::Reconstitute, Some(batch)) => {
            batch
                .attempt(request.id())
                .and_then(|attempt| match attempt {
                    ReconstitutedToolAttempt::Current(attempt) => batch
                        .resume_in_flight_dispatch(attempt.attempt())
                        .ok()
                        .filter(|dispatch| dispatch.request() == request),
                    ReconstitutedToolAttempt::Ended(_) => None,
                })
        }
        (DispatchSource::Issued(_), None) | (DispatchSource::Reconstitute, None) => None,
    };
    if let Some(dispatch) = executable {
        return Ok(ResolvedDelegationDispatch::Executable(Box::new(dispatch)));
    }
    Ok(ResolvedDelegationDispatch::NonExecutable(
        delegation_request_execution_state(connection, request, batch.as_ref()).await?,
    ))
}

enum ResolvedDelegationDispatch {
    Executable(Box<ToolDispatchAuthority>),
    NonExecutable(DelegationRequestExecutionState),
}

async fn delegation_request_execution_state(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    batch: Option<&signalbox_domain::ToolBatch>,
) -> Result<DelegationRequestExecutionState, SessionDelegationRepositoryError> {
    if let Some(batch) = batch {
        let request_is_in_batch = batch
            .requests()
            .iter()
            .any(|candidate| candidate.id() == request.id());
        if request_is_in_batch
            && (batch
                .awaiting_approval()
                .is_some_and(|waiting| waiting.request() == request.id())
                || (batch.approval(request.id()).is_none()
                    && batch.attempt(request.id()).is_none()))
        {
            return Ok(DelegationRequestExecutionState::AwaitingApproval);
        }
        if batch
            .approval(request.id())
            .is_some_and(|approval| !approval.is_approved())
        {
            return Ok(DelegationRequestExecutionState::Denied);
        }
        if batch
            .approval(request.id())
            .is_some_and(signalbox_domain::ToolApprovalResolution::is_approved)
            && batch.attempt(request.id()).is_none()
        {
            return Ok(DelegationRequestExecutionState::Approved);
        }
        if let Some(attempt) = batch.attempt(request.id()) {
            return Ok(match attempt {
                ReconstitutedToolAttempt::Current(_) => DelegationRequestExecutionState::Prepared,
                ReconstitutedToolAttempt::Ended(_) => DelegationRequestExecutionState::AttemptEnded,
            });
        }
    }
    let stored: StoredDelegationRequestExecutionState = sqlx::query_as(
        "SELECT
            EXISTS (
                SELECT 1 FROM semantic_transcript_entry
                 WHERE tool_result_request_id = $1 AND payload_kind = 'tool_denied'
            ) AS denied,
            EXISTS (
                SELECT 1 FROM semantic_transcript_entry
                 WHERE tool_result_request_id = $1
                   AND payload_kind = 'tool_closed_by_turn_end'
            ) AS closed,
            EXISTS (SELECT 1 FROM tool_attempt WHERE request_id = $1) AS attempted",
    )
    .bind(tool_request_id_to_uuid(request.id()))
    .fetch_one(&mut *connection)
    .await?;
    Ok(if stored.denied {
        DelegationRequestExecutionState::Denied
    } else if stored.closed {
        DelegationRequestExecutionState::Closed
    } else if stored.attempted {
        DelegationRequestExecutionState::AttemptEnded
    } else {
        DelegationRequestExecutionState::Closed
    })
}

#[derive(sqlx::FromRow)]
struct StoredDelegationRequestExecutionState {
    denied: bool,
    closed: bool,
    attempted: bool,
}

async fn session_exists(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, SessionDelegationRepositoryError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
        .bind(session_id_to_uuid(session))
        .fetch_one(connection)
        .await
        .map_err(Into::into)
}

async fn lock_message_sessions(
    connection: &mut PgConnection,
    sender: SessionId,
    peer: SessionId,
) -> Result<(), SessionDelegationRepositoryError> {
    let (first, second) = ordered_session_pair(sender, peer);
    lock_delivery_session(connection, first).await?;
    if second != first {
        lock_delivery_session(connection, second).await?;
    }
    lock_tool_session(connection, first).await?;
    if second != first {
        lock_tool_session(connection, second).await?;
    }
    Ok(())
}

fn complete_attempt(
    dispatch: &ToolDispatchAuthority,
    result: ToolResultText,
) -> Result<signalbox_domain::EndedToolAttempt, SessionDelegationRepositoryError> {
    dispatch
        .attempt()
        .clone()
        .apply_terminal_observation(dispatch.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(result),
            },
        ))
        .map_err(|_| {
            SessionDelegationRepositoryError::InvalidTransition(
                "delegation receipt cannot end the current attempt",
            )
        })
}

async fn reject_wait_operation(
    connection: &mut PgConnection,
    dispatch: &ToolDispatchAuthority,
    rejection: DelegationOperationRejection,
    persistence: DefinitiveRejectionPersistence,
) -> Result<RecordDelegationWaitOutcome, SessionDelegationRepositoryError> {
    match persistence {
        DefinitiveRejectionPersistence::ReturnOnly => {
            Ok(RecordDelegationWaitOutcome::Rejected(rejection))
        }
        DefinitiveRejectionPersistence::Persist => {
            insert_wait_rejection(connection, dispatch.request().id(), rejection).await?;
            persist_known_failed_rejection_attempt(connection, dispatch).await?;
            Ok(RecordDelegationWaitOutcome::DurablyRejected(rejection))
        }
    }
}

async fn reject_message_operation(
    connection: &mut PgConnection,
    dispatch: &ToolDispatchAuthority,
    message: DelegationMessageId,
    rejection: DelegationOperationRejection,
    persistence: DefinitiveRejectionPersistence,
) -> Result<RecordDelegationMessageWithSourceOutcome, SessionDelegationRepositoryError> {
    let durable = match persistence {
        DefinitiveRejectionPersistence::ReturnOnly => false,
        DefinitiveRejectionPersistence::Persist => {
            insert_message_rejection(connection, dispatch.request().id(), message, rejection)
                .await?;
            persist_known_failed_rejection_attempt(connection, dispatch).await?;
            true
        }
    };
    Ok(RecordDelegationMessageWithSourceOutcome::Rejected(
        RecordedDelegationMessageRejection {
            rejection,
            message,
            durable,
        },
    ))
}

async fn persist_known_failed_rejection_attempt(
    connection: &mut PgConnection,
    dispatch: &ToolDispatchAuthority,
) -> Result<(), SessionDelegationRepositoryError> {
    let ended = dispatch
        .attempt()
        .clone()
        .apply_terminal_observation(dispatch.executor_fence().bind(
            ToolAttemptObservation::KnownFailed {
                error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
            },
        ))
        .map_err(|_| {
            SessionDelegationRepositoryError::InvalidTransition(
                "process delegation rejection cannot end the current attempt",
            )
        })?;
    persist_ended_attempt(connection, &ended).await?;
    Ok(())
}

fn wait_receipt(wait: DelegationWait) -> Result<ToolResultText, SessionDelegationRepositoryError> {
    if wait.mode() != DelegationWaitMode::Background {
        return Err(SessionDelegationRepositoryError::InvalidTransition(
            "wait receipts represent background waits only",
        ));
    }
    ToolResultText::try_new(
        serde_json::json!({
            "result": "session_await_registered",
            "tool_request_id": wait.awaiting_request().as_uuid().to_string(),
            "child_session_id": wait.child().as_uuid().to_string(),
            "mode": "background",
        })
        .to_string(),
    )
    .map_err(|_| SessionDelegationCorruption::Inconsistent("background wait receipt").into())
}

fn message_receipt(
    request: ToolRequestId,
    receipt: &RecordedDelegationMessage,
) -> Result<ToolResultText, SessionDelegationRepositoryError> {
    ToolResultText::try_new(
        serde_json::json!({
            "result": "session_message_sent",
            "tool_request_id": request.as_uuid().to_string(),
            "message_id": receipt.message().as_uuid().to_string(),
            "direction": delegation_message_direction_to_str(receipt.direction()),
            "ordinal": receipt.ordinal().get(),
            "delivery_sequence": receipt.delivery_sequence().get(),
        })
        .to_string(),
    )
    .map_err(|_| SessionDelegationCorruption::Inconsistent("message receipt").into())
}

async fn validate_wait_replay_attempt(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    dispatch: DispatchSource<'_>,
    wait: DelegationWait,
) -> Result<(), SessionDelegationRepositoryError> {
    let expected_end = match wait.mode() {
        DelegationWaitMode::Background => ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(wait_receipt(wait)?),
        },
        DelegationWaitMode::Foreground => ToolAttemptEnd::AwaitingChild {
            spawning_request: wait.spawning_request(),
            child: wait.child(),
        },
    };
    validate_replay_attempt(
        connection,
        request,
        dispatch,
        ToolEffectClass::EffectFree,
        expected_end,
        "stored wait attempt",
    )
    .await
}

async fn validate_message_replay_attempt(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    dispatch: DispatchSource<'_>,
    receipt: &RecordedDelegationMessage,
) -> Result<(), SessionDelegationRepositoryError> {
    validate_replay_attempt(
        connection,
        request,
        dispatch,
        ToolEffectClass::ExternalEffect,
        ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(message_receipt(request.id(), receipt)?),
        },
        "stored message attempt",
    )
    .await
}

async fn validate_replay_attempt(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    dispatch: DispatchSource<'_>,
    expected_effect: ToolEffectClass,
    expected_end: ToolAttemptEnd,
    corruption_label: &'static str,
) -> Result<(), SessionDelegationRepositoryError> {
    match dispatch {
        DispatchSource::Issued(dispatch) => {
            let attempt_id = dispatch.attempt().attempt();
            let mut attempts = load_attempts_by_id(connection, &[attempt_id]).await?;
            let Some(ReconstitutedToolAttempt::Ended(ended)) = attempts.remove(&attempt_id) else {
                return Err(SessionDelegationCorruption::Inconsistent(corruption_label).into());
            };
            let dispatched = dispatch.attempt();
            if ended.attempt() != dispatched.attempt()
                || ended.request() != dispatched.request()
                || ended.session() != dispatched.session()
                || ended.turn() != dispatched.turn()
                || ended.issuing_attempt() != dispatched.issuing_attempt()
                || ended.generation() != dispatched.generation()
                || dispatched.effect_class() != expected_effect
                || ended.effect_class() != dispatched.effect_class()
                || ended.end() != &expected_end
            {
                return Err(SessionDelegationCorruption::Inconsistent(corruption_label).into());
            }
            Ok(())
        }
        DispatchSource::Reconstitute => {
            validate_reconstituted_replay_attempt(
                connection,
                request,
                expected_effect,
                &expected_end,
                corruption_label,
            )
            .await
        }
    }
}

async fn validate_reconstituted_replay_attempt(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    expected_effect: ToolEffectClass,
    expected_end: &ToolAttemptEnd,
    corruption_label: &'static str,
) -> Result<(), SessionDelegationRepositoryError> {
    let rows = sqlx::query(
        "SELECT *
           FROM tool_attempt
          WHERE request_id = $1
          ORDER BY attempt_id",
    )
    .bind(tool_request_id_to_uuid(request.id()))
    .fetch_all(connection)
    .await?;
    let mut matching_terminal_count = 0_u64;
    for row in rows {
        match decode_attempt(row)? {
            ReconstitutedToolAttempt::Current(_) => {
                return Err(SessionDelegationCorruption::Inconsistent(corruption_label).into());
            }
            ReconstitutedToolAttempt::Ended(ended) => {
                if ended.request() != request.id()
                    || ended.session() != request.session()
                    || ended.turn() != request.turn()
                    || ended.effect_class() != expected_effect
                {
                    return Err(SessionDelegationCorruption::Inconsistent(corruption_label).into());
                }
                if ended.end() == expected_end {
                    matching_terminal_count += 1;
                }
            }
        }
    }
    if matching_terminal_count != 1 {
        return Err(SessionDelegationCorruption::Inconsistent(corruption_label).into());
    }
    Ok(())
}

struct StoredDelegationWaitReplay {
    spawning_request: ToolRequestId,
    parent: SessionId,
    parent_turn: signalbox_domain::TurnId,
    child: SessionId,
    mode: DelegationWaitMode,
}

async fn load_wait_replay(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<StoredDelegationWaitReplay>, SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT spawning_tool_request_id, parent_session_id, parent_turn_id,
                child_session_id, wait_mode
           FROM session_delegation_wait
          WHERE awaiting_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(connection)
    .await?;
    row.map(|row| {
        Ok(StoredDelegationWaitReplay {
            spawning_request: tool_request_id_from_uuid(required(
                &row,
                "spawning_tool_request_id",
            )?),
            parent: session_id_from_uuid(required(&row, "parent_session_id")?),
            parent_turn: turn_id_from_uuid(required(&row, "parent_turn_id")?),
            child: session_id_from_uuid(required(&row, "child_session_id")?),
            mode: decode_wait_mode(&required::<String>(&row, "wait_mode")?)?,
        })
    })
    .transpose()
}

async fn validate_wait_replay_delivery(
    connection: &mut PgConnection,
    wait: DelegationWait,
) -> Result<(), SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT
            EXISTS (
                SELECT 1 FROM session_child_result
                 WHERE spawning_tool_request_id = $2
            ) AS result_exists,
            delivery.awaiting_tool_request_id AS delivery_awaiting_request_id,
            delivery.spawning_tool_request_id AS delivery_spawning_request_id,
            delivery.parent_session_id AS delivery_parent_session_id,
            delivery.delivery_sequence,
            delivery.delivery_kind,
            pending.recipient_session_id AS pending_recipient_session_id,
            pending.delivery_sequence AS pending_delivery_sequence,
            pending.delivery_kind AS pending_delivery_kind
           FROM (VALUES (1)) AS singleton(value)
           LEFT JOIN session_child_result_delivery AS delivery
             ON delivery.awaiting_tool_request_id = $1
           LEFT JOIN session_pending_delivery AS pending
             ON pending.recipient_session_id = delivery.parent_session_id
            AND pending.delivery_sequence = delivery.delivery_sequence
            AND pending.delivery_kind = delivery.delivery_kind",
    )
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .bind(tool_request_id_to_uuid(wait.spawning_request()))
    .fetch_one(connection)
    .await?;
    let result_exists: bool = required(&row, "result_exists")?;
    let awaiting: Option<Uuid> = optional(&row, "delivery_awaiting_request_id")?;
    let spawning: Option<Uuid> = optional(&row, "delivery_spawning_request_id")?;
    let parent: Option<Uuid> = optional(&row, "delivery_parent_session_id")?;
    let sequence: Option<Decimal> = optional(&row, "delivery_sequence")?;
    let kind: Option<String> = optional(&row, "delivery_kind")?;
    let pending_recipient: Option<Uuid> = optional(&row, "pending_recipient_session_id")?;
    let pending_sequence: Option<Decimal> = optional(&row, "pending_delivery_sequence")?;
    let pending_kind: Option<String> = optional(&row, "pending_delivery_kind")?;
    let exact_header = awaiting == Some(tool_request_id_to_uuid(wait.awaiting_request()))
        && spawning == Some(tool_request_id_to_uuid(wait.spawning_request()))
        && parent == Some(session_id_to_uuid(wait.parent()));
    let valid = match (result_exists, wait.mode()) {
        (false, _) => {
            awaiting.is_none()
                && spawning.is_none()
                && parent.is_none()
                && sequence.is_none()
                && kind.is_none()
                && pending_recipient.is_none()
                && pending_sequence.is_none()
                && pending_kind.is_none()
        }
        (true, DelegationWaitMode::Foreground) => {
            exact_header
                && sequence.is_none()
                && kind.is_none()
                && pending_recipient.is_none()
                && pending_sequence.is_none()
                && pending_kind.is_none()
        }
        (true, DelegationWaitMode::Background) => {
            let positive_sequence = sequence
                .map(|value| decode_positive(value, "delivery_sequence"))
                .transpose()?;
            exact_header
                && positive_sequence.is_some()
                && kind.as_deref() == Some("background_result")
                && pending_recipient == Some(session_id_to_uuid(wait.parent()))
                && pending_sequence == sequence
                && pending_kind == kind
        }
    };
    if valid {
        Ok(())
    } else {
        Err(SessionDelegationCorruption::Inconsistent("stored wait delivery").into())
    }
}

async fn validate_wait_replay_update(
    connection: &mut PgConnection,
    wait: DelegationWait,
) -> Result<(), SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT update.event_sequence, update.event_kind,
                update.storage_version, update.session_id, update.update_kind,
                update.spawning_tool_request_id, update.child_session_id,
                update.awaiting_tool_request_id, update.wait_mode,
                (update.policy_kind IS NULL
                    AND update.on_parent_stopped IS NULL
                    AND update.on_parent_cancelled IS NULL
                    AND update.delegation_event_ordinal IS NULL
                    AND update.delegation_event_kind IS NULL
                    AND update.outcome_kind IS NULL
                    AND update.reason_kind IS NULL
                    AND update.provenance_kind IS NULL
                    AND update.provenance_session_id IS NULL
                    AND update.provenance_turn_id IS NULL
                    AND update.provenance_goal_generation IS NULL
                    AND update.provenance_command_id IS NULL
                    AND update.result_spawning_request_id IS NULL
                    AND update.message_id IS NULL
                    AND update.sender_session_id IS NULL
                    AND update.recipient_session_id IS NULL
                    AND update.message_ordinal IS NULL
                    AND update.content_text IS NULL)
                    AS unused_subject_fields_absent,
                header.event_sequence AS header_event_sequence,
                header.event_kind AS header_event_kind,
                header.storage_version AS header_storage_version,
                header.session_id AS header_session_id
           FROM delegation_update_outbox_event AS update
           LEFT JOIN delegation_outbox_event AS header
             ON header.event_sequence = update.event_sequence
          WHERE update.update_kind = 'child_waiting'
            AND update.awaiting_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .fetch_optional(connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing(
        "wait update outbox satellite",
    ))?;
    let event_sequence: Decimal = required(&row, "event_sequence")?;
    let event_kind: String = required(&row, "event_kind")?;
    let storage_version: i16 = required(&row, "storage_version")?;
    let session_id: Uuid = required(&row, "session_id")?;
    if event_kind != "delegation_update"
        || storage_version != STORAGE_VERSION
        || session_id_from_uuid(session_id) != wait.parent()
        || required::<Decimal>(&row, "header_event_sequence")? != event_sequence
        || required::<String>(&row, "header_event_kind")? != event_kind
        || required::<i16>(&row, "header_storage_version")? != storage_version
        || required::<Uuid>(&row, "header_session_id")? != session_id
        || required::<String>(&row, "update_kind")? != "child_waiting"
        || tool_request_id_from_uuid(required(&row, "spawning_tool_request_id")?)
            != wait.spawning_request()
        || session_id_from_uuid(required(&row, "child_session_id")?) != wait.child()
        || tool_request_id_from_uuid(required(&row, "awaiting_tool_request_id")?)
            != wait.awaiting_request()
        || required::<String>(&row, "wait_mode")? != delegation_wait_mode_to_str(wait.mode())
        || !required::<bool>(&row, "unused_subject_fields_absent")?
    {
        return Err(SessionDelegationCorruption::Inconsistent("stored wait update").into());
    }
    Ok(())
}

async fn find_relation_for_wait(
    connection: &mut PgConnection,
    request: &DelegationAwaitRequest,
) -> Result<Option<ToolRequestId>, SessionDelegationRepositoryError> {
    let row = sqlx::query(DELEGATION_FIND_RELATION_FOR_WAIT)
        .bind(session_id_to_uuid(request.request().session()))
        .bind(session_id_to_uuid(request.child()))
        .fetch_optional(connection)
        .await?;
    row.map(|row| required::<Uuid>(&row, "spawning_tool_request_id").map(tool_request_id_from_uuid))
        .transpose()
}

async fn find_relation_for_message(
    connection: &mut PgConnection,
    request: &DelegationMessageRequest,
) -> Result<Option<ToolRequestId>, SessionDelegationRepositoryError> {
    let rows = sqlx::query(DELEGATION_FIND_RELATION_FOR_MESSAGE)
        .bind(session_id_to_uuid(request.request().session()))
        .bind(session_id_to_uuid(request.peer()))
        .fetch_all(connection)
        .await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => required::<Uuid>(row, "spawning_tool_request_id")
            .map(tool_request_id_from_uuid)
            .map(Some),
        _ => Err(SessionDelegationCorruption::Inconsistent(
            "ambiguous delegation message endpoints",
        )
        .into()),
    }
}

#[derive(Clone, Copy)]
struct RelationEndpoints {
    parent: SessionId,
    child: SessionId,
}

async fn prepare_fresh_message_event(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
    request: &DelegationMessageRequest,
    message: DelegationMessageId,
    dispatch: &ToolDispatchAuthority,
) -> Result<
    Result<(DelegationEvent, RelationEndpoints), DelegationTransitionFailure>,
    SessionDelegationRepositoryError,
> {
    let row = sqlx::query(
        "SELECT relation.parent_session_id, relation.child_session_id,
                (SELECT max(event.event_ordinal)
                   FROM session_delegation_event AS event
                  WHERE event.spawning_tool_request_id = relation.spawning_tool_request_id)
                    AS last_event_ordinal,
                EXISTS (
                    SELECT 1 FROM session_delegation_event AS event
                     WHERE event.spawning_tool_request_id = relation.spawning_tool_request_id
                       AND event.event_ordinal = 1
                       AND event.event_kind = 'spawned'
                ) AS has_spawn_event,
                EXISTS (
                    SELECT 1 FROM session_child_result AS result
                     WHERE result.spawning_tool_request_id = relation.spawning_tool_request_id
                ) AS is_terminal,
                EXISTS (
                    SELECT 1 FROM session_message AS existing
                     WHERE existing.message_id = $2
                       AND existing.spawning_tool_request_id =
                           relation.spawning_tool_request_id
                ) AS message_exists
           FROM session_delegation AS relation
          WHERE relation.spawning_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(message.into_uuid())
    .fetch_optional(connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing("delegation relation"))?;
    let endpoints = RelationEndpoints {
        parent: session_id_from_uuid(required(&row, "parent_session_id")?),
        child: session_id_from_uuid(required(&row, "child_session_id")?),
    };
    let last: Decimal = required(&row, "last_event_ordinal")?;
    let has_spawn_event: bool = required(&row, "has_spawn_event")?;
    if !has_spawn_event {
        return Err(SessionDelegationCorruption::Inconsistent("delegation event frontier").into());
    }
    if dispatch.request() != request.request() || request.request().id() == spawning_request {
        return Ok(Err(DelegationTransitionFailure::InvalidProvenance));
    }
    if required::<bool>(&row, "message_exists")? {
        return Ok(Err(DelegationTransitionFailure::DuplicateMessageIdentity));
    }
    let direction = if request.request().session() == endpoints.parent
        && request.peer() == endpoints.child
    {
        DelegationMessageDirection::ParentToChild
    } else if request.request().session() == endpoints.child && request.peer() == endpoints.parent {
        DelegationMessageDirection::ChildToParent
    } else {
        return Ok(Err(DelegationTransitionFailure::InvalidProvenance));
    };
    let stored_message = DelegationMessage::reconstitute(
        request,
        message,
        direction,
        DelegationMessageEndpoints {
            parent: endpoints.parent,
            child: endpoints.child,
        },
    )
    .ok_or(SessionDelegationCorruption::Inconsistent(
        "fresh message endpoints",
    ))?;
    let next = decode_positive(last, "last_event_ordinal")?
        .checked_add(1)
        .map(DelegationEventOrdinal::new)
        .ok_or(DelegationTransitionFailure::EventOrdinalExhausted);
    let ordinal = match next {
        Ok(ordinal) if required::<bool>(&row, "is_terminal")? || ordinal.get() < u64::MAX => {
            ordinal
        }
        Ok(_) | Err(_) => return Ok(Err(DelegationTransitionFailure::EventOrdinalExhausted)),
    };
    Ok(Ok((
        DelegationEvent::MessageDelivered {
            ordinal,
            message: stored_message,
        },
        endpoints,
    )))
}

async fn load_relation(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
) -> Result<SessionDelegation, SessionDelegationRepositoryError> {
    let row = sqlx::query(DELEGATION_LOAD_RELATION)
        .bind(tool_request_id_to_uuid(spawning_request))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SessionDelegationCorruption::Missing("delegation relation"))?;
    let parent = session_id_from_uuid(required(&row, "parent_session_id")?);
    let parent_turn = turn_id_from_uuid(required(&row, "parent_turn_id")?);
    let child = session_id_from_uuid(required(&row, "child_session_id")?);
    let child_turn = turn_id_from_uuid(required(&row, "child_turn_id")?);
    let policy = decode_policy(&row)?;
    let request = load_request_by_id(connection, spawning_request)
        .await?
        .ok_or(SessionDelegationCorruption::Missing(
            "spawning tool request",
        ))?;
    if request.session() != parent || request.turn() != parent_turn {
        return Err(SessionDelegationCorruption::Inconsistent("spawning request session").into());
    }
    let task: String = required(&row, "task_content")?;
    let spawn = DelegatedSpawnRequest::parse(request, task, policy)
        .map_err(|_| SessionDelegationCorruption::Inconsistent("spawning request purpose"))?;
    let events = load_events(connection, &spawn, parent, child).await?;
    SessionDelegationReconstitutionInput::new(spawn, child, child_turn, events)
        .reconstitute()
        .map_err(|error| SessionDelegationCorruption::Reconstitution(error.failure()).into())
}

async fn load_events(
    connection: &mut PgConnection,
    spawn: &DelegatedSpawnRequest,
    parent: SessionId,
    child: SessionId,
) -> Result<Vec<DelegationEvent>, SessionDelegationRepositoryError> {
    let rows = sqlx::query(
        "SELECT event.event_ordinal, event.event_kind, event.outcome_kind,
                event.reason_kind, event.provenance_kind,
                event.provenance_session_id, event.provenance_turn_id,
                event.provenance_goal_generation,
                event.provenance_tool_request_id, event.provenance_command_id,
                message.message_id, message.direction, message.content_text,
                result.outcome_kind AS result_outcome_kind,
                result.content_text AS result_content_text
           FROM session_delegation_event AS event
           LEFT JOIN session_message AS message
             ON message.spawning_tool_request_id = event.spawning_tool_request_id
            AND message.event_ordinal = event.event_ordinal
           LEFT JOIN session_child_result AS result
             ON result.spawning_tool_request_id = event.spawning_tool_request_id
            AND result.event_ordinal = event.event_ordinal
          WHERE event.spawning_tool_request_id = $1
          ORDER BY event.event_ordinal",
    )
    .bind(tool_request_id_to_uuid(spawn.request().id()))
    .fetch_all(&mut *connection)
    .await?;
    let mut message_request_ids = Vec::new();
    for row in &rows {
        let kind: String = required(row, "event_kind")?;
        if kind == "message_delivered" {
            message_request_ids.push(tool_request_id_from_uuid(required(
                row,
                "provenance_tool_request_id",
            )?));
        }
    }
    let mut message_requests = load_requests_by_id(connection, &message_request_ids).await?;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let ordinal = decode_ordinal(required(&row, "event_ordinal")?)?;
        let kind: String = required(&row, "event_kind")?;
        let event = match kind.as_str() {
            "spawned" => {
                validate_spawn_event(&row, spawn)?;
                DelegationEvent::Spawned {
                    ordinal,
                    provenance: DelegationProvenance::from_spawn(spawn),
                }
            }
            "message_delivered" => {
                let result_kind: Option<String> = optional(&row, "result_outcome_kind")?;
                let result_content: Option<String> = optional(&row, "result_content_text")?;
                if result_kind.is_some() || result_content.is_some() {
                    return Err(SessionDelegationCorruption::Inconsistent(
                        "message event result payload",
                    )
                    .into());
                }
                let request_id =
                    tool_request_id_from_uuid(required(&row, "provenance_tool_request_id")?);
                let request = message_requests
                    .remove(&request_id)
                    .ok_or(SessionDelegationCorruption::Missing("message tool request"))?;
                let direction = decode_direction(&required::<String>(&row, "direction")?)?;
                let peer = match direction {
                    DelegationMessageDirection::ParentToChild => child,
                    DelegationMessageDirection::ChildToParent => parent,
                };
                let content: String = required(&row, "content_text")?;
                let logical =
                    DelegationMessageRequest::parse(request, peer, content).map_err(|_| {
                        SessionDelegationCorruption::Inconsistent("message request purpose")
                    })?;
                validate_message_provenance(&row, logical.request())?;
                let message = DelegationMessage::reconstitute(
                    &logical,
                    DelegationMessageId::from_uuid(required(&row, "message_id")?),
                    direction,
                    DelegationMessageEndpoints { parent, child },
                )
                .ok_or(SessionDelegationCorruption::Inconsistent(
                    "message endpoints",
                ))?;
                DelegationEvent::MessageDelivered { ordinal, message }
            }
            "outcome_recorded" => DelegationEvent::OutcomeRecorded {
                ordinal,
                outcome: decode_outcome(&row)?,
            },
            value => {
                return Err(SessionDelegationCorruption::Unsupported {
                    field: "event_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        events.push(event);
    }
    Ok(events)
}

fn validate_spawn_event(
    row: &PgRow,
    spawn: &DelegatedSpawnRequest,
) -> Result<(), SessionDelegationRepositoryError> {
    let kind: String = required(row, "provenance_kind")?;
    let session = session_id_from_uuid(required(row, "provenance_session_id")?);
    let turn = turn_id_from_uuid(required(row, "provenance_turn_id")?);
    let request = tool_request_id_from_uuid(required(row, "provenance_tool_request_id")?);
    let goal_generation: Option<Decimal> = optional(row, "provenance_goal_generation")?;
    let command: Option<Uuid> = optional(row, "provenance_command_id")?;
    let result_kind: Option<String> = optional(row, "result_outcome_kind")?;
    let result_content: Option<String> = optional(row, "result_content_text")?;
    if kind != "tool_request"
        || session != spawn.request().session()
        || turn != spawn.request().turn()
        || request != spawn.request().id()
        || goal_generation.is_some()
        || command.is_some()
        || result_kind.is_some()
        || result_content.is_some()
    {
        return Err(SessionDelegationCorruption::Inconsistent("spawn event provenance").into());
    }
    Ok(())
}

fn decode_outcome(row: &PgRow) -> Result<DelegationOutcome, SessionDelegationRepositoryError> {
    let kind = decode_outcome_kind(&required::<String>(row, "outcome_kind")?)?;
    let payload_kind = optional::<String>(row, "result_outcome_kind")?
        .map(|value| decode_outcome_kind(&value))
        .transpose()?;
    if !outcome_payload_matches(kind, payload_kind) {
        return Err(SessionDelegationCorruption::Inconsistent("delegation outcome payload").into());
    }
    let reason = decode_outcome_reason(&required::<String>(row, "reason_kind")?)?;
    let content = optional::<String>(row, "result_content_text")?
        .map(DelegationContent::try_new)
        .transpose()
        .map_err(|_| SessionDelegationCorruption::Inconsistent("child result content"))?;
    let source = session_id_from_uuid(required(row, "provenance_session_id")?);
    let provenance_kind: String = required(row, "provenance_kind")?;
    let provenance_turn: Option<Uuid> = optional(row, "provenance_turn_id")?;
    let provenance_goal: Option<Decimal> = optional(row, "provenance_goal_generation")?;
    let provenance_request: Option<Uuid> = optional(row, "provenance_tool_request_id")?;
    let provenance_command: Option<Uuid> = optional(row, "provenance_command_id")?;
    let provenance = match provenance_kind.as_str() {
        "child_turn"
            if provenance_goal.is_none()
                && provenance_request.is_none()
                && provenance_command.is_none() =>
        {
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: source,
                turn: turn_id_from_uuid(provenance_turn.ok_or(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape"),
                )?),
            }
        }
        "parent_turn_command" if provenance_goal.is_none() && provenance_request.is_none() => {
            DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session: source,
                turn: turn_id_from_uuid(provenance_turn.ok_or(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape"),
                )?),
                command: decode_command(provenance_command.ok_or(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape"),
                )?)?,
            }
        }
        "parent_goal_command" => {
            if provenance_turn.is_some() || provenance_request.is_some() {
                return Err(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape").into(),
                );
            }
            let generation = decode_positive(
                provenance_goal.ok_or(SessionDelegationCorruption::Inconsistent(
                    "outcome provenance shape",
                ))?,
                "provenance_goal_generation",
            )?;
            DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session: source,
                generation: GoalGeneration::new(generation),
                command: decode_command(provenance_command.ok_or(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape"),
                )?)?,
            }
        }
        "parent_lifecycle_command"
            if provenance_turn.is_none()
                && provenance_goal.is_none()
                && provenance_request.is_none() =>
        {
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session: source,
                command: decode_command(provenance_command.ok_or(
                    SessionDelegationCorruption::Inconsistent("outcome provenance shape"),
                )?)?,
            }
        }
        "child_turn" | "parent_turn_command" | "parent_lifecycle_command" => {
            return Err(
                SessionDelegationCorruption::Inconsistent("outcome provenance shape").into(),
            );
        }
        value => {
            return Err(SessionDelegationCorruption::Unsupported {
                field: "provenance_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    DelegationOutcome::reconstitute(kind, content, reason, provenance)
        .ok_or_else(|| SessionDelegationCorruption::Inconsistent("delegation outcome").into())
}

fn outcome_payload_matches(
    kind: DelegationOutcomeKind,
    payload_kind: Option<DelegationOutcomeKind>,
) -> bool {
    match kind {
        DelegationOutcomeKind::ResultReturned
        | DelegationOutcomeKind::ChildFailed
        | DelegationOutcomeKind::ChildStopped
        | DelegationOutcomeKind::ChildCancelled => payload_kind == Some(kind),
        DelegationOutcomeKind::AlreadyTerminal | DelegationOutcomeKind::ContinueRunning => {
            payload_kind.is_none()
        }
    }
}

fn decode_policy(row: &PgRow) -> Result<ChildRelationshipPolicy, SessionDelegationRepositoryError> {
    let kind: String = required(row, "policy_kind")?;
    let on_parent_stopped: Option<String> = optional(row, "on_parent_stopped")?;
    let on_parent_cancelled: Option<String> = optional(row, "on_parent_cancelled")?;
    match delegation_policy_kind_from_str(&kind) {
        Some(DelegationPolicyStorageKind::Background)
            if on_parent_stopped.is_none() && on_parent_cancelled.is_none() =>
        {
            Ok(ChildRelationshipPolicy::Background)
        }
        Some(DelegationPolicyStorageKind::Background) => {
            Err(SessionDelegationCorruption::Inconsistent("background policy shape").into())
        }
        Some(DelegationPolicyStorageKind::Bound) => Ok(ChildRelationshipPolicy::Bound {
            on_parent_stopped: decode_action(
                on_parent_stopped
                    .as_deref()
                    .ok_or(SessionDelegationCorruption::Missing("on_parent_stopped"))?,
            )?,
            on_parent_cancelled: decode_action(
                on_parent_cancelled
                    .as_deref()
                    .ok_or(SessionDelegationCorruption::Missing("on_parent_cancelled"))?,
            )?,
        }),
        None => Err(SessionDelegationCorruption::Unsupported {
            field: "policy_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_action(value: &str) -> Result<BoundChildAction, SessionDelegationRepositoryError> {
    bound_child_action_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "bound_child_action",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_wait_mode(value: &str) -> Result<DelegationWaitMode, SessionDelegationRepositoryError> {
    delegation_wait_mode_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "wait_mode",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_direction(
    value: &str,
) -> Result<DelegationMessageDirection, SessionDelegationRepositoryError> {
    delegation_message_direction_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "direction",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_outcome_kind(
    value: &str,
) -> Result<DelegationOutcomeKind, SessionDelegationRepositoryError> {
    delegation_outcome_kind_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "outcome_kind",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_outcome_reason(
    value: &str,
) -> Result<DelegationOutcomeReason, SessionDelegationRepositoryError> {
    delegation_outcome_reason_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "reason_kind",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_ordinal(
    value: Decimal,
) -> Result<DelegationEventOrdinal, SessionDelegationRepositoryError> {
    Ok(DelegationEventOrdinal::new(decode_positive(
        value,
        "event_ordinal",
    )?))
}

fn decode_positive(
    value: Decimal,
    field: &'static str,
) -> Result<NonZeroU64, SessionDelegationRepositoryError> {
    u64::try_from(value)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or_else(|| SessionDelegationCorruption::Inconsistent(field).into())
}

fn decode_command(value: Uuid) -> Result<DurableCommandId, SessionDelegationRepositoryError> {
    durable_command_id_from_uuid(value)
        .map_err(|_| SessionDelegationCorruption::Inconsistent("provenance_command_id").into())
}

async fn insert_wait(
    connection: &mut PgConnection,
    wait: DelegationWait,
    parent_turn: TurnId,
) -> Result<(), SessionDelegationRepositoryError> {
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .bind(tool_request_id_to_uuid(wait.spawning_request()))
    .bind(session_id_to_uuid(wait.parent()))
    .bind(turn_id_to_uuid(parent_turn))
    .bind(session_id_to_uuid(wait.child()))
    .bind(delegation_wait_mode_to_str(wait.mode()))
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_wait_update(
    connection: &mut PgConnection,
    wait: DelegationWait,
) -> Result<(), SessionDelegationRepositoryError> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_update', $1, $2)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             awaiting_tool_request_id, wait_mode)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $7::text, $3, $4, $5, $6
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(wait.parent()))
    .bind(tool_request_id_to_uuid(wait.spawning_request()))
    .bind(session_id_to_uuid(wait.child()))
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .bind(delegation_wait_mode_to_str(wait.mode()))
    .bind(delegation_update_kind_to_str(
        DelegationUpdateStorageKind::ChildWaiting,
    ))
    .execute(connection)
    .await?;
    Ok(())
}

async fn child_result_exists(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
) -> Result<bool, SessionDelegationRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM session_child_result WHERE spawning_tool_request_id = $1)",
    )
    .bind(tool_request_id_to_uuid(spawning_request))
    .fetch_one(connection)
    .await
    .map_err(Into::into)
}

async fn insert_result_delivery(
    connection: &mut PgConnection,
    wait: DelegationWait,
    background_sequence: Option<NonZeroU64>,
) -> Result<(), SessionDelegationRepositoryError> {
    match wait.mode() {
        DelegationWaitMode::Foreground => {
            sqlx::query(
                "INSERT INTO session_child_result_delivery
                    (awaiting_tool_request_id, spawning_tool_request_id,
                     parent_session_id, delivery_sequence, delivery_kind)
                 VALUES ($1, $2, $3, NULL, NULL)",
            )
            .bind(tool_request_id_to_uuid(wait.awaiting_request()))
            .bind(tool_request_id_to_uuid(wait.spawning_request()))
            .bind(session_id_to_uuid(wait.parent()))
            .execute(connection)
            .await?;
        }
        DelegationWaitMode::Background => {
            let sequence = background_sequence.ok_or(SessionDelegationCorruption::Inconsistent(
                "background delivery sequence",
            ))?;
            sqlx::query(
                "WITH pending AS (
                    INSERT INTO session_pending_delivery
                        (recipient_session_id, delivery_sequence, delivery_kind)
                    VALUES ($1, $2, 'background_result')
                 )
                 INSERT INTO session_child_result_delivery
                    (awaiting_tool_request_id, spawning_tool_request_id,
                     parent_session_id, delivery_sequence, delivery_kind)
                 VALUES ($3, $4, $1, $2, 'background_result')",
            )
            .bind(session_id_to_uuid(wait.parent()))
            .bind(Decimal::from(sequence.get()))
            .bind(tool_request_id_to_uuid(wait.awaiting_request()))
            .bind(tool_request_id_to_uuid(wait.spawning_request()))
            .execute(connection)
            .await?;
        }
    }
    Ok(())
}

async fn park_foreground_turn(
    connection: &mut PgConnection,
    ended: &signalbox_domain::EndedToolAttempt,
    wait: DelegationWait,
) -> Result<(), SessionDelegationRepositoryError> {
    if ended.end()
        != &(ToolAttemptEnd::AwaitingChild {
            spawning_request: wait.spawning_request(),
            child: wait.child(),
        })
    {
        return Err(SessionDelegationRepositoryError::InvalidTransition(
            "foreground attempt did not retain its wait",
        ));
    }
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3
            AND state_kind = 'running' AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(ended.issuing_attempt().into_uuid())
    .bind(turn_id_to_uuid(ended.turn()))
    .bind(session_id_to_uuid(ended.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "foreground issuing attempt")?;
    let lifecycle_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_child', current_attempt_id = NULL,
                child_wait_request_id = $1
          WHERE turn_id = $2 AND session_id = $3 AND state_kind = 'active'
            AND active_phase_kind = 'running' AND current_attempt_id = $4
            AND active_tool_round_call_id IS NOT NULL",
    )
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .bind(turn_id_to_uuid(ended.turn()))
    .bind(session_id_to_uuid(ended.session()))
    .bind(ended.issuing_attempt().into_uuid())
    .execute(connection)
    .await?
    .rows_affected();
    require_single(lifecycle_rows, "foreground turn parking")
}

async fn pending_background_result_reservations(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<u64, SessionDelegationRepositoryError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM session_delegation_wait AS waiting
           LEFT JOIN session_child_result AS result
             ON result.spawning_tool_request_id = waiting.spawning_tool_request_id
          WHERE waiting.parent_session_id = $1
            AND waiting.wait_mode = 'background'
            AND result.spawning_tool_request_id IS NULL",
    )
    .bind(session_id_to_uuid(recipient))
    .fetch_one(connection)
    .await?;
    u64::try_from(count)
        .map_err(|_| SessionDelegationCorruption::Inconsistent("delivery reservations").into())
}

async fn latest_delivery_sequence(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<u64, SessionDelegationRepositoryError> {
    let latest = sqlx::query_scalar::<_, Option<Decimal>>(
        "SELECT max(delivery_sequence) FROM session_pending_delivery WHERE recipient_session_id = $1",
    )
    .bind(session_id_to_uuid(recipient))
    .fetch_one(connection)
    .await?;
    latest
        .map(|latest| decode_positive(latest, "delivery_sequence").map(NonZeroU64::get))
        .transpose()
        .map(Option::unwrap_or_default)
}

async fn delivery_reservation_available(
    connection: &mut PgConnection,
    recipient: SessionId,
    reservations: u64,
) -> Result<bool, SessionDelegationRepositoryError> {
    let latest = latest_delivery_sequence(connection, recipient).await?;
    Ok(u64::MAX.saturating_sub(latest) >= reservations)
}

async fn next_delivery_sequence_preserving(
    connection: &mut PgConnection,
    recipient: SessionId,
    reservations: u64,
) -> Result<Option<NonZeroU64>, SessionDelegationRepositoryError> {
    let latest = latest_delivery_sequence(connection, recipient).await?;
    let Some(next) = latest.checked_add(1).and_then(NonZeroU64::new) else {
        return Ok(None);
    };
    if u64::MAX.saturating_sub(next.get()) < reservations {
        return Ok(None);
    }
    Ok(Some(next))
}

async fn lock_delivery_session(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<(), SessionDelegationRepositoryError> {
    if !delivery_session_exists(connection, recipient).await? {
        return Err(SessionDelegationCorruption::Missing("delivery recipient").into());
    }
    Ok(())
}

async fn delivery_session_exists(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<bool, SessionDelegationRepositoryError> {
    let locked = sqlx::query_scalar::<_, Uuid>(DELEGATION_DELIVERY_SESSION)
        .bind(session_id_to_uuid(recipient))
        .fetch_optional(connection)
        .await?;
    Ok(locked.is_some_and(|locked| session_id_from_uuid(locked) == recipient))
}

async fn insert_message_state(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
    ordinal: DelegationEventOrdinal,
    message: &DelegationMessage,
    recipient: SessionId,
    delivery_sequence: NonZeroU64,
) -> Result<bool, SessionDelegationRepositoryError> {
    let (source, turn, request) =
        message
            .provenance()
            .tool_request()
            .ok_or(SessionDelegationCorruption::Inconsistent(
                "message provenance",
            ))?;
    let message_rows = sqlx::query(
        "INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         VALUES ($1, $2, $3, 'message_delivered', $4, $5)
         ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(message.id().into_uuid())
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(Decimal::from(ordinal.get()))
    .bind(delegation_message_direction_to_str(message.direction()))
    .bind(message.content().as_str())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if message_rows == 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, $2, 'message_delivered', 'tool_request', $3, $4, $5)",
    )
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(Decimal::from(ordinal.get()))
    .bind(session_id_to_uuid(source))
    .bind(turn_id_to_uuid(turn))
    .bind(tool_request_id_to_uuid(request))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH pending AS (
            INSERT INTO session_pending_delivery
                (recipient_session_id, delivery_sequence, delivery_kind)
            VALUES ($1, $2, 'message')
         )
         INSERT INTO session_message_delivery
            (message_id, spawning_tool_request_id, recipient_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($3, $4, $1, $2, 'message')",
    )
    .bind(session_id_to_uuid(recipient))
    .bind(Decimal::from(delivery_sequence.get()))
    .bind(message.id().into_uuid())
    .bind(tool_request_id_to_uuid(spawning_request))
    .execute(connection)
    .await?;
    Ok(true)
}

async fn append_message_update(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
    ordinal: DelegationEventOrdinal,
    message: &DelegationMessage,
    recipient: SessionId,
) -> Result<(), SessionDelegationRepositoryError> {
    let (sender, _, _) =
        message
            .provenance()
            .tool_request()
            .ok_or(SessionDelegationCorruption::Inconsistent(
                "message update provenance",
            ))?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_update', $1, $2)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, message_id,
             sender_session_id, recipient_session_id, message_ordinal,
             content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $8::text, $3, $4, $5, $2, $6, $7
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(recipient))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(message.id().into_uuid())
    .bind(session_id_to_uuid(sender))
    .bind(Decimal::from(ordinal.get()))
    .bind(message.content().as_str())
    .bind(delegation_update_kind_to_str(
        DelegationUpdateStorageKind::SessionMessage,
    ))
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_message_wake(
    connection: &mut PgConnection,
    recipient: SessionId,
    spawning_request: ToolRequestId,
    message: DelegationMessageId,
) -> Result<(), SessionDelegationRepositoryError> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_wake', $1, $2)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind, message_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $3, $5::text, $4
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(recipient))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(message.into_uuid())
    .bind(delegation_wake_subject_to_str(
        DelegationWakeStorageKind::Message,
    ))
    .execute(connection)
    .await?;
    Ok(())
}

async fn append_result_wake(
    connection: &mut PgConnection,
    parent: SessionId,
    spawning_request: ToolRequestId,
    awaiting_request: Option<ToolRequestId>,
) -> Result<(), SessionDelegationRepositoryError> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_wake', $1, $2)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, awaiting_tool_request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $3, $5::text, $3, $4
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(parent))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(awaiting_request.map(tool_request_id_to_uuid))
    .bind(delegation_wake_subject_to_str(
        DelegationWakeStorageKind::Result,
    ))
    .execute(connection)
    .await?;
    Ok(())
}

async fn load_message_replay(
    connection: &mut PgConnection,
    request: &DelegationMessageRequest,
) -> Result<Option<RecordedDelegationMessage>, SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT relation.parent_session_id, relation.child_session_id,
                event.spawning_tool_request_id AS message_spawning_request_id,
                event.provenance_kind, event.provenance_session_id,
                event.provenance_turn_id, event.provenance_goal_generation,
                event.provenance_tool_request_id, event.provenance_command_id,
                message.message_id, message.direction, message.content_text,
                message.event_ordinal,
                delivery.recipient_session_id AS delivery_recipient_session_id,
                delivery.delivery_sequence AS delivery_sequence,
                delivery.delivery_kind AS delivery_kind,
                pending.recipient_session_id AS pending_recipient_session_id,
                pending.delivery_sequence AS pending_delivery_sequence,
                pending.delivery_kind AS pending_delivery_kind,
                update_event.event_kind AS update_event_kind,
                update_event.storage_version AS update_storage_version,
                update_event.session_id AS update_session_id,
                update_event.update_kind AS update_kind,
                update_event.spawning_tool_request_id AS update_spawning_request_id,
                update_event.message_id AS update_message_id,
                update_event.sender_session_id AS update_sender_session_id,
                update_event.recipient_session_id AS update_recipient_session_id,
                update_event.message_ordinal AS update_message_ordinal,
                update_event.content_text AS update_content_text,
                wake_event.event_kind AS wake_event_kind,
                wake_event.storage_version AS wake_storage_version,
                wake_event.session_id AS wake_session_id,
                wake_event.spawning_tool_request_id AS wake_spawning_request_id,
                wake_event.subject_kind AS wake_subject_kind,
                wake_event.message_id AS wake_message_id,
                update_event.event_sequence AS update_event_sequence,
                update_header.event_sequence AS update_header_event_sequence,
                update_header.event_kind AS update_header_event_kind,
                update_header.storage_version AS update_header_storage_version,
                update_header.session_id AS update_header_session_id,
                wake_event.event_sequence AS wake_event_sequence,
                wake_header.event_sequence AS wake_header_event_sequence,
                wake_header.event_kind AS wake_header_event_kind,
                wake_header.storage_version AS wake_header_storage_version,
                wake_header.session_id AS wake_header_session_id
           FROM session_delegation_event AS event
           LEFT JOIN session_delegation AS relation
             ON relation.spawning_tool_request_id = event.spawning_tool_request_id
           LEFT JOIN session_message AS message
             ON message.spawning_tool_request_id = event.spawning_tool_request_id
            AND message.event_ordinal = event.event_ordinal
           LEFT JOIN session_message_delivery AS delivery
             ON delivery.message_id = message.message_id
            AND delivery.spawning_tool_request_id = message.spawning_tool_request_id
           LEFT JOIN session_pending_delivery AS pending
             ON pending.recipient_session_id = delivery.recipient_session_id
            AND pending.delivery_sequence = delivery.delivery_sequence
            AND pending.delivery_kind = delivery.delivery_kind
           LEFT JOIN delegation_update_outbox_event AS update_event
             ON update_event.update_kind = 'session_message'
            AND update_event.message_id = message.message_id
           LEFT JOIN delegation_outbox_event AS update_header
             ON update_header.event_sequence = update_event.event_sequence
           LEFT JOIN delegation_wake_outbox_event AS wake_event
             ON wake_event.subject_kind = 'message'
            AND wake_event.message_id = message.message_id
           LEFT JOIN delegation_outbox_event AS wake_header
             ON wake_header.event_sequence = wake_event.event_sequence
          WHERE event.event_kind = 'message_delivered'
            AND event.provenance_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request.request().id()))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let parent = session_id_from_uuid(required(&row, "parent_session_id")?);
    let child = session_id_from_uuid(required(&row, "child_session_id")?);
    validate_message_provenance(&row, request.request())?;
    let direction = decode_direction(&required::<String>(&row, "direction")?)?;
    let content: String = required(&row, "content_text")?;
    let delivery_recipient = session_id_from_uuid(required(&row, "delivery_recipient_session_id")?);
    let delivery_sequence =
        decode_positive(required(&row, "delivery_sequence")?, "delivery_sequence")?;
    let pending_recipient = session_id_from_uuid(required(&row, "pending_recipient_session_id")?);
    let pending_sequence = decode_positive(
        required(&row, "pending_delivery_sequence")?,
        "pending_delivery_sequence",
    )?;
    let delivery_kind: String = required(&row, "delivery_kind")?;
    let pending_kind: String = required(&row, "pending_delivery_kind")?;
    let message_id = required::<Uuid>(&row, "message_id")?;
    let message_ordinal = decode_ordinal(required(&row, "event_ordinal")?)?;
    let spawning_request = required::<Uuid>(&row, "message_spawning_request_id")?;
    let update_event_kind: String = required(&row, "update_event_kind")?;
    let wake_event_kind: String = required(&row, "wake_event_kind")?;
    let update_event_sequence: Decimal = required(&row, "update_event_sequence")?;
    let wake_event_sequence: Decimal = required(&row, "wake_event_sequence")?;
    let endpoints = RelationEndpoints { parent, child };
    let recipient = message_recipient(direction, endpoints);
    if content != request.content().as_str()
        || delivery_recipient != recipient
        || pending_recipient != delivery_recipient
        || pending_sequence != delivery_sequence
        || delivery_kind != "message"
        || pending_kind != delivery_kind
        || update_event_kind != "delegation_update"
        || required::<i16>(&row, "update_storage_version")? != STORAGE_VERSION
        || session_id_from_uuid(required(&row, "update_session_id")?) != recipient
        || required::<Decimal>(&row, "update_header_event_sequence")? != update_event_sequence
        || required::<String>(&row, "update_header_event_kind")? != "delegation_update"
        || required::<i16>(&row, "update_header_storage_version")? != STORAGE_VERSION
        || session_id_from_uuid(required(&row, "update_header_session_id")?) != recipient
        || required::<String>(&row, "update_kind")? != "session_message"
        || required::<Uuid>(&row, "update_spawning_request_id")? != spawning_request
        || required::<Uuid>(&row, "update_message_id")? != message_id
        || session_id_from_uuid(required(&row, "update_sender_session_id")?)
            != request.request().session()
        || session_id_from_uuid(required(&row, "update_recipient_session_id")?) != recipient
        || decode_ordinal(required(&row, "update_message_ordinal")?)? != message_ordinal
        || required::<String>(&row, "update_content_text")? != content
        || wake_event_kind != "delegation_wake"
        || required::<i16>(&row, "wake_storage_version")? != STORAGE_VERSION
        || session_id_from_uuid(required(&row, "wake_session_id")?) != recipient
        || required::<Decimal>(&row, "wake_header_event_sequence")? != wake_event_sequence
        || required::<String>(&row, "wake_header_event_kind")? != "delegation_wake"
        || required::<i16>(&row, "wake_header_storage_version")? != STORAGE_VERSION
        || session_id_from_uuid(required(&row, "wake_header_session_id")?) != recipient
        || required::<Uuid>(&row, "wake_spawning_request_id")? != spawning_request
        || required::<String>(&row, "wake_subject_kind")? != "message"
        || required::<Uuid>(&row, "wake_message_id")? != message_id
        || DelegationMessage::reconstitute(
            request,
            DelegationMessageId::from_uuid(message_id),
            direction,
            DelegationMessageEndpoints { parent, child },
        )
        .is_none()
    {
        return Err(SessionDelegationCorruption::Inconsistent("message replay").into());
    }
    Ok(Some(RecordedDelegationMessage {
        tool_request: request.request().id(),
        message: DelegationMessageId::from_uuid(message_id),
        direction,
        ordinal: message_ordinal,
        delivery_sequence,
    }))
}

async fn insert_message_rejection(
    connection: &mut PgConnection,
    request: ToolRequestId,
    message: DelegationMessageId,
    rejection: DelegationOperationRejection,
) -> Result<(), SessionDelegationRepositoryError> {
    let (kind, spawning_request, transition_failure) = encode_message_rejection(rejection)?;
    sqlx::query(
        "INSERT INTO session_delegation_message_rejection
            (tool_request_id, message_id, rejection_kind,
             spawning_tool_request_id, transition_failure_kind)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tool_request_id_to_uuid(request))
    .bind(message.into_uuid())
    .bind(kind)
    .bind(spawning_request.map(tool_request_id_to_uuid))
    .bind(transition_failure)
    .execute(connection)
    .await?;
    Ok(())
}

async fn insert_wait_rejection(
    connection: &mut PgConnection,
    request: ToolRequestId,
    rejection: DelegationOperationRejection,
) -> Result<(), SessionDelegationRepositoryError> {
    let (kind, spawning_request, transition_failure) = encode_wait_rejection(rejection)?;
    sqlx::query(
        "INSERT INTO session_delegation_wait_rejection
            (tool_request_id, rejection_kind, spawning_tool_request_id,
             transition_failure_kind)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tool_request_id_to_uuid(request))
    .bind(kind)
    .bind(spawning_request.map(tool_request_id_to_uuid))
    .bind(transition_failure)
    .execute(connection)
    .await?;
    Ok(())
}

async fn load_wait_rejection(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<DelegationOperationRejection>, SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT rejection_kind, spawning_tool_request_id, transition_failure_kind
           FROM session_delegation_wait_rejection
          WHERE tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(connection)
    .await?;
    row.map(|row| {
        let kind: String = required(&row, "rejection_kind")?;
        let spawning_request =
            optional::<Uuid>(&row, "spawning_tool_request_id")?.map(tool_request_id_from_uuid);
        let transition_failure: Option<String> = optional(&row, "transition_failure_kind")?;
        decode_wait_rejection(&kind, spawning_request, transition_failure.as_deref())
    })
    .transpose()
}

fn encode_wait_rejection(
    rejection: DelegationOperationRejection,
) -> Result<
    (&'static str, Option<ToolRequestId>, Option<&'static str>),
    SessionDelegationRepositoryError,
> {
    match rejection {
        DelegationOperationRejection::RelationshipNotFound => Ok((
            delegation_rejection_kind_to_str(DelegationRejectionStorageKind::RelationshipNotFound),
            None,
            None,
        )),
        DelegationOperationRejection::DeliverySequenceExhausted => Ok((
            delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::DeliverySequenceExhausted,
            ),
            None,
            None,
        )),
        DelegationOperationRejection::Transition {
            spawning_request,
            failure,
        } => Ok((
            delegation_rejection_kind_to_str(DelegationRejectionStorageKind::Transition),
            Some(spawning_request),
            Some(delegation_transition_failure_to_str(failure)),
        )),
        DelegationOperationRejection::StaleDispatch { .. }
        | DelegationOperationRejection::MessageIdentityCollision => {
            Err(SessionDelegationRepositoryError::InvalidTransition(
                "delegation rejection is not a definitive wait outcome",
            ))
        }
    }
}

fn decode_wait_rejection(
    kind: &str,
    spawning_request: Option<ToolRequestId>,
    transition_failure: Option<&str>,
) -> Result<DelegationOperationRejection, SessionDelegationRepositoryError> {
    let kind = delegation_rejection_kind_from_str(kind).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "rejection_kind",
            value: kind.to_owned(),
        }
    })?;
    match (kind, spawning_request, transition_failure) {
        (DelegationRejectionStorageKind::RelationshipNotFound, None, None) => {
            Ok(DelegationOperationRejection::RelationshipNotFound)
        }
        (DelegationRejectionStorageKind::DeliverySequenceExhausted, None, None) => {
            Ok(DelegationOperationRejection::DeliverySequenceExhausted)
        }
        (DelegationRejectionStorageKind::Transition, Some(spawning_request), Some(failure)) => {
            Ok(DelegationOperationRejection::Transition {
                spawning_request,
                failure: decode_transition_failure(failure)?,
            })
        }
        (
            DelegationRejectionStorageKind::RelationshipNotFound
            | DelegationRejectionStorageKind::DeliverySequenceExhausted
            | DelegationRejectionStorageKind::Transition
            | DelegationRejectionStorageKind::MessageIdentityCollision,
            _,
            _,
        ) => Err(SessionDelegationCorruption::Inconsistent("wait rejection shape").into()),
    }
}

async fn load_message_rejection(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<RecordedDelegationMessageRejection>, SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT message_id, rejection_kind, spawning_tool_request_id,
                transition_failure_kind
           FROM session_delegation_message_rejection
          WHERE tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(connection)
    .await?;
    row.map(|row| {
        let kind: String = required(&row, "rejection_kind")?;
        let spawning_request =
            optional::<Uuid>(&row, "spawning_tool_request_id")?.map(tool_request_id_from_uuid);
        let transition_failure: Option<String> = optional(&row, "transition_failure_kind")?;
        let rejection =
            decode_message_rejection(&kind, spawning_request, transition_failure.as_deref())?;
        Ok(RecordedDelegationMessageRejection {
            rejection,
            message: DelegationMessageId::from_uuid(required(&row, "message_id")?),
            durable: true,
        })
    })
    .transpose()
}

fn encode_message_rejection(
    rejection: DelegationOperationRejection,
) -> Result<
    (&'static str, Option<ToolRequestId>, Option<&'static str>),
    SessionDelegationRepositoryError,
> {
    match rejection {
        DelegationOperationRejection::RelationshipNotFound => Ok((
            delegation_rejection_kind_to_str(DelegationRejectionStorageKind::RelationshipNotFound),
            None,
            None,
        )),
        DelegationOperationRejection::MessageIdentityCollision => Ok((
            delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::MessageIdentityCollision,
            ),
            None,
            None,
        )),
        DelegationOperationRejection::DeliverySequenceExhausted => Ok((
            delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::DeliverySequenceExhausted,
            ),
            None,
            None,
        )),
        DelegationOperationRejection::Transition {
            spawning_request,
            failure,
        } => Ok((
            delegation_rejection_kind_to_str(DelegationRejectionStorageKind::Transition),
            Some(spawning_request),
            Some(delegation_transition_failure_to_str(failure)),
        )),
        DelegationOperationRejection::StaleDispatch { .. } => {
            Err(SessionDelegationRepositoryError::InvalidTransition(
                "stale delegation rejection is not a definitive message outcome",
            ))
        }
    }
}

fn decode_message_rejection(
    kind: &str,
    spawning_request: Option<ToolRequestId>,
    transition_failure: Option<&str>,
) -> Result<DelegationOperationRejection, SessionDelegationRepositoryError> {
    let kind = delegation_rejection_kind_from_str(kind).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "rejection_kind",
            value: kind.to_owned(),
        }
    })?;
    match (kind, spawning_request, transition_failure) {
        (DelegationRejectionStorageKind::RelationshipNotFound, None, None) => {
            Ok(DelegationOperationRejection::RelationshipNotFound)
        }
        (DelegationRejectionStorageKind::MessageIdentityCollision, None, None) => {
            Ok(DelegationOperationRejection::MessageIdentityCollision)
        }
        (DelegationRejectionStorageKind::DeliverySequenceExhausted, None, None) => {
            Ok(DelegationOperationRejection::DeliverySequenceExhausted)
        }
        (DelegationRejectionStorageKind::Transition, Some(spawning_request), Some(failure)) => {
            Ok(DelegationOperationRejection::Transition {
                spawning_request,
                failure: decode_transition_failure(failure)?,
            })
        }
        (
            DelegationRejectionStorageKind::RelationshipNotFound
            | DelegationRejectionStorageKind::MessageIdentityCollision
            | DelegationRejectionStorageKind::DeliverySequenceExhausted
            | DelegationRejectionStorageKind::Transition,
            _,
            _,
        ) => Err(SessionDelegationCorruption::Inconsistent("message rejection shape").into()),
    }
}

fn decode_transition_failure(
    value: &str,
) -> Result<DelegationTransitionFailure, SessionDelegationRepositoryError> {
    delegation_transition_failure_from_str(value).ok_or_else(|| {
        SessionDelegationCorruption::Unsupported {
            field: "transition_failure_kind",
            value: value.to_owned(),
        }
        .into()
    })
}

async fn message_outcome_exists(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, SessionDelegationRepositoryError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM session_delegation_event
              WHERE event_kind = 'message_delivered' AND provenance_tool_request_id = $1
             UNION ALL
             SELECT 1 FROM session_delegation_message_rejection
              WHERE tool_request_id = $1
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(connection)
    .await
    .map_err(Into::into)
}

fn validate_message_provenance(
    row: &PgRow,
    request: &signalbox_domain::ToolRequest,
) -> Result<(), SessionDelegationRepositoryError> {
    let kind: String = required(row, "provenance_kind")?;
    let session = session_id_from_uuid(required(row, "provenance_session_id")?);
    let turn = turn_id_from_uuid(required(row, "provenance_turn_id")?);
    let tool_request = tool_request_id_from_uuid(required(row, "provenance_tool_request_id")?);
    let goal_generation: Option<Decimal> = optional(row, "provenance_goal_generation")?;
    let command: Option<Uuid> = optional(row, "provenance_command_id")?;
    if kind != "tool_request"
        || session != request.session()
        || turn != request.turn()
        || tool_request != request.id()
        || goal_generation.is_some()
        || command.is_some()
    {
        return Err(SessionDelegationCorruption::Inconsistent("message provenance").into());
    }
    Ok(())
}

const fn message_recipient(
    direction: DelegationMessageDirection,
    endpoints: RelationEndpoints,
) -> SessionId {
    match direction {
        DelegationMessageDirection::ParentToChild => endpoints.child,
        DelegationMessageDirection::ChildToParent => endpoints.parent,
    }
}

fn required<T>(row: &PgRow, column: &'static str) -> Result<T, SessionDelegationRepositoryError>
where
    for<'value> T: sqlx::Decode<'value, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    optional(row, column)?.ok_or_else(|| SessionDelegationCorruption::Missing(column).into())
}

fn optional<T>(
    row: &PgRow,
    column: &'static str,
) -> Result<Option<T>, SessionDelegationRepositoryError>
where
    for<'value> T: sqlx::Decode<'value, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    let value = match row.try_get::<Option<T>, _>(column) {
        Ok(value) => value,
        Err(sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_)) => {
            return Err(SessionDelegationCorruption::InvalidColumn(column).into());
        }
        Err(error) => return Err(error.into()),
    };
    Ok(value)
}

fn require_single(
    rows: u64,
    relationship: &'static str,
) -> Result<(), SessionDelegationRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(SessionDelegationCorruption::Inconsistent(relationship).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S18: either message direction locks the same endpoint order.
    #[test]
    fn s18_opposite_message_directions_share_canonical_lock_order() {
        let lower = SessionId::from_uuid(Uuid::from_u128(1));
        let higher = SessionId::from_uuid(Uuid::from_u128(2));

        assert_eq!(ordered_session_pair(lower, higher), (lower, higher));
        assert_eq!(ordered_session_pair(higher, lower), (lower, higher));
    }

    #[test]
    fn terminal_outcome_reconstitution_requires_its_payload_row() {
        assert!(!outcome_payload_matches(
            DelegationOutcomeKind::ChildFailed,
            None
        ));
        assert!(outcome_payload_matches(
            DelegationOutcomeKind::ChildFailed,
            Some(DelegationOutcomeKind::ChildFailed)
        ));
    }

    #[test]
    fn non_payload_outcome_reconstitution_rejects_a_payload_row() {
        assert!(outcome_payload_matches(
            DelegationOutcomeKind::ContinueRunning,
            None
        ));
        assert!(!outcome_payload_matches(
            DelegationOutcomeKind::ContinueRunning,
            Some(DelegationOutcomeKind::ChildFailed)
        ));
    }
}
