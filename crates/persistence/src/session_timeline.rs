//! PostgreSQL adapter for bounded historical session-timeline reads.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SessionTimelineBounds, SessionTimelineDescriptor, SessionTimelineDetail,
    SessionTimelineDetailBody, SessionTimelineDetailPage, SessionTimelineEventKind,
    SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts, SessionTimelineWindow,
    SessionWorkFacts, TimelineAddress, TimelineApprovalSource, TimelineBodyContinuation,
    TimelineBodyField, TimelineContinuation, TimelineDetailContinuation, TimelineDetailCursor,
    TimelineDetailLimits, TimelineGoalEvent, TimelineImportedEvidence,
    TimelineModelCallDisposition, TimelineModelCallState, TimelineModelUsage, TimelineTextExcerpt,
    TimelineToolAttempt, TimelineToolState, TimelineTurnLifecycleKind, TimelineWindowAnchor,
    TimelineWindowLimits,
};
use signalbox_domain::{
    ModelCallId, RunnerSandboxProfile, SessionId, ToolApprovalDecider, ToolApprovalDecision,
    ToolApprovalResolution, ToolDecisionSource, TurnId,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};

use crate::{
    mapping::{OutboxEventDiscriminator, outbox_event_discriminator_from_str},
    outbox::{
        DispatchedDelegationOutcome, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWake, DispatchedModelCallDisposition, DispatchedModelCallState,
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
        DispatchedRunnerState, DispatchedToolBatchState, OutboxDispatchError,
    },
};

const PROJECTED_ITEM_ENVELOPE_BYTES: u32 = 64;

/// Integrity failure in the durable timeline projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTimelineCorruption {
    Missing(&'static str),
    InvalidOrdinal(&'static str),
    UnsupportedEventKind(String),
    ItemProjectionOverflow,
    InvalidDetailCursor,
    DetailProjectionOverflow,
    MissingDetailRecord,
}

impl fmt::Display for SessionTimelineCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing session timeline {field}"),
            Self::InvalidOrdinal(field) => write!(formatter, "invalid session timeline {field}"),
            Self::UnsupportedEventKind(kind) => {
                write!(formatter, "unsupported session timeline event kind: {kind}")
            }
            Self::ItemProjectionOverflow => {
                formatter.write_str("session timeline item projection overflowed")
            }
            Self::InvalidDetailCursor => {
                formatter.write_str("invalid session timeline detail cursor")
            }
            Self::DetailProjectionOverflow => {
                formatter.write_str("session timeline detail projection overflowed")
            }
            Self::MissingDetailRecord => {
                formatter.write_str("missing session timeline detail record")
            }
        }
    }
}

impl Error for SessionTimelineCorruption {}

/// Database or fail-closed projection failure.
#[derive(Debug)]
pub enum SessionTimelineRepositoryError {
    Database(sqlx::Error),
    Corruption(SessionTimelineCorruption),
    Outbox(OutboxDispatchError),
}

impl fmt::Display for SessionTimelineRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "session timeline database failure: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::Outbox(error) => {
                write!(formatter, "session timeline detail decode failed: {error}")
            }
        }
    }
}

