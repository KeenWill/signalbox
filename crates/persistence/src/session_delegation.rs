//! Atomic PostgreSQL storage for delegated child sessions and relation history.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    BoundChildAction, ChildRelationshipPolicy, DelegationContent, DelegationEvent,
    DelegationMessageDirection, DelegationMessageId, DelegationOutcome, DelegationOutcomeReason,
    DelegationProvenance, DelegationWait, DelegationWaitMode, DescendantTerminationScope,
    DurableCommandId, ModelCallId, ModelSelectionRequest, NormalizedToolArguments,
    SessionConfigurationDefaults, SessionDelegation, SessionId, ToolArgumentsKind, ToolName,
    ToolRequest, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    SessionCredentialPin, commit_failure_is_ambiguous, mapping::dangerous_tool_auto_approval_to_str,
};

#[derive(Debug)]
pub enum SessionDelegationRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    ActiveDirectChildLimitReached,
    Corruption(&'static str),
    Domain(signalbox_domain::DelegationTransitionFailure),
}

impl fmt::Display for SessionDelegationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "delegation database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(formatter, "delegation commit ambiguous: {error}")
            }
            Self::ActiveDirectChildLimitReached => {
                formatter.write_str("active direct-child limit reached")
            }
            Self::Corruption(reason) => {
                write!(formatter, "delegation storage is corrupt: {reason}")
            }
            Self::Domain(reason) => write!(formatter, "delegation replay failed: {reason:?}"),
        }
    }
}

impl Error for SessionDelegationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::ActiveDirectChildLimitReached | Self::Corruption(_) | Self::Domain(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionDelegationRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone, Debug)]
pub struct SessionDelegationRepository {
    pool: PgPool,
    credential_pin: SessionCredentialPin,
}

impl SessionDelegationRepository {
    pub fn new(pool: PgPool, credential_pin: SessionCredentialPin) -> Self {
        Self {
            pool,
            credential_pin,
        }
    }

    /// Creates the child aggregate, relationship, and spawn event atomically.
    pub async fn create(
        &self,
        delegation: &SessionDelegation,
        defaults: &SessionConfigurationDefaults,
    ) -> Result<(), SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM session WHERE session_id = $1 FOR UPDATE")
            .bind(delegation.parent().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM session_delegation AS relation
              WHERE relation.parent_session_id = $1
                AND NOT EXISTS (SELECT 1 FROM session_child_result AS result
                    WHERE result.spawning_tool_request_id = relation.spawning_tool_request_id)",
        )
        .bind(delegation.parent().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if active >= 32 {
            return Err(SessionDelegationRepositoryError::ActiveDirectChildLimitReached);
        }
        insert_child(&mut transaction, delegation, defaults, &self.credential_pin).await?;
        insert_relation(&mut transaction, delegation).await?;
        insert_event(
            &mut transaction,
            delegation.spawning_request(),
            delegation
                .events()
                .first()
                .ok_or(SessionDelegationRepositoryError::Corruption(
                    "spawn event missing",
                ))?,
        )
        .await?;
        transaction.commit().await.map_err(classify_commit_failure)
    }

