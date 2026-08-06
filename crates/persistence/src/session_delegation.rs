//! Atomic delegated-session await and peer-message persistence.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::DelegationMessageDeliveryProjection;
use signalbox_domain::{
    BoundChildAction, ChildRelationshipPolicy, DelegatedSpawnRequest, DelegationAwaitRequest,
    DelegationContent, DelegationEvent, DelegationEventOrdinal, DelegationMessage,
    DelegationMessageDirection, DelegationMessageId, DelegationMessageRequest, DelegationOutcome,
    DelegationOutcomeKind, DelegationOutcomeReason, DelegationProvenance,
    DelegationProvenanceReconstitutionInput, DelegationRequestFailure, DelegationTransitionFailure,
    DelegationWait, DelegationWaitMode, DurableCommandId, GoalGeneration, ReconstitutedToolAttempt,
    SessionDelegation, SessionDelegationReconstitutionFailure,
    SessionDelegationReconstitutionInput, SessionId, ToolAttemptEnd, ToolAttemptObservation,
    ToolDispatchAuthority, ToolEffectClass, ToolRequestId, ToolResultContent, ToolResultText,
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
        DelegationPolicyStorageKind, bound_child_action_from_str,
        delegation_message_direction_from_str, delegation_message_direction_to_str,
        delegation_outcome_kind_from_str, delegation_outcome_reason_from_str,
        delegation_policy_kind_from_str, delegation_wait_mode_from_str,
        delegation_wait_mode_to_str, durable_command_id_from_uuid, session_id_from_uuid,
        session_id_to_uuid, tool_request_id_from_uuid, tool_request_id_to_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    },
    tool_loop::{
        ToolLoopRepositoryError, load_active_batch_from_connection,
        load_optional_foreground_delegation_outcome, load_request_by_id, load_requests_by_id,
        lock_tool_session, persist_ended_attempt,
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
    relation: SessionDelegation,
    event: DelegationEvent,
}

impl RecordedDelegationDelivery {
    pub const fn relation(&self) -> &SessionDelegation {
        &self.relation
    }

