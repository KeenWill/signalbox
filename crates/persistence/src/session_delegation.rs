//! Atomic delegated-session await and peer-message persistence.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_domain::{
    BoundChildAction, ChildRelationshipPolicy, DelegatedSpawnRequest, DelegationAwaitRequest,
    DelegationContent, DelegationEvent, DelegationEventOrdinal, DelegationMessage,
    DelegationMessageDirection, DelegationMessageId, DelegationMessageRequest, DelegationOutcome,
    DelegationOutcomeKind, DelegationOutcomeReason, DelegationProvenance,
    DelegationProvenanceReconstitutionInput, DelegationTransitionFailure, DelegationWait,
    DelegationWaitMode, DescendantTerminationScope, DurableCommandId, GoalGeneration,
    ReconstitutedToolAttempt, SessionDelegation, SessionDelegationReconstitutionFailure,
    SessionDelegationReconstitutionInput, SessionId, ToolAttemptEnd, ToolAttemptObservation,
    ToolDispatchAuthority, ToolEffectClass, ToolRequestId, ToolResultContent, ToolResultText,
    TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    lock_inventory::{
        DELEGATION_FIND_RELATION_FOR_MESSAGE, DELEGATION_FIND_RELATION_FOR_WAIT,
        DELEGATION_LOAD_RELATION,
    },
    mapping::{
        durable_command_id_from_uuid, session_id_from_uuid, session_id_to_uuid,
        tool_request_id_from_uuid, tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    tool_loop::{
        ToolLoopRepositoryError, load_active_batch_from_connection,
        load_optional_foreground_delegation_outcome, load_request_by_id, lock_tool_session,
        persist_ended_attempt,
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
    relation: SessionDelegation,
    event: DelegationEvent,
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
    pub const fn relation(&self) -> &SessionDelegation {
        &self.relation
    }

    pub const fn event(&self) -> &DelegationEvent {
        &self.event
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

/// Expected rejection of a checked delegation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationOperationRejection {
    RelationshipNotFound,
    StaleDispatch,
    MessageIdentityCollision,
    DeliverySequenceExhausted,
    Transition(DelegationTransitionFailure),
}

#[derive(Clone, Copy)]
enum DispatchSource<'a> {
    Issued(&'a ToolDispatchAuthority),
    Reconstitute,
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
            lock_tool_session(&mut transaction, request.request().session()).await?;
            if let Some(spawning_request) =
                load_wait_replay_subject(&mut transaction, request.request().id()).await?
            {
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
            let Some(dispatch) =
                resolve_dispatch(&mut transaction, request.request(), dispatch).await?
            else {
                return Ok(RecordDelegationWaitOutcome::Rejected(
                    DelegationOperationRejection::StaleDispatch,
                ));
            };
            let wait = match relation.register_wait(&request, &dispatch) {
                Ok(wait) => wait,
                Err(error) => {
                    return Ok(RecordDelegationWaitOutcome::Rejected(
                        DelegationOperationRejection::Transition(error.failure()),
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
                        next_delivery_sequence(&mut transaction, wait.parent()).await?
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
            lock_tool_session(&mut transaction, request.request().session()).await?;
            if let Some(receipt) = load_message_replay(&mut transaction, &request).await? {
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
            let Some(dispatch) =
                resolve_dispatch(&mut transaction, request.request(), dispatch).await?
            else {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::StaleDispatch,
                ));
            };
            if dispatch.attempt().effect_class() != ToolEffectClass::ExternalEffect {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "send_session_message requires an external-effect attempt",
                ));
            }
            if message_identity_exists(&mut transaction, message).await? {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::MessageIdentityCollision,
                ));
            }
            let (relation, event) = match relation.deliver_message(request, message, &dispatch) {
                Ok(recorded) => recorded,
                Err(error) => {
                    return Ok(RecordDelegationMessageOutcome::Rejected(
                        DelegationOperationRejection::Transition(error.failure()),
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
                next_delivery_sequence(&mut transaction, recipient).await?
            else {
                return Ok(RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::DeliverySequenceExhausted,
                ));
            };
            insert_message_state(
                &mut transaction,
                spawning_request,
                event.ordinal(),
                stored_message,
                recipient,
                delivery_sequence,
            )
            .await?;
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
                relation,
                event,
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
        Option<(DelegationAwaitRequest, RecordDelegationWaitOutcome)>,
        SessionDelegationRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        let Some(stored) = load_request_by_id(&mut connection, request).await? else {
            return Ok(None);
        };
        if stored.session() != session || stored.turn() != turn {
            return Ok(None);
        }
        let Ok(logical) = DelegationAwaitRequest::parse(stored, child, mode) else {
            return Ok(None);
        };
        drop(connection);
        let outcome = self
            .record_wait_with_source(logical.clone(), DispatchSource::Reconstitute)
            .await?;
        Ok(Some((logical, outcome)))
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
        Option<(DelegationMessageRequest, RecordDelegationMessageOutcome)>,
        SessionDelegationRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        let Some(stored) = load_request_by_id(&mut connection, request).await? else {
            return Ok(None);
        };
        if stored.session() != session || stored.turn() != turn {
            return Ok(None);
        }
        let Ok(logical) = DelegationMessageRequest::parse(stored, peer, content) else {
            return Ok(None);
        };
        drop(connection);
        let outcome = self
            .record_message_with_source(logical.clone(), message, DispatchSource::Reconstitute)
            .await?;
        Ok(Some((logical, outcome)))
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

async fn dispatch_is_current(
    connection: &mut PgConnection,
    dispatch: &ToolDispatchAuthority,
) -> Result<bool, SessionDelegationRepositoryError> {
    let request = dispatch.request();
    let Some(batch) =
        load_active_batch_from_connection(connection, request.session(), request.turn()).await?
    else {
        return Ok(false);
    };
    Ok(batch
        .resume_in_flight_dispatch(dispatch.attempt().attempt())
        .is_ok_and(|stored| stored == *dispatch))
}

async fn resolve_dispatch(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
    source: DispatchSource<'_>,
) -> Result<Option<ToolDispatchAuthority>, SessionDelegationRepositoryError> {
    match source {
        DispatchSource::Issued(dispatch) => dispatch_is_current(connection, dispatch)
            .await
            .map(|current| current.then(|| dispatch.clone())),
        DispatchSource::Reconstitute => {
            let Some(batch) =
                load_active_batch_from_connection(connection, request.session(), request.turn())
                    .await?
            else {
                return Ok(None);
            };
            let Some(ReconstitutedToolAttempt::Current(attempt)) = batch.attempt(request.id())
            else {
                return Ok(None);
            };
            let dispatch = batch
                .resume_in_flight_dispatch(attempt.attempt())
                .ok()
                .filter(|dispatch| dispatch.request() == request);
            Ok(dispatch)
        }
    }
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
            "direction": encode_direction(receipt.direction()),
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

async fn relation_endpoints(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
) -> Result<(SessionId, SessionId), SessionDelegationRepositoryError> {
    let row = sqlx::query(
        "SELECT parent_session_id, child_session_id
           FROM session_delegation
          WHERE spawning_tool_request_id = $1",
    )
    .bind(tool_request_id_to_uuid(spawning_request))
    .fetch_optional(connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing("delegation relation"))?;
    Ok((
        session_id_from_uuid(required(&row, "parent_session_id")?),
        session_id_from_uuid(required(&row, "child_session_id")?),
    ))
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
                let request = load_request_by_id(connection, request_id)
                    .await?
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
    match kind.as_str() {
        "background" => Ok(ChildRelationshipPolicy::Background),
        "bound" => Ok(ChildRelationshipPolicy::Bound {
            on_parent_stopped: decode_action(&required::<String>(row, "on_parent_stopped")?)?,
            on_parent_cancelled: decode_action(&required::<String>(row, "on_parent_cancelled")?)?,
        }),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "policy_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_action(value: &str) -> Result<BoundChildAction, SessionDelegationRepositoryError> {
    match value {
        "keep_running" => Ok(BoundChildAction::KeepRunning),
        "stop" => Ok(BoundChildAction::Stop),
        "cancel" => Ok(BoundChildAction::Cancel),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "bound_child_action",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_wait_mode(value: &str) -> Result<DelegationWaitMode, SessionDelegationRepositoryError> {
    match value {
        "foreground" => Ok(DelegationWaitMode::Foreground),
        "background" => Ok(DelegationWaitMode::Background),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "wait_mode",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_direction(
    value: &str,
) -> Result<DelegationMessageDirection, SessionDelegationRepositoryError> {
    match value {
        "parent_to_child" => Ok(DelegationMessageDirection::ParentToChild),
        "child_to_parent" => Ok(DelegationMessageDirection::ChildToParent),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "direction",
            value: value.to_owned(),
        }
        .into()),
    }
}

const fn encode_direction(direction: DelegationMessageDirection) -> &'static str {
    match direction {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    }
}

fn decode_outcome_kind(
    value: &str,
) -> Result<DelegationOutcomeKind, SessionDelegationRepositoryError> {
    match value {
        "result_returned" => Ok(DelegationOutcomeKind::ResultReturned),
        "child_failed" => Ok(DelegationOutcomeKind::ChildFailed),
        "child_stopped" => Ok(DelegationOutcomeKind::ChildStopped),
        "child_cancelled" => Ok(DelegationOutcomeKind::ChildCancelled),
        "already_terminal" => Ok(DelegationOutcomeKind::AlreadyTerminal),
        "continue_running" => Ok(DelegationOutcomeKind::ContinueRunning),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "outcome_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_outcome_reason(
    value: &str,
) -> Result<DelegationOutcomeReason, SessionDelegationRepositoryError> {
    match value {
        "child_completed" => Ok(DelegationOutcomeReason::ChildCompleted),
        "child_execution_failed" => Ok(DelegationOutcomeReason::ChildExecutionFailed),
        "child_result_unavailable" => Ok(DelegationOutcomeReason::ChildResultUnavailable),
        "child_cancelled" => Ok(DelegationOutcomeReason::ChildCancelled),
        "parent_stopped_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        "parent_cancelled_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        value => Err(SessionDelegationCorruption::Unsupported {
            field: "reason_kind",
            value: value.to_owned(),
        }
        .into()),
    }
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
    .bind(encode_wait_mode(wait.mode()))
    .execute(connection)
    .await?;
    Ok(())
}

const fn encode_wait_mode(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
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
    .bind(encode_wait_mode(wait.mode()))
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

async fn next_delivery_sequence(
    connection: &mut PgConnection,
    recipient: SessionId,
) -> Result<Option<NonZeroU64>, SessionDelegationRepositoryError> {
    let locked = sqlx::query_scalar::<_, Uuid>(
        "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE",
    )
    .bind(session_id_to_uuid(recipient))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(SessionDelegationCorruption::Missing("delivery recipient"))?;
    if session_id_from_uuid(locked) != recipient {
        return Err(SessionDelegationCorruption::Inconsistent("delivery recipient lock").into());
    }
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

async fn message_identity_exists(
    connection: &mut PgConnection,
    message: DelegationMessageId,
) -> Result<bool, SessionDelegationRepositoryError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session_message WHERE message_id = $1)")
        .bind(message.into_uuid())
        .fetch_one(connection)
        .await
        .map_err(Into::into)
}

async fn insert_message_state(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
    ordinal: DelegationEventOrdinal,
    message: &DelegationMessage,
    recipient: SessionId,
    delivery_sequence: NonZeroU64,
) -> Result<(), SessionDelegationRepositoryError> {
    let (source, turn, request) =
        message
            .provenance()
            .tool_request()
            .ok_or(SessionDelegationCorruption::Inconsistent(
                "message provenance",
            ))?;
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
        "INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         VALUES ($1, $2, $3, 'message_delivered', $4, $5)",
    )
    .bind(message.id().into_uuid())
    .bind(tool_request_id_to_uuid(spawning_request))
    .bind(Decimal::from(ordinal.get()))
    .bind(encode_direction(message.direction()))
    .bind(message.content().as_str())
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
    Ok(())
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
        "SELECT event.spawning_tool_request_id,
                relation.parent_session_id, relation.child_session_id,
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
    let spawning_request = tool_request_id_from_uuid(required(&row, "spawning_tool_request_id")?);
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
    let relation = load_relation(connection, spawning_request).await?;
    let event = relation
        .events()
        .iter()
        .find(|event| {
            event.message().is_some_and(|message| {
                message
                    .provenance()
                    .tool_request()
                    .is_some_and(|(_, _, tool_request)| tool_request == request.request().id())
            })
        })
        .cloned()
        .ok_or(SessionDelegationCorruption::Missing(
            "replayed message event",
        ))?;
    Ok(Some(RecordedDelegationMessage {
        relation,
        event,
        message: DelegationMessageId::from_uuid(required(&row, "message_id")?),
        direction,
        ordinal: decode_ordinal(required(&row, "event_ordinal")?)?,
        delivery_sequence: decode_positive(
            required(&row, "delivery_sequence")?,
            "delivery_sequence",
        )?,
    }))
}

const fn message_recipient(
    direction: DelegationMessageDirection,
    endpoints: (SessionId, SessionId),
) -> SessionId {
    match direction {
        DelegationMessageDirection::ParentToChild => endpoints.1,
        DelegationMessageDirection::ChildToParent => endpoints.0,
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