    pub async fn register_wait(
        &self,
        wait: DelegationWait,
    ) -> Result<(), SessionDelegationRepositoryError> {
        sqlx::query(
            "INSERT INTO session_delegation_wait
                (awaiting_tool_request_id, spawning_tool_request_id,
                 parent_session_id, parent_turn_id, child_session_id, wait_mode)
             VALUES ($1, $2, $3,
                 (SELECT turn_id FROM tool_request WHERE request_id = $1), $4, $5)",
        )
        .bind(wait.awaiting_request().into_uuid())
        .bind(wait.spawning_request().into_uuid())
        .bind(wait.parent().into_uuid())
        .bind(wait.child().into_uuid())
        .bind(wait_mode_to_str(wait.mode()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn deliver_message(
        &self,
        spawning_request: ToolRequestId,
        id: DelegationMessageId,
        content: DelegationContent,
        provenance: DelegationProvenance,
    ) -> Result<SessionDelegation, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let current = load_from_connection(&mut transaction, spawning_request)
            .await?
            .ok_or(SessionDelegationRepositoryError::Corruption(
                "relation missing",
            ))?;
        let updated = current
            .deliver_message(id, content, provenance)
            .map_err(|error| SessionDelegationRepositoryError::Domain(error.failure()))?;
        let event = updated
            .events()
            .last()
            .ok_or(SessionDelegationRepositoryError::Corruption(
                "message event missing",
            ))?;
        insert_event(&mut transaction, spawning_request, event).await?;
        transaction
            .commit()
            .await
            .map_err(classify_commit_failure)?;
        Ok(updated)
    }

    pub async fn record_outcome(
        &self,
        spawning_request: ToolRequestId,
        outcome: DelegationOutcome,
    ) -> Result<SessionDelegation, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let current = load_from_connection(&mut transaction, spawning_request)
            .await?
            .ok_or(SessionDelegationRepositoryError::Corruption(
                "relation missing",
            ))?;
        let updated = current
            .record_outcome(outcome)
            .map_err(|error| SessionDelegationRepositoryError::Domain(error.failure()))?;
        let event = updated
            .events()
            .last()
            .ok_or(SessionDelegationRepositoryError::Corruption(
                "outcome event missing",
            ))?;
        insert_event(&mut transaction, spawning_request, event).await?;
        transaction
            .commit()
            .await
            .map_err(classify_commit_failure)?;
        Ok(updated)
    }

    pub async fn load(
        &self,
        spawning_request: ToolRequestId,
    ) -> Result<Option<SessionDelegation>, SessionDelegationRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_from_connection(&mut connection, spawning_request).await
    }
}

fn classify_commit_failure(error: sqlx::Error) -> SessionDelegationRepositoryError {
    if commit_failure_is_ambiguous(&error) {
        SessionDelegationRepositoryError::CommitAmbiguous(error)
    } else {
        SessionDelegationRepositoryError::Database(error)
    }
}

async fn insert_child(
    connection: &mut PgConnection,
    delegation: &SessionDelegation,
    defaults: &SessionConfigurationDefaults,
    pin: &SessionCredentialPin,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind, spawning_tool_request_id)
         VALUES ($1, 'delegated', 'none', $2)",
    )
    .bind(delegation.child().into_uuid())
    .bind(delegation.spawning_request().into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO session_scheduler(session_id) VALUES ($1)")
        .bind(delegation.child().into_uuid())
        .execute(&mut *connection)
        .await?;
    let (selection_kind, direct, alias) = encode_selection(defaults.model());
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         VALUES ($1, 1, $2, $3, $4, $5, $6)",
    )
    .bind(delegation.child().into_uuid())
    .bind(selection_kind)
    .bind(direct)
    .bind(alias)
    .bind(dangerous_tool_auto_approval_to_str(
        defaults.dangerous_tool_auto_approval(),
    ))
    .bind(
        defaults
            .system_prompt()
            .map(signalbox_domain::SessionSystemPrompt::as_str),
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO session_current_defaults(session_id, current_version) VALUES ($1, 1)")
        .bind(delegation.child().into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_tool_request_id, recorded_at)
         VALUES ($1, 1, 'created', 'delegated_session', $2, transaction_timestamp())",
    )
    .bind(delegation.child().into_uuid())
    .bind(delegation.spawning_request().into_uuid())
    .execute(&mut *connection)
    .await?;
    for credential in pin.credentials() {
        sqlx::query(
            "INSERT INTO session_model_credential_entry
                (session_id, event_ordinal, model_family, credential_reference)
             VALUES ($1, 1, $2, $3)",
        )
        .bind(delegation.child().into_uuid())
        .bind(credential.model_family())
        .bind(credential.credential_reference())
        .execute(&mut *connection)
        .await?;
    }
    sqlx::query(
        "INSERT INTO session_current_model_credentials(session_id, current_event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(delegation.child().into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn encode_selection(
    selection: ModelSelectionRequest,
) -> (&'static str, Option<Uuid>, Option<Uuid>) {
    match selection {
        ModelSelectionRequest::Direct(value) => ("direct", Some(value.into_uuid()), None),
        ModelSelectionRequest::Alias(value) => ("alias", None, Some(value.into_uuid())),
    }
}