    pub const fn event(&self) -> &DelegationEvent {
        &self.event
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordDelegationWaitOutcome {
    Recorded(RecordedDelegationWait),
    Rejected(DelegationOperationRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordDelegationMessageOutcome {
    Recorded(Box<RecordedDelegationMessage>),
    Rejected(DelegationOperationRejection),
}

/// Stored facts failed checked reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDelegationCorruption {
    Missing(&'static str),
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
            lock_delivery_session(&mut transaction, request.request().session()).await?;
            lock_tool_session(&mut transaction, request.request().session()).await?;
            if let Some(spawning_request) =
                load_wait_replay_subject(&mut transaction, request.request().id()).await?
            {
                if !dispatch.matches_request(request.request()) {
                    return Ok(RecordDelegationWaitOutcome::Rejected(
                        DelegationOperationRejection::StaleDispatch {
                            state: DelegationRequestExecutionState::AttemptEnded,
                        },
                    ));
                }
                let relation = load_relation(&mut transaction, spawning_request).await?;
                let wait = DelegationWait::reconstitute(&relation, &request).ok_or(
                    SessionDelegationCorruption::Inconsistent("stored wait purpose"),
                )?;
                if load_wait_mode(&mut transaction, request.request().id()).await? != wait.mode() {
                    return Err(
                        SessionDelegationCorruption::Inconsistent("stored wait mode").into(),
                    );
                }
                return Ok(RecordDelegationWaitOutcome::Recorded(
                    RecordedDelegationWait { wait },
                ));
            }

            let Some(spawning_request) = find_relation_for_wait(&mut transaction, &request).await?
            else {
                return Ok(RecordDelegationWaitOutcome::Rejected(
                    DelegationOperationRejection::RelationshipNotFound,
                ));
            };
            let relation = load_relation(&mut transaction, spawning_request).await?;
            let dispatch =
                match resolve_dispatch(&mut transaction, request.request(), dispatch).await? {
                    ResolvedDelegationDispatch::Executable(dispatch) => *dispatch,
                    ResolvedDelegationDispatch::NonExecutable(state) => {
                        return Ok(RecordDelegationWaitOutcome::Rejected(
                            DelegationOperationRejection::StaleDispatch { state },
                        ));
                    }
                };
            let wait = match relation.register_wait(&request, &dispatch) {
                Ok(wait) => wait,
                Err(error) => {
                    return Ok(RecordDelegationWaitOutcome::Rejected(
                        DelegationOperationRejection::Transition {
                            spawning_request: error.spawning_request(),
                            failure: error.failure(),
                        },
                    ));
                }
            };
            if dispatch.attempt().effect_class() != ToolEffectClass::EffectFree {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "await_session requires an effect-free attempt",
                ));
            }

            let result_exists =
                child_result_exists(&mut transaction, wait.spawning_request()).await?;
            let background_delivery_sequence = match (result_exists, wait.mode()) {
                (true, DelegationWaitMode::Background) => {
                    let Some(sequence) =
                        next_delivery_sequence_for_locked_session(&mut transaction, wait.parent())
                            .await?
                    else {
                        return Ok(RecordDelegationWaitOutcome::Rejected(
                            DelegationOperationRejection::DeliverySequenceExhausted,
                        ));
                    };
                    Some(sequence)
                }
                (false, DelegationWaitMode::Background) | (_, DelegationWaitMode::Foreground) => {
                    None
                }
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
        self.record_message_with_source(request, message, DispatchSource::Issued(dispatch))
            .await
    }

    async fn record_message_with_source(
        &self,
        request: DelegationMessageRequest,
        message: DelegationMessageId,
        dispatch: DispatchSource<'_>,
    ) -> Result<RecordDelegationMessageOutcome, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if !session_exists(&mut transaction, request.peer()).await? {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::RelationshipNotFound,
                ));
            }
            lock_message_sessions(
                &mut transaction,
                request.request().session(),
                request.peer(),
            )
            .await?;
            if let Some(receipt) = load_message_replay(&mut transaction, &request).await? {
                if !dispatch.matches_request(request.request()) {
                    return Ok(RecordDelegationMessageOutcome::Rejected(
                        DelegationOperationRejection::StaleDispatch {
                            state: DelegationRequestExecutionState::AttemptEnded,
                        },
                    ));
                }
                return Ok(RecordDelegationMessageOutcome::Recorded(Box::new(receipt)));
            }
            let Some(spawning_request) =
                find_relation_for_message(&mut transaction, &request).await?
            else {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::RelationshipNotFound,
                ));
            };
            let relation = load_relation(&mut transaction, spawning_request).await?;
            let dispatch =
                match resolve_dispatch(&mut transaction, request.request(), dispatch).await? {
                    ResolvedDelegationDispatch::Executable(dispatch) => *dispatch,
                    ResolvedDelegationDispatch::NonExecutable(state) => {
                        return Ok(RecordDelegationMessageOutcome::Rejected(
                            DelegationOperationRejection::StaleDispatch { state },
                        ));
                    }
                };
            if dispatch.attempt().effect_class() != ToolEffectClass::ExternalEffect {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "send_session_message requires an external-effect attempt",
                ));
            }
            let tool_request = request.request().id();
            let (_, event) = match relation.deliver_message(request, message, &dispatch) {
                Ok(recorded) => recorded,
                Err(error) => {
                    return Ok(RecordDelegationMessageOutcome::Rejected(
                        DelegationOperationRejection::Transition {
                            spawning_request: error.spawning_request(),
                            failure: error.failure(),
                        },
                    ));
                }
            };
            let stored_message =
                event
                    .message()
                    .ok_or(SessionDelegationCorruption::Inconsistent(
                        "message transition event",
                    ))?;
            let endpoints = relation_endpoints(&mut transaction, spawning_request).await?;
            let recipient = message_recipient(stored_message.direction(), endpoints);
            let Some(delivery_sequence) =
                next_delivery_sequence_for_locked_session(&mut transaction, recipient).await?
            else {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::DeliverySequenceExhausted,
                ));
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
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::MessageIdentityCollision,
                ));
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
            let ended = complete_attempt(&dispatch, message_receipt(&dispatch, &receipt)?)?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            Ok(RecordDelegationMessageOutcome::Recorded(Box::new(receipt)))
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
            Err(_)
                if load_wait_replay_subject(&mut connection, request)
                    .await?
                    .is_some() =>
            {
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
            Err(error)
                if matches!(error.failure(), DelegationRequestFailure::InvalidContent(_)) =>
            {
                return Ok(ProcessDelegationOutcome::InvalidRequest);
            }
            Err(_) if message_replay_exists(&mut connection, request).await? => {
                return Ok(ProcessDelegationOutcome::Rejected(
                    ProcessDelegationRequestRejection::MessageConflict,
                ));
            }
            Err(_) => return Ok(ProcessDelegationOutcome::InvalidRequest),
        };
        drop(connection);
        let outcome = self
            .record_message_with_source(logical.clone(), message, DispatchSource::Reconstitute)
            .await?;
        Ok(match outcome {
            RecordDelegationMessageOutcome::Recorded(recorded) => {
                ProcessDelegationOutcome::Applied((logical, recorded))
            }
            RecordDelegationMessageOutcome::Rejected(rejection) => {
                ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
                    rejection,
                ))
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
        let relation = load_relation(&mut transaction, wait.spawning_request()).await?;
        let event = relation
            .events()
            .iter()
            .find(|event| event.outcome() == Some(&outcome))
            .cloned()
            .ok_or(SessionDelegationCorruption::Inconsistent(
                "foreground result event",
            ))?;
        transaction.rollback().await?;
        Ok(Some(RecordedDelegationDelivery { relation, event }))
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
        if batch
            .awaiting_approval()
            .is_some_and(|waiting| waiting.request() == request.id())
            || (batch.approval(request.id()).is_none() && batch.attempt(request.id()).is_none())
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
    let (denied, closed, attempted): (bool, bool, bool) = sqlx::query_as(
        "SELECT
            EXISTS (
                SELECT 1 FROM semantic_transcript_entry
                 WHERE tool_result_request_id = $1 AND payload_kind = 'tool_denied'
            ),
            EXISTS (
                SELECT 1 FROM semantic_transcript_entry
                 WHERE tool_result_request_id = $1
                   AND payload_kind = 'tool_closed_by_turn_end'
            ),
            EXISTS (SELECT 1 FROM tool_attempt WHERE request_id = $1)",
    )
    .bind(tool_request_id_to_uuid(request.id()))
    .fetch_one(&mut *connection)
    .await?;
    Ok(if denied {
        DelegationRequestExecutionState::Denied
    } else if closed {
        DelegationRequestExecutionState::Closed
    } else if attempted {
        DelegationRequestExecutionState::AttemptEnded
    } else {
        DelegationRequestExecutionState::Closed
    })
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

fn wait_receipt(wait: DelegationWait) -> Result<ToolResultText, SessionDelegationRepositoryError> {
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
    dispatch: &ToolDispatchAuthority,
    receipt: &RecordedDelegationMessage,
) -> Result<ToolResultText, SessionDelegationRepositoryError> {
    ToolResultText::try_new(
        serde_json::json!({
            "result": "session_message_sent",
            "tool_request_id": dispatch.request().id().as_uuid().to_string(),
            "message_id": receipt.message().as_uuid().to_string(),
            "direction": delegation_message_direction_to_str(receipt.direction()),
            "ordinal": receipt.ordinal().get(),
            "delivery_sequence": receipt.delivery_sequence().get(),
        })
        .to_string(),
    )
    .map_err(|_| SessionDelegationCorruption::Inconsistent("message receipt").into())
}

async fn load_wait_replay_subject(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<ToolRequestId>, SessionDelegationRepositoryError> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT spawning_tool_request_id
           FROM session_delegation_wait
          WHERE awaiting_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut *connection)
    .await
    .map(|value| value.map(tool_request_id_from_uuid))
    .map_err(Into::into)
}