impl Error for SessionTimelineRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::Outbox(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SessionTimelineRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SessionTimelineCorruption> for SessionTimelineRepositoryError {
    fn from(error: SessionTimelineCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<OutboxDispatchError> for SessionTimelineRepositoryError {
    fn from(error: OutboxDispatchError) -> Self {
        Self::Outbox(error)
    }
}

/// PostgreSQL implementation of the bounded historical read port.
#[derive(Clone, Debug)]
pub struct SessionTimelineRepository {
    pool: PgPool,
}

impl SessionTimelineRepository {
    /// Uses the supplied pool for repeatable-read session projections.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads one lightweight descriptor without materializing event rows.
    pub async fn read_descriptor(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionTimelineDescriptor>, SessionTimelineRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_descriptor(&mut connection, session).await
    }

    /// Reads at most the validated item and projected-byte limits.
    pub async fn read_window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> Result<Option<SessionTimelineWindow>, SessionTimelineRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let Some(descriptor) = load_descriptor(&mut transaction, session).await? else {
            transaction.commit().await?;
            return Ok(None);
        };
        let fetch_limit = i64::from(limits.max_items()) + 1;
        let rows = match anchor {
            TimelineWindowAnchor::First => {
                fetch_window(
                    &mut transaction,
                    FIRST_WINDOW_SQL,
                    session,
                    None,
                    fetch_limit,
                )
                .await?
            }
            TimelineWindowAnchor::Latest => {
                fetch_window(
                    &mut transaction,
                    LATEST_WINDOW_SQL,
                    session,
                    None,
                    fetch_limit,
                )
                .await?
            }
            TimelineWindowAnchor::Before(address) => {
                fetch_window(
                    &mut transaction,
                    BEFORE_WINDOW_SQL,
                    session,
                    Some(address),
                    fetch_limit,
                )
                .await?
            }
            TimelineWindowAnchor::After(address) => {
                fetch_window(
                    &mut transaction,
                    AFTER_WINDOW_SQL,
                    session,
                    Some(address),
                    fetch_limit,
                )
                .await?
            }
            TimelineWindowAnchor::Around(address) => {
                fetch_window(
                    &mut transaction,
                    AROUND_WINDOW_SQL,
                    session,
                    Some(address),
                    fetch_limit,
                )
                .await?
            }
        };
        let mut projected_bytes = 0_u32;
        let mut items = Vec::with_capacity(usize::from(limits.max_items()));
        for row in rows.into_iter().take(usize::from(limits.max_items())) {
            let item = decode_item(row)?;
            let Some(next_bytes) = projected_bytes.checked_add(item.projected_structured_bytes)
            else {
                return Err(SessionTimelineCorruption::ItemProjectionOverflow.into());
            };
            if next_bytes > limits.max_projected_bytes() {
                break;
            }
            projected_bytes = next_bytes;
            items.push(item);
        }
        items.sort_by_key(|item| item.address);
        let first = items.first().map(|item| item.address);
        let latest = items.last().map(|item| item.address);
        let continuation_before = match first.zip(descriptor.bounds.first) {
            Some((loaded, bound)) if loaded > bound => TimelineContinuation::MoreAt(loaded),
            _ => TimelineContinuation::Exhausted,
        };
        let continuation_after = match latest.zip(descriptor.bounds.latest) {
            Some((loaded, bound)) if loaded < bound => TimelineContinuation::MoreAt(loaded),
            _ => TimelineContinuation::Exhausted,
        };
        transaction.commit().await?;

        Ok(Some(SessionTimelineWindow {
            session,
            items,
            projected_structured_bytes: projected_bytes,
            continuation_before,
            continuation_after,
        }))
    }

    /// Reads one selected typed body without scanning unrelated history.
    pub async fn read_item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, SessionTimelineRepositoryError> {
        if cursor.is_some_and(|cursor| cursor.address != address) {
            return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let Some(event) = load_detail_event(&mut transaction, session, address).await? else {
            transaction.commit().await?;
            return Ok(None);
        };
        let (detail, continuation) = project_detail_event(
            &mut transaction,
            &event,
            cursor,
            limits.max_projected_bytes(),
        )
        .await?;
        let projected_body_bytes = detail.projected_body_bytes;
        transaction.commit().await?;
        Ok(Some(SessionTimelineDetailPage {
            session,
            items: vec![detail],
            projected_body_bytes,
            continuation,
        }))
    }

    /// Reads bounded details belonging to one exact turn.
    pub async fn read_turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, SessionTimelineRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2)",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if !exists {
            transaction.commit().await?;
            return Ok(None);
        }
        let addresses = fetch_turn_addresses(
            &mut transaction,
            session,
            turn,
            cursor.map(|cursor| cursor.address),
            limits.max_items(),
        )
        .await?;
        let page =
            project_address_page(&mut transaction, session, addresses, cursor, limits).await?;
        transaction.commit().await?;
        Ok(Some(page))
    }

    /// Reads one bounded inclusive contiguous address region.
    pub async fn read_region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, SessionTimelineRepositoryError> {
        if first > through
            || cursor.is_some_and(|cursor| cursor.address < first || cursor.address > through)
        {
            return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let session_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
                .bind(session.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
        if !session_exists {
            transaction.commit().await?;
            return Ok(None);
        }
        let addresses = fetch_region_addresses(
            &mut transaction,
            session,
            cursor.map_or(first, |cursor| cursor.address),
            through,
            limits.max_items(),
        )
        .await?;
        let page =
            project_address_page(&mut transaction, session, addresses, cursor, limits).await?;
        transaction.commit().await?;
        Ok(Some(page))
    }
}

impl SessionTimelineReader for SessionTimelineRepository {
    type Error = SessionTimelineRepositoryError;

    async fn read_descriptor(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionTimelineDescriptor>, Self::Error> {
        SessionTimelineRepository::read_descriptor(self, session).await
    }

    async fn read_window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> Result<Option<SessionTimelineWindow>, Self::Error> {
        SessionTimelineRepository::read_window(self, session, anchor, limits).await
    }

    async fn read_item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Self::Error> {
        SessionTimelineRepository::read_item_details(self, session, address, cursor, limits).await
    }

    async fn read_turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Self::Error> {
        SessionTimelineRepository::read_turn_details(self, session, turn, cursor, limits).await
    }

    async fn read_region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Self::Error> {
        SessionTimelineRepository::read_region_details(
            self, session, first, through, cursor, limits,
        )
        .await
    }
}

const DETAIL_ENVELOPE_BYTES: u32 = 128;

const TURN_DETAIL_ADDRESSES_SQL: &str = r#"
WITH turn_events AS (
    SELECT event_sequence FROM input_accepted_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM goal_turn_retired_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_activated_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_failed_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event.event_sequence
      FROM model_call_transition_outbox_event AS event
      JOIN model_call AS call ON call.model_call_id = event.model_call_id
     WHERE call.session_id = $1 AND call.turn_id = $2
    UNION ALL
    SELECT event.event_sequence
      FROM tool_batch_transition_outbox_event AS event
      JOIN model_call AS call
        ON call.model_call_id = event.producing_model_call_id
     WHERE call.session_id = $1 AND call.turn_id = $2
    UNION ALL
    SELECT event.event_sequence
      FROM tool_approval_decided_outbox_event AS event
      JOIN tool_request AS request ON request.request_id = event.request_id
     WHERE request.session_id = $1 AND request.turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_completed_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_refused_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_cancelled_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_reconciliation_required_outbox_event
     WHERE session_id = $1 AND turn_id = $2
)
SELECT event_sequence FROM turn_events
 WHERE event_sequence >= $3
 ORDER BY event_sequence ASC LIMIT $4
"#;

const REGION_DETAIL_ADDRESSES_SQL: &str = r#"
WITH session_events AS (
    SELECT event_sequence FROM outbox_event WHERE session_id = $1
    UNION ALL
    SELECT event_sequence FROM delegation_outbox_event WHERE session_id = $1
)
SELECT event_sequence FROM session_events
 WHERE event_sequence >= $2 AND event_sequence <= $3
 ORDER BY event_sequence ASC LIMIT $4
"#;

async fn fetch_turn_addresses(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    turn: TurnId,
    first: Option<TimelineAddress>,
    max_items: u16,
) -> Result<Vec<TimelineAddress>, SessionTimelineRepositoryError> {
    let first = first.map_or(1, |address| address.sequence().get());
    let rows: Vec<Decimal> = sqlx::query_scalar(TURN_DETAIL_ADDRESSES_SQL)
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .bind(Decimal::from(first))
        .bind(i64::from(max_items) + 1)
        .fetch_all(&mut **transaction)
        .await?;
    rows.into_iter()
        .map(|value| required_address(value, "turn detail address").map_err(Into::into))
        .collect()
}

async fn fetch_region_addresses(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    first: TimelineAddress,
    through: TimelineAddress,
    max_items: u16,
) -> Result<Vec<TimelineAddress>, SessionTimelineRepositoryError> {
    let rows: Vec<Decimal> = sqlx::query_scalar(REGION_DETAIL_ADDRESSES_SQL)
        .bind(session.into_uuid())
        .bind(Decimal::from(first.sequence().get()))
        .bind(Decimal::from(through.sequence().get()))
        .bind(i64::from(max_items) + 1)
        .fetch_all(&mut **transaction)
        .await?;
    rows.into_iter()
        .map(|value| required_address(value, "region detail address").map_err(Into::into))
        .collect()
}

async fn project_address_page(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    addresses: Vec<TimelineAddress>,
    cursor: Option<TimelineDetailCursor>,
    limits: TimelineDetailLimits,
) -> Result<SessionTimelineDetailPage, SessionTimelineRepositoryError> {
    if cursor.is_some_and(|cursor| addresses.first().copied() != Some(cursor.address)) {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    let item_limit = usize::from(limits.max_items());
    let next_by_count = addresses.get(item_limit).copied();
    let mut remaining = limits.max_projected_bytes();
    let mut items = Vec::with_capacity(addresses.len().min(item_limit));
    let mut continuation = None;
    for address in addresses.into_iter().take(item_limit) {
        if remaining < DETAIL_ENVELOPE_BYTES {
            continuation = Some(TimelineDetailContinuation::MoreAt(address));
            break;
        }
        let event = load_detail_event(transaction, session, address)
            .await?
            .ok_or(SessionTimelineCorruption::MissingDetailRecord)?;
        let event_cursor = cursor.filter(|cursor| cursor.address == address);
        let (detail, body_continuation) =
            project_detail_event(transaction, &event, event_cursor, remaining).await?;
        remaining = remaining
            .checked_sub(detail.projected_body_bytes)
            .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
        items.push(detail);
        if body_continuation.is_some() {
            continuation = body_continuation;
            break;
        }
    }
    if continuation.is_none() {
        continuation = next_by_count.map(TimelineDetailContinuation::MoreAt);
    }
    let projected_body_bytes = limits
        .max_projected_bytes()
        .checked_sub(remaining)
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    Ok(SessionTimelineDetailPage {
        session,
        items,
        projected_body_bytes,
        continuation,
    })
}

async fn load_detail_event(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    address: TimelineAddress,
) -> Result<Option<DispatchedOutboxEvent>, SessionTimelineRepositoryError> {
    let (_, event_beyond_allocated, event) =
        crate::outbox::load_event(transaction, address.sequence().get()).await?;
    if event_beyond_allocated {
        return Err(SessionTimelineCorruption::MissingDetailRecord.into());
    }
    Ok(event.filter(|event| event.session() == session))
}

async fn project_detail_event(
    transaction: &mut Transaction<'_, Postgres>,
    event: &DispatchedOutboxEvent,
    cursor: Option<TimelineDetailCursor>,
    max_bytes: u32,
) -> Result<
    (SessionTimelineDetail, Option<TimelineDetailContinuation>),
    SessionTimelineRepositoryError,
> {
    if max_bytes < DETAIL_ENVELOPE_BYTES {
        return Err(SessionTimelineCorruption::DetailProjectionOverflow.into());
    }
    let address = TimelineAddress::new(
        NonZeroU64::new(event.sequence())
            .ok_or(SessionTimelineCorruption::InvalidOrdinal("detail address"))?,
    );
    if cursor.is_some_and(|cursor| cursor.address != address) {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    let kind = dispatched_event_kind(event.kind());
    let mut remaining = max_bytes - DETAIL_ENVELOPE_BYTES;
    let (body, body_continuation) = match event.kind() {
        DispatchedOutboxEventKind::SessionCreated => {
            require_no_body_cursor(cursor)?;
            let imported_evidence = load_imported_evidence(transaction, event.session()).await?;
            (
                SessionTimelineDetailBody::SessionCreated { imported_evidence },
                None,
            )
        }
        DispatchedOutboxEventKind::SessionModelSettingsChanged(_) => {
            require_no_body_cursor(cursor)?;
            (
                SessionTimelineDetailBody::ModelSettings {
                    turn_id: None,
                    cause_code: String::from("session_defaults_changed"),
                },
                None,
            )
        }
        DispatchedOutboxEventKind::TurnModelSettingsResolved(settings) => {
            require_no_body_cursor(cursor)?;
            (
                SessionTimelineDetailBody::ModelSettings {
                    turn_id: Some(settings.turn().into_uuid().to_string()),
                    cause_code: String::from("turn_settings_resolved"),
                },
                None,
            )
        }
        DispatchedOutboxEventKind::InputAccepted { turn, content, .. } => {
            require_cursor_field(cursor, TimelineBodyField::InputText, 0)?;
            let text = excerpt_text(
                content,
                address,
                TimelineBodyField::InputText,
                0,
                cursor.map_or(0, |cursor| cursor.offset_bytes),
                &mut remaining,
            )?;
            let continuation = text.continuation.map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineDetailBody::UserInput {
                    turn_id: turn.into_uuid().to_string(),
                    text,
                    attachments: Vec::new(),
                },
                continuation,
            )
        }
        DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
            require_cursor_field(cursor, TimelineBodyField::ModelResponse, 0)?;
            let row = load_model_detail(transaction, *call).await?;
            let response = match row.response {
                Some(response) => Some(excerpt_text(
                    &response,
                    address,
                    TimelineBodyField::ModelResponse,
                    0,
                    cursor.map_or(0, |cursor| cursor.offset_bytes),
                    &mut remaining,
                )?),
                None if cursor.is_none() => None,
                None => return Err(SessionTimelineCorruption::InvalidDetailCursor.into()),
            };
            let continuation = response
                .as_ref()
                .and_then(|response| response.continuation)
                .map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineDetailBody::ModelCall {
                    turn_id: turn.into_uuid().to_string(),
                    model_call_id: call.into_uuid().to_string(),
                    state: model_call_state(*state),
                    model_identity_id: row.model_identity_id,
                    request_context_items: row.request_context_items,
                    response,
                    usage: row.usage,
                    cause_code: row.cause_code,
                },
                continuation,
            )
        }
        DispatchedOutboxEventKind::GoalTurnRetired { turn } => {
            let event =
                load_goal_turn_event(transaction, *turn, address, cursor, &mut remaining).await?;
            let continuation = event
                .text
                .as_ref()
                .and_then(|text| text.continuation)
                .map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineDetailBody::GoalEvent {
                    turn_id: turn.into_uuid().to_string(),
                    event,
                },
                continuation,
            )
        }
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call,
            state,
        } => {
            project_tool_batch(
                transaction,
                address,
                *turn,
                *producing_call,
                *state,
                cursor,
                &mut remaining,
            )
            .await?
        }
        DispatchedOutboxEventKind::ToolApprovalDecided {
            turn,
            approval,
            decider,
        } => {
            project_tool_approval(
                transaction,
                address,
                *turn,
                approval,
                decider,
                cursor,
                &mut remaining,
            )
            .await?
        }
        DispatchedOutboxEventKind::ContextCompacted {
            compaction,
            call,
            through_position,
            summary_entry,
            result_frontier,
        } => {
            require_cursor_field(cursor, TimelineBodyField::CompactionSummary, 0)?;
            let summary_text: String = sqlx::query_scalar(
                "SELECT context_summary_value FROM semantic_transcript_entry
                  WHERE semantic_entry_id = $1 AND payload_kind = 'context_summary'",
            )
            .bind(summary_entry.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
            let summary = excerpt_text(
                &summary_text,
                address,
                TimelineBodyField::CompactionSummary,
                0,
                cursor.map_or(0, |cursor| cursor.offset_bytes),
                &mut remaining,
            )?;
            let continuation = summary
                .continuation
                .map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineDetailBody::ContextCompaction {
                    compaction_id: compaction.into_uuid().to_string(),
                    model_call_id: call.into_uuid().to_string(),
                    through_position: *through_position,
                    summary_entry_id: summary_entry.into_uuid().to_string(),
                    result_frontier_id: result_frontier.into_uuid().to_string(),
                    summary,
                },
                continuation,
            )
        }
        DispatchedOutboxEventKind::TurnActivated { turn, .. } => {
            require_no_body_cursor(cursor)?;
            (
                SessionTimelineDetailBody::TurnLifecycle {
                    turn_id: turn.into_uuid().to_string(),
                    lifecycle: TimelineTurnLifecycleKind::Activated,
                    cause_code: String::from("activated"),
                },
                None,
            )
        }
        DispatchedOutboxEventKind::TurnFailed { turn, .. } => {
            terminal_turn_body(*turn, "failed", cursor)?
        }
        DispatchedOutboxEventKind::TurnCompleted { turn, .. } => {
            terminal_turn_body(*turn, "completed", cursor)?
        }
        DispatchedOutboxEventKind::TurnRefused { turn, .. } => {
            terminal_turn_body(*turn, "refused", cursor)?
        }
        DispatchedOutboxEventKind::TurnCancelled { turn, .. } => {
            terminal_turn_body(*turn, "cancelled", cursor)?
        }
        DispatchedOutboxEventKind::TurnReconciliationRequired {
            turn, operation, ..
        } => {
            require_no_body_cursor(cursor)?;
            let (operation_kind, operation_id) = reconciliation_operation(*operation);
            let attempt_count = load_turn_attempt_count(transaction, *turn).await?;
            (
                SessionTimelineDetailBody::Reconciliation {
                    turn_id: turn.into_uuid().to_string(),
                    operation_kind,
                    operation_id,
                    attempt_count,
                    exhausted: true,
                    operator_required: true,
                    cause_code: String::from("ambiguous_operation"),
                },
                None,
            )
        }
        DispatchedOutboxEventKind::RunnerStateTransition {
            runner,
            placement_revision,
            sandbox,
            working_directory,
            state,
        } => {
            require_no_body_cursor(cursor)?;
            (
                SessionTimelineDetailBody::Runner {
                    runner_id: runner.into_uuid().to_string(),
                    placement_revision: placement_revision.get(),
                    sandbox_posture: runner_sandbox(*sandbox),
                    working_directory: working_directory
                        .as_ref()
                        .map(|directory| directory.as_str().to_owned()),
                    state: runner_state(*state),
                },
                None,
            )
        }
        DispatchedOutboxEventKind::DelegationUpdate(update) => {
            project_delegation_update(address, update, cursor, &mut remaining)?
        }
        DispatchedOutboxEventKind::DelegationWake(wake) => {
            require_no_body_cursor(cursor)?;
            (delegation_wake_body(*wake), None)
        }
    };
    let projected_body_bytes = max_bytes
        .checked_sub(remaining)
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    Ok((
        SessionTimelineDetail {
            address,
            kind,
            body,
            projected_body_bytes,
        },
        body_continuation,
    ))
}