async fn insert_relation(
    connection: &mut PgConnection,
    delegation: &SessionDelegation,
) -> Result<(), sqlx::Error> {
    let (policy, stopped, cancelled) = encode_policy(delegation.policy());
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind, on_parent_stopped, on_parent_cancelled)
         VALUES ($1, $2, (SELECT turn_id FROM tool_request WHERE request_id = $1),
                 $3, $4, $5, $6)",
    )
    .bind(delegation.spawning_request().into_uuid())
    .bind(delegation.parent().into_uuid())
    .bind(delegation.child().into_uuid())
    .bind(policy)
    .bind(stopped)
    .bind(cancelled)
    .execute(connection)
    .await?;
    Ok(())
}

async fn insert_event(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
    event: &DelegationEvent,
) -> Result<(), SessionDelegationRepositoryError> {
    let encoded = EncodedEvent::from_domain(event)?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind,
             reason_kind, provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id, provenance_command_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(spawning_request.into_uuid())
    .bind(Decimal::from(event.ordinal().get()))
    .bind(encoded.event_kind)
    .bind(encoded.outcome_kind)
    .bind(encoded.reason_kind)
    .bind(encoded.provenance.kind)
    .bind(encoded.provenance.session)
    .bind(encoded.provenance.turn)
    .bind(encoded.provenance.request)
    .bind(encoded.provenance.command)
    .execute(&mut *connection)
    .await?;
    if let DelegationEvent::MessageDelivered { message, .. } = event {
        sqlx::query(
            "INSERT INTO session_message
                (message_id, spawning_tool_request_id, event_ordinal, event_kind,
                 direction, content_text)
             VALUES ($1, $2, $3, 'message_delivered', $4, $5)",
        )
        .bind(message.id().into_uuid())
        .bind(spawning_request.into_uuid())
        .bind(Decimal::from(event.ordinal().get()))
        .bind(direction_to_str(message.direction()))
        .bind(message.content().as_str())
        .execute(&mut *connection)
        .await?;
    }
    if let DelegationEvent::OutcomeRecorded { outcome, .. } = event
        && let Some((kind, content)) = terminal_result(outcome)
    {
        sqlx::query(
                "INSERT INTO session_child_result
                    (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind, content_text)
                 VALUES ($1, $2, 'outcome_recorded', $3, $4)",
            )
            .bind(spawning_request.into_uuid())
            .bind(Decimal::from(event.ordinal().get()))
            .bind(kind)
            .bind(content)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

async fn load_from_connection(
    connection: &mut PgConnection,
    spawning_request: ToolRequestId,
) -> Result<Option<SessionDelegation>, SessionDelegationRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT relation.*, request.request_id, request.producing_model_call_id,
                request.request_ordinal, request.tool_name,
                request.arguments_kind, request.arguments_text
           FROM session_delegation AS relation
           JOIN tool_request AS request
             ON request.request_id = relation.spawning_tool_request_id
          WHERE relation.spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let parent = SessionId::from_uuid(row.try_get("parent_session_id")?);
    let parent_turn = TurnId::from_uuid(row.try_get("parent_turn_id")?);
    let child = SessionId::from_uuid(row.try_get("child_session_id")?);
    let call = ModelCallId::from_uuid(row.try_get("producing_model_call_id")?);
    let policy = decode_policy(&row)?;
    let request = crate::tool_loop::decode_request(row, call, parent, parent_turn)
        .map_err(|_| SessionDelegationRepositoryError::Corruption("spawning request"))?;
    let mut delegation = SessionDelegation::spawn(&request, child, policy)
        .map_err(|error| SessionDelegationRepositoryError::Domain(error.failure()))?;
    let rows = sqlx::query(
        "SELECT event.*, message.message_id, message.direction, message.content_text,
                result.content_text AS result_content,
                provenance_request.producing_model_call_id AS provenance_call_id,
                provenance_request.request_ordinal AS provenance_request_ordinal,
                provenance_request.tool_name AS provenance_tool_name,
                provenance_request.arguments_kind AS provenance_arguments_kind,
                provenance_request.arguments_text AS provenance_arguments_text
           FROM session_delegation_event AS event
           LEFT JOIN session_message AS message USING (spawning_tool_request_id, event_ordinal)
           LEFT JOIN session_child_result AS result USING (spawning_tool_request_id, event_ordinal)
           LEFT JOIN tool_request AS provenance_request
             ON provenance_request.request_id = event.provenance_tool_request_id
          WHERE event.spawning_tool_request_id = $1 ORDER BY event.event_ordinal",
    )
    .bind(spawning_request.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if rows
        .first()
        .is_none_or(|row| row.try_get::<String, _>("event_kind").ok().as_deref() != Some("spawned"))
    {
        return Err(SessionDelegationRepositoryError::Corruption("spawn event"));
    }
    for row in rows.into_iter().skip(1) {
        delegation = replay_event(delegation, row)?;
    }
    Ok(Some(delegation))
}

fn replay_event(
    delegation: SessionDelegation,
    row: PgRow,
) -> Result<SessionDelegation, SessionDelegationRepositoryError> {
    let provenance = decode_provenance(&row)?;
    match required_string(&row, "event_kind")?.as_str() {
        "message_delivered" => {
            let id = DelegationMessageId::from_uuid(row.try_get("message_id")?);
            let content = DelegationContent::try_new(row.try_get("content_text")?)
                .map_err(|_| SessionDelegationRepositoryError::Corruption("message content"))?;
            let updated = delegation
                .deliver_message(id, content, provenance)
                .map_err(|error| SessionDelegationRepositoryError::Domain(error.failure()))?;
            let stored = required_string(&row, "direction")?;
            let actual = updated
                .events()
                .last()
                .and_then(DelegationEvent::message)
                .map(|message| direction_to_str(message.direction()));
            if actual != Some(stored.as_str()) {
                return Err(SessionDelegationRepositoryError::Corruption(
                    "message direction",
                ));
            }
            Ok(updated)
        }
        "outcome_recorded" => delegation
            .record_outcome(decode_outcome(&row, provenance)?)
            .map_err(|error| SessionDelegationRepositoryError::Domain(error.failure())),
        _ => Err(SessionDelegationRepositoryError::Corruption("event kind")),
    }
}

struct EncodedProvenance {
    kind: &'static str,
    session: Uuid,
    turn: Option<Uuid>,
    request: Option<Uuid>,
    command: Option<Uuid>,
}
impl EncodedProvenance {
    fn from_domain(value: DelegationProvenance) -> Result<Self, SessionDelegationRepositoryError> {
        if let Some((session, turn, request)) = value.tool_request() {
            Ok(Self {
                kind: "tool_request",
                session: session.into_uuid(),
                turn: Some(turn.into_uuid()),
                request: Some(request.into_uuid()),
                command: None,
            })
        } else if let Some((session, turn)) = value.child_turn() {
            Ok(Self {
                kind: "child_turn",
                session: session.into_uuid(),
                turn: Some(turn.into_uuid()),
                request: None,
                command: None,
            })
        } else if let Some((session, command)) = value.parent_command() {
            Ok(Self {
                kind: "parent_command",
                session: session.into_uuid(),
                turn: None,
                request: None,
                command: Some(command.into_uuid()),
            })
        } else {
            Err(SessionDelegationRepositoryError::Corruption(
                "domain provenance",
            ))
        }
    }
}

struct EncodedEvent {
    event_kind: &'static str,
    outcome_kind: Option<&'static str>,
    reason_kind: Option<&'static str>,
    provenance: EncodedProvenance,
}
impl EncodedEvent {
    fn from_domain(event: &DelegationEvent) -> Result<Self, SessionDelegationRepositoryError> {
        let (event_kind, outcome_kind, reason_kind, provenance) = match event {
            DelegationEvent::Spawned { provenance, .. } => ("spawned", None, None, *provenance),
            DelegationEvent::MessageDelivered { message, .. } => {
                ("message_delivered", None, None, message.provenance())
            }
            DelegationEvent::OutcomeRecorded { outcome, .. } => {
                let (kind, reason, provenance) = outcome_parts(outcome);
                (
                    "outcome_recorded",
                    Some(kind),
                    Some(reason_to_str(reason)),
                    provenance,
                )
            }
        };
        Ok(Self {
            event_kind,
            outcome_kind,
            reason_kind,
            provenance: EncodedProvenance::from_domain(provenance)?,
        })
    }
}

fn outcome_parts(
    outcome: &DelegationOutcome,
) -> (&'static str, DelegationOutcomeReason, DelegationProvenance) {
    match outcome {
        DelegationOutcome::ResultReturned {
            reason, provenance, ..
        } => ("result_returned", *reason, *provenance),
        DelegationOutcome::ChildFailed { reason, provenance } => {
            ("child_failed", *reason, *provenance)
        }
        DelegationOutcome::ChildStopped { reason, provenance } => {
            ("child_stopped", *reason, *provenance)
        }
        DelegationOutcome::ChildCancelled { reason, provenance } => {
            ("child_cancelled", *reason, *provenance)
        }
        DelegationOutcome::ContinueRunning { reason, provenance } => {
            ("continue_running", *reason, *provenance)
        }
    }
}

fn terminal_result(outcome: &DelegationOutcome) -> Option<(&'static str, Option<&str>)> {
    match outcome {
        DelegationOutcome::ResultReturned { content, .. } => {
            Some(("result_returned", Some(content.as_str())))
        }
        DelegationOutcome::ChildFailed { .. } => Some(("child_failed", None)),
        DelegationOutcome::ChildStopped { .. } => Some(("child_stopped", None)),
        DelegationOutcome::ChildCancelled { .. } => Some(("child_cancelled", None)),
        DelegationOutcome::ContinueRunning { .. } => None,
    }
}

fn decode_provenance(
    row: &PgRow,
) -> Result<DelegationProvenance, SessionDelegationRepositoryError> {
    let session = SessionId::from_uuid(row.try_get("provenance_session_id")?);
    match required_string(row, "provenance_kind")?.as_str() {
        "tool_request" => Ok(DelegationProvenance::from_tool_request(
            &load_event_request(row, session)?,
        )),
        "child_turn" => Ok(DelegationProvenance::from_child_turn(
            session,
            TurnId::from_uuid(row.try_get("provenance_turn_id")?),
        )),
        "parent_command" => Ok(DelegationProvenance::from_parent_command(
            session,
            DurableCommandId::from_uuid(row.try_get("provenance_command_id")?),
        )),
        _ => Err(SessionDelegationRepositoryError::Corruption(
            "provenance kind",
        )),
    }
}

fn load_event_request(
    row: &PgRow,
    session: SessionId,
) -> Result<ToolRequest, SessionDelegationRepositoryError> {
    let request = ToolRequestId::from_uuid(row.try_get("provenance_tool_request_id")?);
    let turn = TurnId::from_uuid(row.try_get("provenance_turn_id")?);
    let call = ModelCallId::from_uuid(row.try_get("provenance_call_id")?);
    let ordinal: Decimal = row.try_get("provenance_request_ordinal")?;
    let ordinal = u32::try_from(ordinal)
        .map_err(|_| SessionDelegationRepositoryError::Corruption("request ordinal"))?;
    let name = ToolName::try_new(row.try_get("provenance_tool_name")?)
        .map_err(|_| SessionDelegationRepositoryError::Corruption("tool name"))?;
    let arguments_kind = match required_string(row, "provenance_arguments_kind")?.as_str() {
        "json" => ToolArgumentsKind::Json,
        "undecodable" => ToolArgumentsKind::Undecodable,
        _ => {
            return Err(SessionDelegationRepositoryError::Corruption(
                "arguments kind",
            ));
        }
    };
    let arguments = NormalizedToolArguments::try_from_stored(
        arguments_kind,
        row.try_get("provenance_arguments_text")?,
    )
    .map_err(|_| SessionDelegationRepositoryError::Corruption("tool arguments"))?;
    Ok(ToolRequestReconstitutionInput::new(
        request,
        session,
        turn,
        call,
        ToolRequestOrdinal::from_u32(ordinal),
        name,
        arguments,
    )
    .into_request())
}

fn decode_outcome(
    row: &PgRow,
    provenance: DelegationProvenance,
) -> Result<DelegationOutcome, SessionDelegationRepositoryError> {
    let reason = reason_from_str(&required_string(row, "reason_kind")?)?;
    match required_string(row, "outcome_kind")?.as_str() {
        "result_returned" => Ok(DelegationOutcome::ResultReturned {
            content: DelegationContent::try_new(row.try_get("result_content")?)
                .map_err(|_| SessionDelegationRepositoryError::Corruption("result content"))?,
            reason,
            provenance,
        }),
        "child_failed" => Ok(DelegationOutcome::ChildFailed { reason, provenance }),
        "child_stopped" => Ok(DelegationOutcome::ChildStopped { reason, provenance }),
        "child_cancelled" => Ok(DelegationOutcome::ChildCancelled { reason, provenance }),
        "continue_running" => Ok(DelegationOutcome::ContinueRunning { reason, provenance }),
        _ => Err(SessionDelegationRepositoryError::Corruption("outcome kind")),
    }
}

fn encode_policy(
    policy: ChildRelationshipPolicy,
) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    match policy {
        ChildRelationshipPolicy::Background => ("background", None, None),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => (
            "bound",
            Some(action_to_str(on_parent_stopped)),
            Some(action_to_str(on_parent_cancelled)),
        ),
    }
}
fn decode_policy(row: &PgRow) -> Result<ChildRelationshipPolicy, SessionDelegationRepositoryError> {
    match required_string(row, "policy_kind")?.as_str() {
        "background" => Ok(ChildRelationshipPolicy::Background),
        "bound" => Ok(ChildRelationshipPolicy::Bound {
            on_parent_stopped: action_from_str(&required_string(row, "on_parent_stopped")?)?,
            on_parent_cancelled: action_from_str(&required_string(row, "on_parent_cancelled")?)?,
        }),
        _ => Err(SessionDelegationRepositoryError::Corruption("policy kind")),
    }
}
fn action_to_str(value: BoundChildAction) -> &'static str {
    match value {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}
fn action_from_str(value: &str) -> Result<BoundChildAction, SessionDelegationRepositoryError> {
    match value {
        "keep_running" => Ok(BoundChildAction::KeepRunning),
        "stop" => Ok(BoundChildAction::Stop),
        "cancel" => Ok(BoundChildAction::Cancel),
        _ => Err(SessionDelegationRepositoryError::Corruption("bound action")),
    }
}
fn wait_mode_to_str(value: DelegationWaitMode) -> &'static str {
    match value {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}
fn direction_to_str(value: DelegationMessageDirection) -> &'static str {
    match value {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    }
}
fn reason_to_str(value: DelegationOutcomeReason) -> &'static str {
    match value {
        DelegationOutcomeReason::ChildCompleted => "child_completed",
        DelegationOutcomeReason::ChildExecutionFailed => "child_execution_failed",
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAlone,
        } => "parent_stopped_parent_alone",
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => "parent_stopped_parent_and_descendants",
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAlone,
        } => "parent_cancelled_parent_alone",
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => "parent_cancelled_parent_and_descendants",
    }
}
fn reason_from_str(
    value: &str,
) -> Result<DelegationOutcomeReason, SessionDelegationRepositoryError> {
    match value {
        "child_completed" => Ok(DelegationOutcomeReason::ChildCompleted),
        "child_execution_failed" => Ok(DelegationOutcomeReason::ChildExecutionFailed),
        "parent_stopped_parent_alone" => Ok(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAlone,
        }),
        "parent_stopped_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        "parent_cancelled_parent_alone" => Ok(DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAlone,
        }),
        "parent_cancelled_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        _ => Err(SessionDelegationRepositoryError::Corruption(
            "outcome reason",
        )),
    }
}
fn required_string(
    row: &PgRow,
    field: &'static str,
) -> Result<String, SessionDelegationRepositoryError> {
    row.try_get::<Option<String>, _>(field)?
        .ok_or(SessionDelegationRepositoryError::Corruption(field))
}