async fn load_wait_mode(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<DelegationWaitMode, SessionDelegationRepositoryError> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT wait_mode FROM session_delegation_wait WHERE awaiting_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing("delegation wait"))?;
    decode_wait_mode(&value)
}

async fn find_relation_for_wait(
    connection: &mut PgConnection,
    request: &DelegationAwaitRequest,
) -> Result<Option<ToolRequestId>, SessionDelegationRepositoryError> {
    sqlx::query_scalar::<_, Uuid>(DELEGATION_FIND_RELATION_FOR_WAIT)
        .bind(session_id_to_uuid(request.request().session()))
        .bind(session_id_to_uuid(request.child()))
        .fetch_optional(connection)
        .await
        .map(|value| value.map(tool_request_id_from_uuid))
        .map_err(Into::into)
}

async fn find_relation_for_message(
    connection: &mut PgConnection,
    request: &DelegationMessageRequest,
) -> Result<Option<ToolRequestId>, SessionDelegationRepositoryError> {
    sqlx::query_scalar::<_, Uuid>(DELEGATION_FIND_RELATION_FOR_MESSAGE)
        .bind(session_id_to_uuid(request.request().session()))
        .bind(session_id_to_uuid(request.peer()))
        .fetch_optional(connection)
        .await
        .map(|value| value.map(tool_request_id_from_uuid))
        .map_err(Into::into)
}