fn terminal_turn_body(
    turn: TurnId,
    cause_code: &str,
    cursor: Option<TimelineDetailCursor>,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    require_no_body_cursor(cursor)?;
    Ok((
        SessionTimelineDetailBody::TurnLifecycle {
            turn_id: turn.into_uuid().to_string(),
            lifecycle: TimelineTurnLifecycleKind::Terminalized,
            cause_code: cause_code.to_owned(),
        },
        None,
    ))
}

async fn load_imported_evidence(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
) -> Result<Option<TimelineImportedEvidence>, SessionTimelineRepositoryError> {
    let row = sqlx::query(
        "SELECT imported_frontier_entry_id, imported_frontier_position
           FROM session WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    let entry = row.try_get::<Option<uuid::Uuid>, _>("imported_frontier_entry_id")?;
    let position = row.try_get::<Option<Decimal>, _>("imported_frontier_position")?;
    match (entry, position) {
        (None, None) => Ok(None),
        (Some(entry), Some(position)) => Ok(Some(TimelineImportedEvidence {
            imported_entry_id: entry.to_string(),
            imported_position: nonnegative(position, "imported frontier position")?,
        })),
        _ => Err(SessionTimelineCorruption::Missing("imported frontier evidence").into()),
    }
}

async fn load_goal_turn_event(
    transaction: &mut Transaction<'_, Postgres>,
    turn: TurnId,
    address: TimelineAddress,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<TimelineGoalEvent, SessionTimelineRepositoryError> {
    require_cursor_field(cursor, TimelineBodyField::GoalText, 0)?;
    let row = sqlx::query(
        "SELECT event.generation, event.event_kind, event.blocked_reason,
                COALESCE(event.statement, event.need, event.guidance, event.report) AS body
           FROM goal_turn AS goal
           JOIN goal_event AS event
             ON event.session_id = goal.session_id
            AND event.generation = goal.goal_generation
          WHERE goal.turn_id = $1
          ORDER BY event.event_ordinal DESC
          LIMIT 1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    let text = row
        .try_get::<Option<String>, _>("body")?
        .map(|text| {
            excerpt_text(
                &text,
                address,
                TimelineBodyField::GoalText,
                0,
                cursor.map_or(0, |cursor| cursor.offset_bytes),
                remaining,
            )
        })
        .transpose()?;
    if text.is_none() && cursor.is_some() {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    Ok(TimelineGoalEvent {
        generation: nonnegative(row.try_get("generation")?, "goal generation")?,
        event_kind: row.try_get("event_kind")?,
        reason: row.try_get("blocked_reason")?,
        text,
    })
}

async fn project_tool_batch(
    transaction: &mut Transaction<'_, Postgres>,
    address: TimelineAddress,
    turn: TurnId,
    producing_call: ModelCallId,
    state: DispatchedToolBatchState,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let goal_rows = load_goal_event_rows(transaction, producing_call).await?;
    if let Some(goal_cursor) =
        cursor.filter(|cursor| cursor.field == Some(TimelineBodyField::GoalText))
    {
        return project_tool_goal(
            address,
            turn,
            producing_call,
            state,
            goal_cursor,
            goal_rows,
            remaining,
        );
    }
    let rows = sqlx::query(
        "SELECT request.request_id, request.tool_name, request.arguments_text,
                request.approval_posture, attempt.attempt_id,
                attempt.effect_class, attempt.state_kind,
                attempt.terminal_disposition_kind, attempt.result_text,
                attempt.error_kind, attempt.error_detail,
                (
                    SELECT CASE placement.requested_sandbox_profile
                        WHEN 'ambient' THEN 'unsandboxed'
                        WHEN 'workspace_restricted' THEN 'sandboxed'
                    END
                      FROM runner_tool_request_lease_binding AS binding
                      JOIN runner_lease_generation AS lease
                        ON lease.lease_id = binding.lease_id
                      JOIN runner_session_placement_record AS placement
                        ON placement.session_id = lease.session_id
                       AND placement.event_ordinal = lease.placement_event_ordinal
                     WHERE binding.request_id = request.request_id
                     ORDER BY lease.generation DESC
                     LIMIT 1
                ) AS sandbox_posture,
                EXISTS (
                    SELECT 1 FROM tool_approval_judge_model_call AS judge
                     WHERE judge.request_id = request.request_id
                       AND judge.recommendation_kind = 'escalate_to_human'
                ) AS judge_escalated
           FROM tool_request AS request
           LEFT JOIN tool_attempt AS attempt ON attempt.request_id = request.request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    if rows.is_empty() {
        require_no_body_cursor(cursor)?;
    }
    let member_index = cursor.map_or(0, |cursor| cursor.member_index);
    let index = usize::try_from(member_index)
        .map_err(|_| SessionTimelineCorruption::InvalidDetailCursor)?;
    let mut tools = Vec::new();
    let mut continuation = None;
    if let Some(row) = rows.get(index) {
        let requested_field = cursor
            .and_then(|cursor| cursor.field)
            .unwrap_or(TimelineBodyField::ToolArguments);
        if !matches!(
            requested_field,
            TimelineBodyField::ToolArguments
                | TimelineBodyField::ToolResult
                | TimelineBodyField::ToolFailure
        ) {
            return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
        }
        let arguments_text: String = row.try_get("arguments_text")?;
        let result_text: Option<String> = row.try_get("result_text")?;
        let failure_text: Option<String> = row.try_get("error_detail")?;
        let selected = match requested_field {
            TimelineBodyField::ToolArguments => Some(&arguments_text),
            TimelineBodyField::ToolResult => result_text.as_ref(),
            TimelineBodyField::ToolFailure => failure_text.as_ref(),
            _ => None,
        }
        .ok_or(SessionTimelineCorruption::InvalidDetailCursor)?;
        let excerpt = excerpt_text(
            selected,
            address,
            requested_field,
            member_index,
            cursor.map_or(0, |cursor| cursor.offset_bytes),
            remaining,
        )?;
        continuation = excerpt
            .continuation
            .map(TimelineDetailContinuation::MoreBody)
            .or_else(|| {
                next_tool_field(
                    address,
                    requested_field,
                    member_index,
                    result_text.is_some(),
                    failure_text.is_some(),
                    index + 1 < rows.len(),
                )
            });
        if continuation.is_none() && index + 1 == rows.len() && !goal_rows.is_empty() {
            continuation = Some(TimelineDetailContinuation::MoreBody(
                TimelineBodyContinuation {
                    address,
                    field: TimelineBodyField::GoalText,
                    member_index: 0,
                    offset_bytes: 0,
                },
            ));
        }
        let state_kind: Option<String> = row.try_get("state_kind")?;
        let disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let approval_posture: String = row.try_get("approval_posture")?;
        let approval_judge_escalated: bool = row.try_get("judge_escalated")?;
        tools.push(TimelineToolAttempt {
            request_id: row.try_get::<uuid::Uuid, _>("request_id")?.to_string(),
            attempt_id: row
                .try_get::<Option<uuid::Uuid>, _>("attempt_id")?
                .map(|id| id.to_string()),
            tool_name: row.try_get("tool_name")?,
            arguments: (requested_field == TimelineBodyField::ToolArguments)
                .then_some(excerpt.clone()),
            result: (requested_field == TimelineBodyField::ToolResult).then_some(excerpt.clone()),
            failure: (requested_field == TimelineBodyField::ToolFailure).then_some(excerpt),
            operator_required: approval_judge_escalated || approval_posture == "human",
            approval_posture,
            approval_judge_escalated,
            effect_posture: row.try_get("effect_class")?,
            sandbox_posture: row.try_get("sandbox_posture")?,
            state: tool_state(state_kind.as_deref(), disposition.as_deref()),
            cause_code: row.try_get("error_kind")?,
        });
    } else if !rows.is_empty() {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    Ok((
        SessionTimelineDetailBody::ToolBatch {
            turn_id: turn.into_uuid().to_string(),
            producing_model_call_id: producing_call.into_uuid().to_string(),
            state: tool_batch_state(state),
            tools,
            goal_events: Vec::new(),
        },
        continuation,
    ))
}

fn next_tool_field(
    address: TimelineAddress,
    field: TimelineBodyField,
    member_index: u32,
    has_result: bool,
    has_failure: bool,
    has_next: bool,
) -> Option<TimelineDetailContinuation> {
    let (field, member_index) = match field {
        TimelineBodyField::ToolArguments if has_result => {
            (TimelineBodyField::ToolResult, member_index)
        }
        TimelineBodyField::ToolArguments if has_failure => {
            (TimelineBodyField::ToolFailure, member_index)
        }
        TimelineBodyField::ToolArguments
        | TimelineBodyField::ToolResult
        | TimelineBodyField::ToolFailure
            if has_next =>
        {
            (TimelineBodyField::ToolArguments, member_index + 1)
        }
        _ => return None,
    };
    Some(TimelineDetailContinuation::MoreBody(
        TimelineBodyContinuation {
            address,
            field,
            member_index,
            offset_bytes: 0,
        },
    ))
}

fn tool_state(state: Option<&str>, disposition: Option<&str>) -> Option<TimelineToolState> {
    match (state, disposition) {
        (Some("prepared"), None) => Some(TimelineToolState::Prepared),
        (Some("in_flight"), None) => Some(TimelineToolState::InFlight),
        (Some("terminal"), Some("completed")) => Some(TimelineToolState::Completed),
        (Some("terminal"), Some("known_failed")) => Some(TimelineToolState::KnownFailed),
        (Some("terminal"), Some("ambiguous")) => Some(TimelineToolState::Ambiguous),
        _ => None,
    }
}

fn tool_batch_state(state: DispatchedToolBatchState) -> String {
    match state {
        DispatchedToolBatchState::Proposed { .. } => String::from("proposed"),
        DispatchedToolBatchState::ResultsProjected { .. } => String::from("results_projected"),
        DispatchedToolBatchState::RecoveryRequired { .. } => String::from("recovery_required"),
    }
}

#[derive(Debug)]
struct StoredGoalEvent {
    generation: u64,
    event_kind: String,
    reason: Option<String>,
    text: Option<String>,
}

async fn load_goal_event_rows(
    transaction: &mut Transaction<'_, Postgres>,
    call: ModelCallId,
) -> Result<Vec<StoredGoalEvent>, SessionTimelineRepositoryError> {
    let rows = sqlx::query(
        "SELECT event.generation, event.event_kind, event.blocked_reason,
                COALESCE(event.statement, event.need, event.guidance, event.report) AS body
           FROM goal_event AS event
           JOIN tool_request AS request
             ON request.request_id = event.model_tool_request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY event.event_ordinal",
    )
    .bind(call.into_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(StoredGoalEvent {
                generation: nonnegative(row.try_get("generation")?, "goal generation")?,
                event_kind: row.try_get("event_kind")?,
                reason: row.try_get("blocked_reason")?,
                text: row.try_get("body")?,
            })
        })
        .collect()
}

fn project_tool_goal(
    address: TimelineAddress,
    turn: TurnId,
    producing_call: ModelCallId,
    state: DispatchedToolBatchState,
    cursor: TimelineDetailCursor,
    goal_rows: Vec<StoredGoalEvent>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let index = usize::try_from(cursor.member_index)
        .map_err(|_| SessionTimelineCorruption::InvalidDetailCursor)?;
    let row = goal_rows
        .get(index)
        .ok_or(SessionTimelineCorruption::InvalidDetailCursor)?;
    let raw_text = row
        .text
        .as_deref()
        .ok_or(SessionTimelineCorruption::InvalidDetailCursor)?;
    let text = excerpt_text(
        raw_text,
        address,
        TimelineBodyField::GoalText,
        cursor.member_index,
        cursor.offset_bytes,
        remaining,
    )?;
    let continuation = text
        .continuation
        .map(TimelineDetailContinuation::MoreBody)
        .or_else(|| {
            (index + 1 < goal_rows.len()).then_some(TimelineDetailContinuation::MoreBody(
                TimelineBodyContinuation {
                    address,
                    field: TimelineBodyField::GoalText,
                    member_index: cursor.member_index + 1,
                    offset_bytes: 0,
                },
            ))
        });
    Ok((
        SessionTimelineDetailBody::ToolBatch {
            turn_id: turn.into_uuid().to_string(),
            producing_model_call_id: producing_call.into_uuid().to_string(),
            state: tool_batch_state(state),
            tools: Vec::new(),
            goal_events: vec![TimelineGoalEvent {
                generation: row.generation,
                event_kind: row.event_kind.clone(),
                reason: row.reason.clone(),
                text: Some(text),
            }],
        },
        continuation,
    ))
}

async fn project_tool_approval(
    transaction: &mut Transaction<'_, Postgres>,
    address: TimelineAddress,
    turn: TurnId,
    approval: &ToolApprovalResolution,
    _decider: &ToolApprovalDecider,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let tool_name: String =
        sqlx::query_scalar("SELECT tool_name FROM tool_request WHERE request_id = $1")
            .bind(approval.request().into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
    let rationale = approval
        .rationale()
        .map(|rationale| rationale.as_str())
        .or_else(|| match approval.decision() {
            ToolApprovalDecision::Deny {
                reason: Some(reason),
            } => Some(reason.as_str()),
            ToolApprovalDecision::Approve | ToolApprovalDecision::Deny { reason: None } => None,
        });
    match rationale {
        Some(_) => require_cursor_field(cursor, TimelineBodyField::ApprovalRationale, 0)?,
        None => require_no_body_cursor(cursor)?,
    }
    let rationale = rationale
        .map(|rationale| {
            excerpt_text(
                rationale,
                address,
                TimelineBodyField::ApprovalRationale,
                0,
                cursor.map_or(0, |cursor| cursor.offset_bytes),
                remaining,
            )
        })
        .transpose()?;
    let continuation = rationale
        .as_ref()
        .and_then(|rationale| rationale.continuation)
        .map(TimelineDetailContinuation::MoreBody);
    let decision = match approval.decision() {
        ToolApprovalDecision::Approve => String::from("approve"),
        ToolApprovalDecision::Deny { .. } => String::from("deny"),
    };
    let source = match approval.source() {
        ToolDecisionSource::PolicyAuto
        | ToolDecisionSource::SessionBlanket
        | ToolDecisionSource::SessionOverride => TimelineApprovalSource::Policy,
        ToolDecisionSource::Delegate => TimelineApprovalSource::Delegate,
        ToolDecisionSource::UserCommand => TimelineApprovalSource::User,
    };
    Ok((
        SessionTimelineDetailBody::ToolApprovalDecision {
            turn_id: turn.into_uuid().to_string(),
            request_id: approval.request().into_uuid().to_string(),
            tool_name,
            decision,
            source,
            rationale,
            approval_judge_escalated: false,
        },
        continuation,
    ))
}

async fn load_turn_attempt_count(
    transaction: &mut Transaction<'_, Postgres>,
    turn: TurnId,
) -> Result<u64, SessionTimelineRepositoryError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
    u64::try_from(count)
        .map_err(|_| SessionTimelineCorruption::InvalidOrdinal("turn attempt count").into())
}

fn reconciliation_operation(operation: DispatchedReconciliationOperation) -> (String, String) {
    match operation {
        DispatchedReconciliationOperation::ModelCall(call) => {
            (String::from("model_call"), call.into_uuid().to_string())
        }
        DispatchedReconciliationOperation::ToolAttempt(attempt) => (
            String::from("tool_attempt"),
            attempt.into_uuid().to_string(),
        ),
    }
}

fn runner_sandbox(sandbox: RunnerSandboxProfile) -> String {
    match sandbox {
        RunnerSandboxProfile::Ambient => String::from("unsandboxed"),
        RunnerSandboxProfile::WorkspaceRestricted => String::from("sandboxed"),
    }
}

fn runner_state(state: DispatchedRunnerState) -> String {
    match state {
        DispatchedRunnerState::Pinned => String::from("pinned"),
        DispatchedRunnerState::Suspect => String::from("suspect"),
        DispatchedRunnerState::Connected => String::from("connected"),
        DispatchedRunnerState::RunnerLostBeforePin => String::from("runner_lost_before_pin"),
        DispatchedRunnerState::RunnerLost => String::from("runner_lost"),
        DispatchedRunnerState::Replaced => String::from("replaced"),
        DispatchedRunnerState::WorkingDirectoryChanged => String::from("working_directory_changed"),
        DispatchedRunnerState::Abandoned => String::from("abandoned"),
    }
}

fn project_delegation_update(
    address: TimelineAddress,
    update: &DispatchedDelegationUpdate,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let (event_kind, relationship_id, subject_id, outcome, reason, raw_content) = match update {
        DispatchedDelegationUpdate::ChildSpawned {
            spawning_request,
            child,
            ..
        } => (
            "child_spawned",
            spawning_request.into_uuid().to_string(),
            Some(child.into_uuid().to_string()),
            None,
            None,
            None,
        ),
        DispatchedDelegationUpdate::ChildWaiting {
            spawning_request,
            awaiting_request,
            ..
        } => (
            "child_waiting",
            spawning_request.into_uuid().to_string(),
            Some(awaiting_request.into_uuid().to_string()),
            None,
            None,
            None,
        ),
        DispatchedDelegationUpdate::ChildLifecycleDisposition {
            spawning_request,
            child,
            outcome,
            reason,
            ..
        } => (
            "child_lifecycle_disposition",
            spawning_request.into_uuid().to_string(),
            Some(child.into_uuid().to_string()),
            Some(delegation_outcome(*outcome)),
            Some(delegation_reason(*reason)),
            None,
        ),
        DispatchedDelegationUpdate::ChildResult {
            spawning_request,
            child,
            outcome,
            reason,
            content,
            ..
        } => (
            "child_result",
            spawning_request.into_uuid().to_string(),
            Some(child.into_uuid().to_string()),
            Some(delegation_outcome(*outcome)),
            Some(delegation_reason(*reason)),
            content.as_deref(),
        ),
        DispatchedDelegationUpdate::SessionMessage {
            spawning_request,
            message,
            content,
            ..
        } => (
            "session_message",
            spawning_request.into_uuid().to_string(),
            Some(message.into_uuid().to_string()),
            None,
            None,
            Some(content.as_str()),
        ),
    };
    let content = match raw_content {
        Some(content) => {
            require_cursor_field(cursor, TimelineBodyField::DelegationContent, 0)?;
            Some(excerpt_text(
                content,
                address,
                TimelineBodyField::DelegationContent,
                0,
                cursor.map_or(0, |cursor| cursor.offset_bytes),
                remaining,
            )?)
        }
        None => {
            require_no_body_cursor(cursor)?;
            None
        }
    };
    let continuation = content
        .as_ref()
        .and_then(|content| content.continuation)
        .map(TimelineDetailContinuation::MoreBody);
    Ok((
        SessionTimelineDetailBody::Delegation {
            event_kind: event_kind.to_owned(),
            relationship_id,
            subject_id,
            outcome,
            reason,
            content,
        },
        continuation,
    ))
}

fn delegation_wake_body(wake: DispatchedDelegationWake) -> SessionTimelineDetailBody {
    let (event_kind, relationship_id, subject_id) = match wake {
        DispatchedDelegationWake::Result {
            spawning_request,
            awaiting_request,
        } => (
            "result_wake",
            spawning_request.into_uuid().to_string(),
            awaiting_request.map(|request| request.into_uuid().to_string()),
        ),
        DispatchedDelegationWake::Message {
            spawning_request,
            message,
        } => (
            "message_wake",
            spawning_request.into_uuid().to_string(),
            Some(message.into_uuid().to_string()),
        ),
    };
    SessionTimelineDetailBody::Delegation {
        event_kind: event_kind.to_owned(),
        relationship_id,
        subject_id,
        outcome: None,
        reason: None,
        content: None,
    }
}

fn delegation_outcome(outcome: DispatchedDelegationOutcome) -> String {
    match outcome {
        DispatchedDelegationOutcome::ResultReturned => String::from("result_returned"),
        DispatchedDelegationOutcome::ChildFailed => String::from("child_failed"),
        DispatchedDelegationOutcome::ChildStopped => String::from("child_stopped"),
        DispatchedDelegationOutcome::ChildCancelled => String::from("child_cancelled"),
        DispatchedDelegationOutcome::ContinueRunning => String::from("continue_running"),
        DispatchedDelegationOutcome::AlreadyTerminal => String::from("already_terminal"),
    }
}

fn delegation_reason(reason: DispatchedDelegationReason) -> String {
    match reason {
        DispatchedDelegationReason::ChildCompleted => String::from("child_completed"),
        DispatchedDelegationReason::ChildExecutionFailed => String::from("child_execution_failed"),
        DispatchedDelegationReason::ChildResultUnavailable => {
            String::from("child_result_unavailable")
        }
        DispatchedDelegationReason::ChildCancelled => String::from("child_cancelled"),
        DispatchedDelegationReason::ParentStoppedWithDescendants => {
            String::from("parent_stopped_with_descendants")
        }
        DispatchedDelegationReason::ParentCancelledWithDescendants => {
            String::from("parent_cancelled_with_descendants")
        }
    }
}

fn require_no_body_cursor(
    cursor: Option<TimelineDetailCursor>,
) -> Result<(), SessionTimelineRepositoryError> {
    if cursor.is_some_and(|cursor| {
        cursor.field.is_some() || cursor.member_index != 0 || cursor.offset_bytes != 0
    }) {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    Ok(())
}

fn require_cursor_field(
    cursor: Option<TimelineDetailCursor>,
    field: TimelineBodyField,
    member_index: u32,
) -> Result<(), SessionTimelineRepositoryError> {
    if let Some(cursor) = cursor {
        let item_start =
            cursor.field.is_none() && cursor.member_index == 0 && cursor.offset_bytes == 0;
        if !item_start && (cursor.field != Some(field) || cursor.member_index != member_index) {
            return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
        }
    }
    Ok(())
}

fn excerpt_text(
    value: &str,
    address: TimelineAddress,
    field: TimelineBodyField,
    member_index: u32,
    offset_bytes: u64,
    remaining: &mut u32,
) -> Result<TimelineTextExcerpt, SessionTimelineRepositoryError> {
    let start = usize::try_from(offset_bytes)
        .map_err(|_| SessionTimelineCorruption::InvalidDetailCursor)?;
    if start > value.len() || !value.is_char_boundary(start) {
        return Err(SessionTimelineCorruption::InvalidDetailCursor.into());
    }
    let available = usize::try_from(*remaining)
        .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
    let mut end = start.saturating_add(available).min(value.len());
    while end > start && !value.is_char_boundary(end) {
        end -= 1;
    }
    let text = value[start..end].to_owned();
    let charged = u32::try_from(text.len())
        .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
    *remaining = remaining
        .checked_sub(charged)
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    let continuation = (end < value.len()).then_some(TimelineBodyContinuation {
        address,
        field,
        member_index,
        offset_bytes: u64::try_from(end)
            .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?,
    });
    Ok(TimelineTextExcerpt {
        text,
        offset_bytes,
        total_bytes: u64::try_from(value.len())
            .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?,
        continuation,
    })
}

struct ModelDetailRow {
    model_identity_id: String,
    request_context_items: u64,
    response: Option<String>,
    usage: TimelineModelUsage,
    cause_code: Option<String>,
}

async fn load_model_detail(
    transaction: &mut Transaction<'_, Postgres>,
    call: signalbox_domain::ModelCallId,
) -> Result<ModelDetailRow, SessionTimelineRepositoryError> {
    let row = sqlx::query(
        r#"
SELECT call.resolved_provider_model_identity_id,
       frontier.member_count,
       call.usage_input_tokens,
       call.usage_output_tokens,
       call.usage_cache_creation_input_tokens,
       call.usage_cache_read_input_tokens,
       call.terminal_provider_failure_cause,
       (
           SELECT string_agg(part.assistant_text_value, '' ORDER BY part.member_position)
             FROM (
                 SELECT entry.semantic_entry_id, entry.assistant_text_value,
                        min(member.member_position) AS member_position
                   FROM semantic_transcript_entry AS entry
                   JOIN context_frontier_member AS member
                     ON member.source_session_id = entry.source_session_id
                    AND member.semantic_entry_id = entry.semantic_entry_id
                  WHERE entry.producing_model_call_id = call.model_call_id
                    AND entry.payload_kind = 'assistant_text'
                  GROUP BY entry.semantic_entry_id, entry.assistant_text_value
             ) AS part
       ) AS response_text
  FROM model_call AS call
  JOIN context_frontier AS frontier
    ON frontier.owning_session_id = call.session_id
   AND frontier.context_frontier_id = call.context_frontier_id
 WHERE call.model_call_id = $1
"#,
    )
    .bind(call.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(SessionTimelineCorruption::MissingDetailRecord)?;
    Ok(ModelDetailRow {
        model_identity_id: row
            .try_get::<uuid::Uuid, _>("resolved_provider_model_identity_id")?
            .to_string(),
        request_context_items: nonnegative(row.try_get("member_count")?, "context item count")?,
        response: row.try_get("response_text")?,
        usage: TimelineModelUsage {
            input_tokens: optional_nonnegative(row.try_get("usage_input_tokens")?, "input usage")?,
            output_tokens: optional_nonnegative(
                row.try_get("usage_output_tokens")?,
                "output usage",
            )?,
            cache_creation_input_tokens: optional_nonnegative(
                row.try_get("usage_cache_creation_input_tokens")?,
                "cache creation usage",
            )?,
            cache_read_input_tokens: optional_nonnegative(
                row.try_get("usage_cache_read_input_tokens")?,
                "cache read usage",
            )?,
        },
        cause_code: row.try_get("terminal_provider_failure_cause")?,
    })
}

fn dispatched_event_kind(kind: &DispatchedOutboxEventKind) -> SessionTimelineEventKind {
    match kind {
        DispatchedOutboxEventKind::SessionCreated => SessionTimelineEventKind::SessionCreated,
        DispatchedOutboxEventKind::SessionModelSettingsChanged(_) => {
            SessionTimelineEventKind::SessionModelSettingsChanged
        }
        DispatchedOutboxEventKind::TurnModelSettingsResolved(_) => {
            SessionTimelineEventKind::TurnModelSettingsResolved
        }
        DispatchedOutboxEventKind::InputAccepted { .. } => SessionTimelineEventKind::InputAccepted,
        DispatchedOutboxEventKind::GoalTurnRetired { .. } => {
            SessionTimelineEventKind::GoalTurnRetired
        }
        DispatchedOutboxEventKind::TurnActivated { .. } => SessionTimelineEventKind::TurnActivated,
        DispatchedOutboxEventKind::TurnFailed { .. } => SessionTimelineEventKind::TurnFailed,
        DispatchedOutboxEventKind::ModelCallTransition { .. } => {
            SessionTimelineEventKind::ModelCallTransition
        }
        DispatchedOutboxEventKind::ToolBatchTransition { .. } => {
            SessionTimelineEventKind::ToolBatchTransition
        }
        DispatchedOutboxEventKind::ToolApprovalDecided { .. } => {
            SessionTimelineEventKind::ToolApprovalDecided
        }
        DispatchedOutboxEventKind::ContextCompacted { .. } => {
            SessionTimelineEventKind::ContextCompacted
        }
        DispatchedOutboxEventKind::TurnCompleted { .. } => SessionTimelineEventKind::TurnCompleted,
        DispatchedOutboxEventKind::TurnRefused { .. } => SessionTimelineEventKind::TurnRefused,
        DispatchedOutboxEventKind::TurnCancelled { .. } => SessionTimelineEventKind::TurnCancelled,
        DispatchedOutboxEventKind::TurnReconciliationRequired { .. } => {
            SessionTimelineEventKind::TurnReconciliationRequired
        }
        DispatchedOutboxEventKind::RunnerStateTransition { .. } => {
            SessionTimelineEventKind::RunnerStateTransition
        }
        DispatchedOutboxEventKind::DelegationUpdate(_) => {
            SessionTimelineEventKind::DelegationUpdate
        }
        DispatchedOutboxEventKind::DelegationWake(_) => SessionTimelineEventKind::DelegationWake,
    }
}

const fn model_call_state(state: DispatchedModelCallState) -> TimelineModelCallState {
    match state {
        DispatchedModelCallState::Prepared => TimelineModelCallState::Prepared,
        DispatchedModelCallState::InFlight => TimelineModelCallState::InFlight,
        DispatchedModelCallState::CancellationRequested => {
            TimelineModelCallState::CancellationRequested
        }
        DispatchedModelCallState::Terminal(disposition) => {
            TimelineModelCallState::Terminal(match disposition {
                DispatchedModelCallDisposition::Completed => {
                    TimelineModelCallDisposition::Completed
                }
                DispatchedModelCallDisposition::KnownFailed => {
                    TimelineModelCallDisposition::KnownFailed
                }
                DispatchedModelCallDisposition::Refused => TimelineModelCallDisposition::Refused,
                DispatchedModelCallDisposition::Cancelled => {
                    TimelineModelCallDisposition::Cancelled
                }
                DispatchedModelCallDisposition::Ambiguous => {
                    TimelineModelCallDisposition::Ambiguous
                }
            })
        }
    }
}

fn optional_nonnegative(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<u64>, SessionTimelineCorruption> {
    value.map(|value| nonnegative(value, field)).transpose()
}

const DESCRIPTOR_SQL: &str = r#"
SELECT session.session_id, facts.session_id IS NOT NULL AS facts_present,
       facts.item_count, facts.first_sequence,
       facts.latest_sequence,
       facts.item_count * $2 + facts.event_kind_bytes AS structured_bytes,
       facts.projected_text_bytes AS text_bytes,
       facts.active_turn_count AS active_count, facts.queued_turn_count AS queued_count,
       (SELECT last_sequence FROM outbox_sequence_state WHERE singleton) AS last_sequence
  FROM session
  LEFT JOIN session_timeline_fact AS facts USING (session_id)
 WHERE session.session_id = $1
"#;

macro_rules! window_sql {
    ($tail:literal) => {
        concat!(
            "WITH session_events AS (",
            "SELECT event_sequence, event_kind FROM outbox_event WHERE session_id = $1 ",
            "UNION ALL ",
            "SELECT event_sequence, event_kind FROM delegation_outbox_event WHERE session_id = $1",
            ") ",
            $tail
        )
    };
}

const FIRST_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind FROM session_events ORDER BY event_sequence ASC LIMIT $2"
);
const LATEST_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind FROM session_events ORDER BY event_sequence DESC LIMIT $2"
);
const BEFORE_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind FROM session_events WHERE event_sequence < $2 ORDER BY event_sequence DESC LIMIT $3"
);
const AFTER_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind FROM session_events WHERE event_sequence > $2 ORDER BY event_sequence ASC LIMIT $3"
);
const AROUND_WINDOW_SQL: &str = r#"
WITH before_candidates AS (
    (SELECT event_sequence, event_kind FROM outbox_event
      WHERE session_id = $1 AND event_sequence <= $2
      ORDER BY event_sequence DESC LIMIT $3)
    UNION ALL
    (SELECT event_sequence, event_kind FROM delegation_outbox_event
      WHERE session_id = $1 AND event_sequence <= $2
      ORDER BY event_sequence DESC LIMIT $3)
), before_events AS (
    SELECT event_sequence, event_kind FROM before_candidates
     ORDER BY event_sequence DESC LIMIT $3
), after_candidates AS (
    (SELECT event_sequence, event_kind FROM outbox_event
      WHERE session_id = $1 AND event_sequence > $2
      ORDER BY event_sequence ASC LIMIT $3)
    UNION ALL
    (SELECT event_sequence, event_kind FROM delegation_outbox_event
      WHERE session_id = $1 AND event_sequence > $2
      ORDER BY event_sequence ASC LIMIT $3)
), after_events AS (
    SELECT event_sequence, event_kind FROM after_candidates
     ORDER BY event_sequence ASC LIMIT $3
), candidates AS (
    SELECT event_sequence, event_kind FROM before_events
    UNION ALL
    SELECT event_sequence, event_kind FROM after_events
)
SELECT event_sequence, event_kind FROM candidates
 ORDER BY abs(event_sequence - $2), event_sequence ASC LIMIT $3
"#;

async fn load_descriptor(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<SessionTimelineDescriptor>, SessionTimelineRepositoryError> {
    let row = sqlx::query(DESCRIPTOR_SQL)
        .bind(session.into_uuid())
        .bind(i64::from(PROJECTED_ITEM_ENVELOPE_BYTES))
        .fetch_optional(connection)
        .await?;
    let Some(row) = row else { return Ok(None) };
    if !row.try_get::<bool, _>("facts_present")? {
        return Err(SessionTimelineCorruption::Missing("projection facts").into());
    }
    let first = optional_address(row.try_get("first_sequence")?, "first address")?;
    let latest = optional_address(row.try_get("latest_sequence")?, "latest address")?;
    Ok(Some(SessionTimelineDescriptor {
        session,
        sizes: SessionTimelineSizeFacts {
            item_count: nonnegative(row.try_get("item_count")?, "item count")?,
            projected_text_bytes: nonnegative(row.try_get("text_bytes")?, "projected text bytes")?,
            projected_structured_bytes: nonnegative(
                row.try_get("structured_bytes")?,
                "projected structured bytes",
            )?,
            referenced_blob_count: 0,
            referenced_blob_bytes: 0,
        },
        bounds: SessionTimelineBounds { first, latest },
        work: SessionWorkFacts {
            active_turn_count: nonnegative(row.try_get("active_count")?, "active turn count")?,
            queued_turn_count: nonnegative(row.try_get("queued_count")?, "queued turn count")?,
        },
        observed_through: nonnegative(
            row.try_get::<Option<Decimal>, _>("last_sequence")?
                .ok_or(SessionTimelineCorruption::Missing("observation cursor"))?,
            "observation cursor",
        )?,
    }))
}

async fn fetch_window(
    connection: &mut PgConnection,
    sql: &'static str,
    session: SessionId,
    address: Option<TimelineAddress>,
    limit: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, sqlx::Error> {
    let query = sqlx::query(sql).bind(session.into_uuid());
    match address {
        Some(address) => {
            query
                .bind(Decimal::from(address.sequence().get()))
                .bind(limit)
                .fetch_all(connection)
                .await
        }
        None => query.bind(limit).fetch_all(connection).await,
    }
}

fn decode_item(
    row: sqlx::postgres::PgRow,
) -> Result<SessionTimelineItem, SessionTimelineRepositoryError> {
    let kind_text: String = row.try_get("event_kind")?;
    let kind = decode_kind(&kind_text)?;
    let address = required_address(row.try_get("event_sequence")?, "item address")?;
    let projected_structured_bytes = PROJECTED_ITEM_ENVELOPE_BYTES
        .checked_add(u32::try_from(kind_text.len()).map_err(|_| {
            SessionTimelineRepositoryError::Corruption(
                SessionTimelineCorruption::ItemProjectionOverflow,
            )
        })?)
        .ok_or(SessionTimelineCorruption::ItemProjectionOverflow)?;
    Ok(SessionTimelineItem {
        address,
        kind,
        projected_structured_bytes,
    })
}

fn decode_kind(value: &str) -> Result<SessionTimelineEventKind, SessionTimelineCorruption> {
    let discriminator = outbox_event_discriminator_from_str(value)
        .ok_or_else(|| SessionTimelineCorruption::UnsupportedEventKind(value.to_owned()))?;
    Ok(match discriminator {
        OutboxEventDiscriminator::SessionCreated => SessionTimelineEventKind::SessionCreated,
        OutboxEventDiscriminator::SessionModelSettingsChanged => {
            SessionTimelineEventKind::SessionModelSettingsChanged
        }
        OutboxEventDiscriminator::TurnModelSettingsResolved => {
            SessionTimelineEventKind::TurnModelSettingsResolved
        }
        OutboxEventDiscriminator::InputAccepted => SessionTimelineEventKind::InputAccepted,
        OutboxEventDiscriminator::GoalTurnRetired => SessionTimelineEventKind::GoalTurnRetired,
        OutboxEventDiscriminator::TurnActivated => SessionTimelineEventKind::TurnActivated,
        OutboxEventDiscriminator::TurnFailed => SessionTimelineEventKind::TurnFailed,
        OutboxEventDiscriminator::ModelCallTransition => {
            SessionTimelineEventKind::ModelCallTransition
        }
        OutboxEventDiscriminator::ToolBatchTransition => {
            SessionTimelineEventKind::ToolBatchTransition
        }
        OutboxEventDiscriminator::ToolApprovalDecided => {
            SessionTimelineEventKind::ToolApprovalDecided
        }
        OutboxEventDiscriminator::ContextCompacted => SessionTimelineEventKind::ContextCompacted,
        OutboxEventDiscriminator::TurnCompleted => SessionTimelineEventKind::TurnCompleted,
        OutboxEventDiscriminator::TurnRefused => SessionTimelineEventKind::TurnRefused,
        OutboxEventDiscriminator::TurnCancelled => SessionTimelineEventKind::TurnCancelled,
        OutboxEventDiscriminator::TurnReconciliationRequired => {
            SessionTimelineEventKind::TurnReconciliationRequired
        }
        OutboxEventDiscriminator::RunnerStateTransition => {
            SessionTimelineEventKind::RunnerStateTransition
        }
        OutboxEventDiscriminator::DelegationUpdate => SessionTimelineEventKind::DelegationUpdate,
        OutboxEventDiscriminator::DelegationWake => SessionTimelineEventKind::DelegationWake,
    })
}

fn nonnegative(value: Decimal, field: &'static str) -> Result<u64, SessionTimelineCorruption> {
    if !value.fract().is_zero() || value.is_sign_negative() {
        return Err(SessionTimelineCorruption::InvalidOrdinal(field));
    }
    u64::try_from(value).map_err(|_| SessionTimelineCorruption::InvalidOrdinal(field))
}

fn required_address(
    value: Decimal,
    field: &'static str,
) -> Result<TimelineAddress, SessionTimelineCorruption> {
    let sequence = nonnegative(value, field)?;
    NonZeroU64::new(sequence)
        .map(TimelineAddress::new)
        .ok_or(SessionTimelineCorruption::InvalidOrdinal(field))
}

fn optional_address(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<TimelineAddress>, SessionTimelineCorruption> {
    value
        .map(|value| required_address(value, field))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_event_categories_decode_without_generic_fallback() {
        assert_eq!(
            decode_kind("delegation_wake"),
            Ok(SessionTimelineEventKind::DelegationWake)
        );
        assert!(matches!(
            decode_kind("future_event"),
            Err(SessionTimelineCorruption::UnsupportedEventKind(_))
        ));
    }

    #[test]
    fn detail_excerpt_continuation_preserves_utf8_boundaries() {
        let address =
            TimelineAddress::new(NonZeroU64::new(7).expect("fixture address is positive"));
        let content = "aéz";
        let first_expected = "aé";
        let second_expected = "z";
        let mut first_budget = 3;
        let first = excerpt_text(
            content,
            address,
            TimelineBodyField::InputText,
            0,
            0,
            &mut first_budget,
        )
        .expect("the first excerpt is valid");
        let continuation = first.continuation.expect("the body explicitly continues");
        let mut second_budget = 1;
        let second = excerpt_text(
            content,
            address,
            TimelineBodyField::InputText,
            0,
            continuation.offset_bytes,
            &mut second_budget,
        )
        .expect("the continuation begins on a character boundary");

        assert_eq!(first.text, first_expected);
        assert_eq!(continuation.offset_bytes, first_expected.len() as u64);
        assert_eq!(second.text, second_expected);
        assert_eq!(second.total_bytes, content.len() as u64);
        assert_eq!(second.continuation, None);
    }

    #[test]
    fn detail_excerpt_rejects_a_mid_character_cursor() {
        let address =
            TimelineAddress::new(NonZeroU64::new(8).expect("fixture address is positive"));
        let mut budget = 4;
        let error = excerpt_text(
            "é",
            address,
            TimelineBodyField::InputText,
            0,
            1,
            &mut budget,
        )
        .expect_err("a continuation cannot split a UTF-8 character");

        assert!(matches!(
            error,
            SessionTimelineRepositoryError::Corruption(
                SessionTimelineCorruption::InvalidDetailCursor
            )
        ));
    }

    #[test]
    fn tool_detail_advances_from_arguments_to_result() {
        let address =
            TimelineAddress::new(NonZeroU64::new(9).expect("fixture address is positive"));
        let continuation = next_tool_field(
            address,
            TimelineBodyField::ToolArguments,
            3,
            true,
            false,
            true,
        );
        let expected = Some(TimelineDetailContinuation::MoreBody(
            TimelineBodyContinuation {
                address,
                field: TimelineBodyField::ToolResult,
                member_index: 3,
                offset_bytes: 0,
            },
        ));

        assert_eq!(continuation, expected);
    }

    #[test]
    fn tool_detail_advances_to_the_next_member_after_failure() {
        let address =
            TimelineAddress::new(NonZeroU64::new(10).expect("fixture address is positive"));
        let continuation = next_tool_field(
            address,
            TimelineBodyField::ToolFailure,
            4,
            false,
            true,
            true,
        );
        let expected = Some(TimelineDetailContinuation::MoreBody(
            TimelineBodyContinuation {
                address,
                field: TimelineBodyField::ToolArguments,
                member_index: 5,
                offset_bytes: 0,
            },
        ));

        assert_eq!(continuation, expected);
    }
}
