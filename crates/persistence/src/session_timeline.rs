//! PostgreSQL adapter for bounded historical session-timeline reads.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SessionTimelineBounds, SessionTimelineDescriptor, SessionTimelineDetail,
    SessionTimelineDetailBody, SessionTimelineDetailPage, SessionTimelineEventKind,
    SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts, SessionTimelineWindow,
    SessionWorkFacts, TimelineAddress, TimelineBodyContinuation, TimelineBodyField,
    TimelineContinuation, TimelineDetailContinuation, TimelineDetailCursor, TimelineDetailLimits,
    TimelineModelCallDisposition, TimelineModelCallState, TimelineModelUsage, TimelineTextExcerpt,
    TimelineTurnLifecycleKind, TimelineWindowAnchor, TimelineWindowLimits,
};
use signalbox_domain::{ProviderModelIdentity, SessionId, TurnId};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};

use crate::{
    mapping::{OutboxEventDiscriminator, outbox_event_discriminator_from_str},
    outbox::{
        DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEvent,
        DispatchedOutboxEventKind, OutboxDispatchError,
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
    InvalidDetailQuery,
    Corruption(SessionTimelineCorruption),
    Outbox(OutboxDispatchError),
}

impl fmt::Display for SessionTimelineRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "session timeline database failure: {error}")
            }
            Self::InvalidDetailQuery => {
                formatter.write_str("invalid session timeline detail query")
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
            Self::InvalidDetailQuery => None,
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
            return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
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
            return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
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
    SELECT event.event_sequence
      FROM turn_model_settings_resolved_outbox_event AS event
      JOIN turn_model_settings_resolved AS settings
        ON settings.accepted_input_id = event.accepted_input_id
       AND settings.session_id = event.session_id
     WHERE settings.session_id = $1 AND settings.turn_id = $2
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
    UNION ALL
    SELECT wake.event_sequence
      FROM session_delegation_wake_turn_origin AS origin
      JOIN session_pending_delivery AS delivery
        ON delivery.recipient_session_id = origin.recipient_session_id
       AND delivery.delivery_sequence BETWEEN origin.first_delivery_sequence
                                          AND origin.through_delivery_sequence
      LEFT JOIN session_message_delivery AS message_delivery
        ON message_delivery.recipient_session_id = delivery.recipient_session_id
       AND message_delivery.delivery_sequence = delivery.delivery_sequence
       AND delivery.delivery_kind = 'message'
      LEFT JOIN session_child_result_delivery AS result_delivery
        ON result_delivery.parent_session_id = delivery.recipient_session_id
       AND result_delivery.delivery_sequence = delivery.delivery_sequence
       AND delivery.delivery_kind = 'background_result'
      JOIN delegation_wake_outbox_event AS wake
        ON wake.session_id = delivery.recipient_session_id
       AND (
            wake.message_id = message_delivery.message_id
            OR wake.spawning_tool_request_id = result_delivery.spawning_tool_request_id
       )
     WHERE origin.recipient_session_id = $1 AND origin.turn_id = $2
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
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
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
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    let kind = dispatched_event_kind(event.kind());
    let mut remaining = max_bytes - DETAIL_ENVELOPE_BYTES;
    let (body, body_continuation) = match event.kind() {
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
                    turn_id: *turn,
                    text,
                    attachments: Vec::new(),
                },
                continuation,
            )
        }
        DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
            require_cursor_field(cursor, TimelineBodyField::ModelResponse, 0)?;
            let response_offset = cursor.map_or(0, |cursor| cursor.offset_bytes);
            let row = load_model_detail(transaction, *call, response_offset, remaining).await?;
            let response = match row.response {
                Some(response) => Some(response_excerpt(response, address, &mut remaining)?),
                None if cursor.is_none() || cursor.is_some_and(is_item_start_cursor) => None,
                None => return Err(SessionTimelineRepositoryError::InvalidDetailQuery),
            };
            let continuation = response
                .as_ref()
                .and_then(|response| response.continuation)
                .map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineDetailBody::ModelCall {
                    turn_id: *turn,
                    model_call_id: *call,
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
        DispatchedOutboxEventKind::TurnActivated { turn, .. } => {
            require_no_body_cursor(cursor)?;
            (
                SessionTimelineDetailBody::TurnLifecycle {
                    turn_id: *turn,
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
        DispatchedOutboxEventKind::TurnReconciliationRequired { turn, .. } => {
            terminal_turn_body(*turn, "reconciliation_required", cursor)?
        }
        _ => {
            require_no_body_cursor(cursor)?;
            (SessionTimelineDetailBody::EventFact { kind }, None)
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
            turn_id: turn,
            lifecycle: TimelineTurnLifecycleKind::Terminalized,
            cause_code: cause_code.to_owned(),
        },
        None,
    ))
}

fn require_no_body_cursor(
    cursor: Option<TimelineDetailCursor>,
) -> Result<(), SessionTimelineRepositoryError> {
    if cursor.is_some_and(|cursor| {
        cursor.field.is_some() || cursor.member_index != 0 || cursor.offset_bytes != 0
    }) {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    Ok(())
}

fn require_cursor_field(
    cursor: Option<TimelineDetailCursor>,
    field: TimelineBodyField,
    member_index: u32,
) -> Result<(), SessionTimelineRepositoryError> {
    if let Some(cursor) = cursor {
        let item_start = is_item_start_cursor(cursor);
        if !item_start && (cursor.field != Some(field) || cursor.member_index != member_index) {
            return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
        }
    }
    Ok(())
}

const fn is_item_start_cursor(cursor: TimelineDetailCursor) -> bool {
    cursor.field.is_none() && cursor.member_index == 0 && cursor.offset_bytes == 0
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
        .map_err(|_| SessionTimelineRepositoryError::InvalidDetailQuery)?;
    if start > value.len() || !value.is_char_boundary(start) {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
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
    model_identity_id: ProviderModelIdentity,
    request_context_items: u64,
    response: Option<ModelResponseSlice>,
    usage: TimelineModelUsage,
    cause_code: Option<String>,
}

struct ModelResponseSlice {
    bytes: Vec<u8>,
    offset_bytes: u64,
    total_bytes: u64,
}

async fn load_model_detail(
    transaction: &mut Transaction<'_, Postgres>,
    call: signalbox_domain::ModelCallId,
    response_offset: u64,
    max_response_bytes: u32,
) -> Result<ModelDetailRow, SessionTimelineRepositoryError> {
    let row = sqlx::query(
        r#"
SELECT call.session_id,
       call.resolved_provider_model_identity_id,
       frontier.member_count,
       call.usage_input_tokens,
       call.usage_output_tokens,
       call.usage_cache_creation_input_tokens,
       call.usage_cache_read_input_tokens,
       call.terminal_provider_failure_cause,
       (
           SELECT sum(octet_length(entry.assistant_text_value))::numeric
             FROM semantic_transcript_entry AS entry
            WHERE entry.source_session_id = call.session_id
              AND entry.producing_model_call_id = call.model_call_id
              AND entry.payload_kind = 'assistant_text'
       ) AS response_total_bytes
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
    let response_total_bytes = optional_nonnegative(
        row.try_get("response_total_bytes")?,
        "model response byte length",
    )?;
    let response = match response_total_bytes {
        None => None,
        Some(total_bytes) => {
            if response_offset > total_bytes {
                return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
            }
            let slice_end = response_offset
                .saturating_add(u64::from(max_response_bytes))
                .saturating_add(3)
                .min(total_bytes);
            let rows: Vec<Vec<u8>> = sqlx::query_scalar(
                r#"
WITH response_part AS (
    SELECT entry.assistant_response_part_ordinal AS part_ordinal,
           convert_to(entry.assistant_text_value, 'UTF8') AS part_bytes,
           octet_length(entry.assistant_text_value)::numeric AS part_length
      FROM semantic_transcript_entry AS entry
     WHERE entry.source_session_id = $1
       AND entry.producing_model_call_id = $2
       AND entry.payload_kind = 'assistant_text'
), positioned_part AS (
    SELECT part_ordinal, part_bytes, part_length,
           coalesce(
               sum(part_length) OVER (
                   ORDER BY part_ordinal
                   ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
               ),
               0
           ) AS part_start
      FROM response_part
)
SELECT substring(
           part_bytes
           FROM (greatest($3::numeric, part_start) - part_start + 1)::integer
           FOR (
               least(part_start + part_length, $4::numeric)
               - greatest($3::numeric, part_start)
           )::integer
       )
  FROM positioned_part
 WHERE part_start < $4::numeric
   AND part_start + part_length > $3::numeric
 ORDER BY part_ordinal
"#,
            )
            .bind(call_session_uuid(&row)?)
            .bind(call.into_uuid())
            .bind(Decimal::from(response_offset))
            .bind(Decimal::from(slice_end))
            .fetch_all(&mut **transaction)
            .await?;
            Some(ModelResponseSlice {
                bytes: rows.into_iter().flatten().collect(),
                offset_bytes: response_offset,
                total_bytes,
            })
        }
    };
    Ok(ModelDetailRow {
        model_identity_id: ProviderModelIdentity::from_uuid(
            row.try_get::<uuid::Uuid, _>("resolved_provider_model_identity_id")?,
        ),
        request_context_items: nonnegative(row.try_get("member_count")?, "context item count")?,
        response,
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

fn call_session_uuid(row: &sqlx::postgres::PgRow) -> Result<uuid::Uuid, sqlx::Error> {
    row.try_get("session_id")
}

fn response_excerpt(
    response: ModelResponseSlice,
    address: TimelineAddress,
    remaining: &mut u32,
) -> Result<TimelineTextExcerpt, SessionTimelineRepositoryError> {
    let valid_bytes = match std::str::from_utf8(&response.bytes) {
        Ok(_) => response.bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return Err(SessionTimelineRepositoryError::InvalidDetailQuery),
    };
    let available = usize::try_from(*remaining)
        .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
    let mut selected = valid_bytes.min(available);
    while selected > 0 && std::str::from_utf8(&response.bytes[..selected]).is_err() {
        selected -= 1;
    }
    let text = std::str::from_utf8(&response.bytes[..selected])
        .map_err(|_| SessionTimelineRepositoryError::InvalidDetailQuery)?
        .to_owned();
    let charged =
        u32::try_from(selected).map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
    *remaining = remaining
        .checked_sub(charged)
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    let next_offset = response
        .offset_bytes
        .checked_add(
            u64::try_from(selected)
                .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?,
        )
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    let continuation = (next_offset < response.total_bytes).then_some(TimelineBodyContinuation {
        address,
        field: TimelineBodyField::ModelResponse,
        member_index: 0,
        offset_bytes: next_offset,
    });
    Ok(TimelineTextExcerpt {
        text,
        offset_bytes: response.offset_bytes,
        total_bytes: response.total_bytes,
        continuation,
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
            SessionTimelineRepositoryError::InvalidDetailQuery
        ));
    }

    #[test]
    fn model_response_slice_stays_bounded_at_a_utf8_boundary() {
        let address =
            TimelineAddress::new(NonZeroU64::new(9).expect("fixture address is positive"));
        let response = ModelResponseSlice {
            bytes: "éz".as_bytes().to_vec(),
            offset_bytes: 0,
            total_bytes: 4,
        };
        let mut budget = 2;
        let excerpt = response_excerpt(response, address, &mut budget)
            .expect("the bounded response slice is valid UTF-8");
        let continuation = excerpt
            .continuation
            .expect("the response has one byte remaining");

        assert_eq!(excerpt.text, "é");
        assert_eq!(excerpt.offset_bytes, 0);
        assert_eq!(excerpt.total_bytes, 4);
        assert_eq!(continuation.offset_bytes, 2);
        assert_eq!(budget, 0);
    }

    #[test]
    fn model_response_slice_rejects_invalid_interior_utf8() {
        let address =
            TimelineAddress::new(NonZeroU64::new(10).expect("fixture address is positive"));
        let response = ModelResponseSlice {
            bytes: vec![0xff],
            offset_bytes: 1,
            total_bytes: 2,
        };
        let mut budget = 1;

        assert!(matches!(
            response_excerpt(response, address, &mut budget),
            Err(SessionTimelineRepositoryError::InvalidDetailQuery)
        ));
    }
}