#[derive(Clone, Copy)]
struct RelationEndpoints {
    parent: SessionId,
    child: SessionId,
}

async fn relation_endpoints(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
) -> Result<RelationEndpoints, SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT parent_session_id, child_session_id
           FROM session_delegation
          WHERE spawning_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(spawning_request))
    .fetch_optional(connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing("delegation relation"))?;
    Ok(RelationEndpoints {
        parent: session_id_from_uuid(required(&row, "parent_session_id")?),
        child: session_id_from_uuid(required(&row, "child_session_id")?),
    })
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
                let stored_source = session_id_from_uuid(required(&row, "provenance_session_id")?);
                if logical.request().session() != stored_source {
                    return Err(SessionDelegationCorruption::Inconsistent(
                        "message provenance session",
                    )
                    .into());
                }
                let message = DelegationMessage::reconstitute(
                    &logical,
                    DelegationMessageId::from_uuid(required(&row, "message_id")?),
                    direction,
                    parent,
                    child,
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
    if kind != "tool_request"
        || session != spawn.request().session()
        || turn != spawn.request().turn()
        || request != spawn.request().id()
    {
        return Err(SessionDelegationCorruption::Inconsistent("spawn event provenance").into());
    }
    Ok(())
}

fn decode_outcome(row: &PgRow) -> Result<DelegationOutcome, SessionDelegationRepositoryError> {
    let kind = decode_outcome_kind(&required::<String>(row, "outcome_kind")?)?;
    let reason = decode_outcome_reason(&required::<String>(row, "reason_kind")?)?;
    let content = row
        .try_get::<Option<String>, _>("result_content_text")?
        .map(DelegationContent::try_new)
        .transpose()
        .map_err(|_| SessionDelegationCorruption::Inconsistent("child result content"))?;
    let source = session_id_from_uuid(required(row, "provenance_session_id")?);
    let provenance_kind: String = required(row, "provenance_kind")?;
    let provenance = match provenance_kind.as_str() {
        "child_turn" => DelegationProvenanceReconstitutionInput::ChildTurn {
            session: source,
            turn: turn_id_from_uuid(required(row, "provenance_turn_id")?),
        },
        "parent_turn_command" => DelegationProvenanceReconstitutionInput::ParentTurnCommand {
            session: source,
            turn: turn_id_from_uuid(required(row, "provenance_turn_id")?),
            command: decode_command(required(row, "provenance_command_id")?)?,
        },
        "parent_goal_command" => {
            let generation = decode_positive(
                required(row, "provenance_goal_generation")?,
                "provenance_goal_generation",
            )?;
            DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session: source,
                generation: GoalGeneration::new(generation),
                command: decode_command(required(row, "provenance_command_id")?)?,
            }
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

fn decode_policy(row: &PgRow) -> Result<ChildRelationshipPolicy, SessionDelegationRepositoryError> {
    let kind: String = required(row, "policy_kind")?;
    match delegation_policy_kind_from_str(&kind) {
        Some(DelegationPolicyStorageKind::Background) => Ok(ChildRelationshipPolicy::Background),
        Some(DelegationPolicyStorageKind::Bound) => Ok(ChildRelationshipPolicy::Bound {
            on_parent_stopped: decode_action(&required::<String>(row, "on_parent_stopped")?)?,
            on_parent_cancelled: decode_action(&required::<String>(row, "on_parent_cancelled")?)?,
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
                'child_waiting', $3, $4, $5, $6
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(wait.parent()))
    .bind(tool_request_id_to_uuid(wait.spawning_request()))
    .bind(session_id_to_uuid(wait.child()))
    .bind(tool_request_id_to_uuid(wait.awaiting_request()))
    .bind(delegation_wait_mode_to_str(wait.mode()))
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

async fn next_delivery_sequence_for_locked_session(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<Option<NonZeroU64>, SessionDelegationRepositoryError> {
    let latest = sqlx::query_scalar::<_, Option<Decimal>>(
        "SELECT max(delivery_sequence) FROM session_pending_delivery WHERE recipient_session_id = $1",
    )
    .bind(session_id_to_uuid(recipient))
    .fetch_one(connection)
    .await?;
    match latest {
        None => Ok(Some(NonZeroU64::MIN)),
        Some(latest) => Ok(decode_positive(latest, "delivery_sequence")?.checked_add(1)),
    }
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
                'session_message', $3, $4, $5, $2, $6, $7
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(recipient))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(message.id().into_uuid())
    .bind(session_id_to_uuid(sender))
    .bind(Decimal::from(ordinal.get()))
    .bind(message.content().as_str())
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
                $3, 'message', $4
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(recipient))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(message.into_uuid())
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
                $3, 'result', $3, $4
           FROM header",
    )
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(parent))
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(awaiting_request.map(tool_request_id_to_uuid))
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
                message.message_id, message.direction, message.content_text,
                message.event_ordinal, delivery.delivery_sequence
           FROM session_delegation_event AS event
           JOIN session_delegation AS relation
             ON relation.spawning_tool_request_id = event.spawning_tool_request_id
           JOIN session_message AS message
             ON message.spawning_tool_request_id = event.spawning_tool_request_id
            AND message.event_ordinal = event.event_ordinal
           JOIN session_message_delivery AS delivery
             ON delivery.message_id = message.message_id
            AND delivery.spawning_tool_request_id = message.spawning_tool_request_id
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
    let direction = decode_direction(&required::<String>(&row, "direction")?)?;
    let content: String = required(&row, "content_text")?;
    if content != request.content().as_str()
        || DelegationMessage::reconstitute(
            request,
            DelegationMessageId::from_uuid(required(&row, "message_id")?),
            direction,
            parent,
            child,
        )
        .is_none()
    {
        return Err(SessionDelegationCorruption::Inconsistent("message replay").into());
    }
    Ok(Some(RecordedDelegationMessage {
        tool_request: request.request().id(),
        message: DelegationMessageId::from_uuid(required(&row, "message_id")?),
        direction,
        ordinal: decode_ordinal(required(&row, "event_ordinal")?)?,
        delivery_sequence: decode_positive(
            required(&row, "delivery_sequence")?,
            "delivery_sequence",
        )?,
    }))
}

async fn message_replay_exists(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, SessionDelegationRepositoryError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
               FROM session_delegation_event
              WHERE event_kind = 'message_delivered'
                AND provenance_tool_request_id = $1
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(connection)
    .await
    .map_err(Into::into)
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
    row.try_get::<Option<T>, _>(column)?
        .ok_or_else(|| SessionDelegationCorruption::Missing(column).into())
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

    /// S18 / INV-010: either message direction locks the same endpoint order.
    #[test]
    fn s18_inv010_opposite_message_directions_share_canonical_lock_order() {
        let lower = SessionId::from_uuid(Uuid::from_u128(1));
        let higher = SessionId::from_uuid(Uuid::from_u128(2));

        assert_eq!(ordered_session_pair(lower, higher), (lower, higher));
        assert_eq!(ordered_session_pair(higher, lower), (lower, higher));
    }
}
