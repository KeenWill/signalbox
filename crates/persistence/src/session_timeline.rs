//! PostgreSQL adapter for bounded historical session-timeline reads.

use signalbox_application::{
    TimelineOwnershipTransition, TimelineSessionOutcome, TimelineSessionState,
};
use std::num::NonZeroU64;

use rust_decimal::Decimal;
use signalbox_application::{
    SessionTimelineBounds, SessionTimelineDescriptor, SessionTimelineDetail,
    SessionTimelineDetailBody, SessionTimelineDetailPage, SessionTimelineEventKind,
    SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts, SessionTimelineWindow,
    SessionWorkFacts, TimelineAddress, TimelineApprovalActor, TimelineApprovalDecision,
    TimelineBlobReference, TimelineBodyContinuation, TimelineBodyField, TimelineBoundChildAction,
    TimelineContinuation, TimelineDelegationDetail, TimelineDelegationOutcome,
    TimelineDelegationPolicy, TimelineDelegationProvenance, TimelineDelegationReason,
    TimelineDelegationWaitMode, TimelineDetailContinuation, TimelineDetailCursor,
    TimelineDetailLimits, TimelineGoalBlockedReason, TimelineGoalEvent, TimelineImportedEvidence,
    TimelineModelCallDisposition, TimelineModelCallState, TimelineModelSettingsDetail,
    TimelineModelUsage, TimelineReconciliationOperation, TimelineRunnerSandboxPosture,
    TimelineRunnerState, TimelineTextExcerpt, TimelineToolApprovalPosture, TimelineToolAttempt,
    TimelineToolBatchState, TimelineToolEffectPosture, TimelineToolSandboxPosture,
    TimelineToolState, TimelineTurnLifecycleKind, TimelineWindowAnchor, TimelineWindowLimits,
    timeline_detail_envelope_bytes,
};
use signalbox_domain::{
    BlobDigest, ImportedConversationId, ImportedSessionRelationship, ImportedTranscriptEntryId,
    ModelCallId, ProviderModelCallFailureCause, ProviderModelIdentity, RunnerSandboxProfile,
    SessionId, ToolApprovalDecider, ToolApprovalDecision, ToolApprovalResolution, ToolAttemptId,
    ToolDecisionSource, ToolName, ToolRequestId, TurnId,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};

use crate::{
    mapping::{
        OUTBOX_EVENT_KIND_UTF8_BYTE_BOUNDS, OutboxEventDiscriminator, TurnDispositionStorageKind,
        input_position_from_numeric, outbox_event_discriminator_from_str, timeline_event_kind_str,
        turn_disposition_kind_from_str,
    },
    outbox::{
        DispatchedBoundChildAction, DispatchedDelegationOutcome, DispatchedDelegationPolicy,
        DispatchedDelegationProvenance, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWaitMode, DispatchedDelegationWake, DispatchedModelCallDisposition,
        DispatchedModelCallState, DispatchedOutboxEvent, DispatchedOutboxEventKind,
        DispatchedReconciliationOperation, DispatchedRunnerState, DispatchedToolBatchState,
        DispatchedTurnTerminalDisposition, OutboxDispatchError,
    },
};

const PROJECTED_ITEM_ENVELOPE_BYTES: u32 = 64;

#[derive(signalbox_derive::OperatorError)]
/// Integrity failure in the durable timeline projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTimelineCorruption {
    #[error("missing session timeline {field_0}")]
    Missing(&'static str),
    #[error("invalid session timeline {field_0}")]
    InvalidOrdinal(&'static str),
    #[error("inconsistent session timeline {field_0}")]
    Inconsistent(&'static str),
    #[error("unsupported session timeline event kind: {field_0}")]
    UnsupportedEventKind(String),
    #[error("unsupported session timeline turn disposition: {field_0}")]
    UnsupportedTurnDisposition(String),
    #[error("session timeline item projection overflowed")]
    ItemProjectionOverflow,
    #[error("session timeline detail projection overflowed")]
    DetailProjectionOverflow,
    #[error("missing session timeline detail record")]
    MissingDetailRecord,
    #[error("invalid session timeline stored {field_0}")]
    InvalidStoredValue(&'static str),
}

#[derive(signalbox_derive::OperatorError)]
/// Database or fail-closed projection failure.
#[derive(Debug)]
pub enum SessionTimelineRepositoryError {
    #[error("session timeline database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error("invalid session timeline detail query")]
    InvalidDetailQuery,
    #[error("session timeline detail contains invalid stored UTF-8")]
    InvalidStoredUtf8,
    #[error(transparent)]
    Corruption(#[source] SessionTimelineCorruption),
    #[error("session timeline detail decode failed: {field_0}")]
    Outbox(#[source] OutboxDispatchError),
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
        let requires_nonempty_window = match &anchor {
            TimelineWindowAnchor::First
            | TimelineWindowAnchor::Latest
            | TimelineWindowAnchor::Around(_) => true,
            TimelineWindowAnchor::Before(address) => descriptor
                .bounds
                .first
                .is_some_and(|first| *address > first),
            TimelineWindowAnchor::After(address) => descriptor
                .bounds
                .latest
                .is_some_and(|latest| *address < latest),
        };
        let requires_first_bound = matches!(&anchor, TimelineWindowAnchor::First);
        let requires_latest_bound = matches!(&anchor, TimelineWindowAnchor::Latest);
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
        if items
            .windows(2)
            .any(|pair| pair[0].address == pair[1].address)
        {
            return Err(SessionTimelineCorruption::InvalidOrdinal("window addresses").into());
        }
        if requires_nonempty_window && items.is_empty() {
            return Err(SessionTimelineCorruption::InvalidOrdinal("window items").into());
        }
        let first = items.first().map(|item| item.address);
        let latest = items.last().map(|item| item.address);
        if (requires_first_bound && first != descriptor.bounds.first)
            || (requires_latest_bound && latest != descriptor.bounds.latest)
            || first
                .is_some_and(|loaded| descriptor.bounds.first.is_none_or(|bound| loaded < bound))
            || latest
                .is_some_and(|loaded| descriptor.bounds.latest.is_none_or(|bound| loaded > bound))
        {
            return Err(SessionTimelineCorruption::InvalidOrdinal("window bounds").into());
        }
        let item_count = u64::try_from(items.len())
            .map_err(|_| SessionTimelineCorruption::InvalidOrdinal("window totals"))?;
        if item_count > descriptor.sizes.item_count
            || u64::from(projected_bytes) > descriptor.sizes.projected_structured_bytes
            || (item_count == descriptor.sizes.item_count
                && u64::from(projected_bytes) != descriptor.sizes.projected_structured_bytes)
        {
            return Err(SessionTimelineCorruption::InvalidOrdinal("window totals").into());
        }
        if item_count == descriptor.sizes.item_count
            && (first != descriptor.bounds.first || latest != descriptor.bounds.latest)
        {
            return Err(SessionTimelineCorruption::InvalidOrdinal("window bounds").into());
        }
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
        let Some(event) = load_detail_event(
            &mut transaction,
            session,
            address,
            cursor,
            limits.max_projected_bytes(),
        )
        .await?
        else {
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
        let projected_body_bytes = detail
            .as_ref()
            .map_or(0, |detail| detail.projected_body_bytes);
        transaction.commit().await?;
        Ok(Some(SessionTimelineDetailPage {
            session,
            items: detail.into_iter().collect(),
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

const DETAIL_ENVELOPE_BYTES: u32 = timeline_detail_envelope_bytes();

/// The widest UTF-8 encoding of a single Unicode scalar value.
///
/// A detail item is only admitted to a page when its body budget can carry at
/// least this many bytes, which is what guarantees that an admitted
/// text-bearing item always delivers at least one complete scalar. Without
/// that headroom a page can charge the envelope, select an empty excerpt, and
/// hand back a body continuation at the offset the caller already asked for —
/// a cursor that never advances.
const MAX_UTF8_SCALAR_BYTES: u32 = 4;

/// The smallest page budget that can seat one text-bearing detail item and
/// still make progress through its body.
const DETAIL_PROGRESS_BYTES: u32 = DETAIL_ENVELOPE_BYTES + MAX_UTF8_SCALAR_BYTES;

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
    SELECT event_sequence FROM turn_activated_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM automatic_reconciliation_exhausted_outbox_event
     WHERE session_id = $1 AND turn_id = $2
    UNION ALL
    SELECT event_sequence FROM turn_terminal_outbox_event
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
    SELECT event.event_sequence
      FROM compact_session_command AS command
      JOIN context_compacted_outbox_event AS event
        ON event.session_id = command.session_id
       AND event.context_compaction_id = command.result_context_compaction_id
     WHERE command.session_id = $1
       AND command.automatic_for_turn_id = $2
       AND command.result_kind = 'applied'
    UNION ALL
    SELECT event.event_sequence
      FROM session_delegation AS relation
      JOIN delegation_update_outbox_event AS event
        ON event.spawning_tool_request_id = relation.spawning_tool_request_id
       AND event.session_id = relation.parent_session_id
     WHERE relation.parent_session_id = $1
       AND relation.parent_turn_id = $2
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
            (wake.subject_kind = 'message'
             AND wake.message_id = message_delivery.message_id)
            OR (wake.subject_kind = 'result'
                AND wake.result_spawning_request_id =
                    result_delivery.spawning_tool_request_id
                AND wake.awaiting_tool_request_id IS NOT DISTINCT FROM
                    result_delivery.awaiting_tool_request_id)
       )
     WHERE origin.recipient_session_id = $1 AND origin.turn_id = $2
)
SELECT event_sequence FROM turn_events
 WHERE event_sequence >= $3
 ORDER BY event_sequence ASC LIMIT $4
"#;

const REGION_DETAIL_ADDRESSES_SQL: &str = r#"
SELECT event_sequence FROM session_timeline_item
 WHERE session_id = $1
   AND event_sequence >= $2 AND event_sequence <= $3
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
        if remaining < DETAIL_PROGRESS_BYTES {
            continuation = Some(TimelineDetailContinuation::MoreAt(address));
            break;
        }
        let event_cursor = cursor.filter(|cursor| cursor.address == address);
        let event = load_detail_event(transaction, session, address, event_cursor, remaining)
            .await?
            .ok_or(SessionTimelineCorruption::MissingDetailRecord)?;
        let (detail, body_continuation) =
            project_detail_event(transaction, &event, event_cursor, remaining).await?;
        if let Some(detail) = detail {
            remaining = remaining
                .checked_sub(detail.projected_body_bytes)
                .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
            items.push(detail);
        }
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

enum DetailEvent {
    Decoded(DispatchedOutboxEvent),
    DelegationUpdate {
        sequence: u64,
        update: DispatchedDelegationUpdate,
        content: Option<ModelResponseSlice>,
    },
    InputAccepted {
        sequence: u64,
        turn: TurnId,
        content: ModelResponseSlice,
        attachments: Vec<TimelineBlobReference>,
    },
}

impl DetailEvent {
    const fn sequence(&self) -> u64 {
        match self {
            Self::Decoded(event) => event.sequence(),
            Self::InputAccepted { sequence, .. } | Self::DelegationUpdate { sequence, .. } => {
                *sequence
            }
        }
    }
}

async fn load_detail_event(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    address: TimelineAddress,
    cursor: Option<TimelineDetailCursor>,
    max_bytes: u32,
) -> Result<Option<DetailEvent>, SessionTimelineRepositoryError> {
    let sequence = address.sequence().get();
    let (allocated, event_beyond_allocated, header) =
        crate::outbox::load_event_header(transaction, sequence).await?;
    if event_beyond_allocated {
        return Err(SessionTimelineCorruption::MissingDetailRecord.into());
    }
    if sequence > allocated {
        return Ok(None);
    }
    let Some(header) = header else {
        return Err(SessionTimelineCorruption::MissingDetailRecord.into());
    };
    if header.session != Some(session) {
        return Ok(None);
    }
    if header.discriminator == OutboxEventDiscriminator::DelegationUpdate {
        let update = crate::outbox::load_delegation_update(
            transaction,
            sequence,
            session.into_uuid(),
            false,
        )
        .await?;
        let offset = cursor.map_or(0, |cursor| cursor.offset_bytes);
        let row = sqlx::query(
            "SELECT octet_length(content_text)::numeric AS total_bytes,
                    substring(convert_to(content_text, 'UTF8')
                        FROM (least($3::numeric, octet_length(content_text)::numeric) + 1)::integer
                        FOR $4::integer) AS content_bytes
               FROM delegation_update_outbox_event
              WHERE event_sequence = $1 AND session_id = $2",
        )
        .bind(Decimal::from(sequence))
        .bind(session.into_uuid())
        .bind(Decimal::from(offset))
        .bind(
            i64::from(max_bytes.saturating_sub(DETAIL_ENVELOPE_BYTES))
                + i64::from(MAX_UTF8_SCALAR_BYTES)
                - 1,
        )
        .fetch_one(&mut **transaction)
        .await?;
        return Ok(Some(DetailEvent::DelegationUpdate {
            sequence,
            update,
            content: optional_text_slice(&row, offset)?,
        }));
    }
    if header.discriminator == OutboxEventDiscriminator::InputAccepted {
        require_cursor_field(cursor, TimelineBodyField::InputText, 0)?;
        let offset = cursor.map_or(0, |cursor| cursor.offset_bytes);
        let body_budget = max_bytes.saturating_sub(DETAIL_ENVELOPE_BYTES);
        let requested_bytes = u64::from(body_budget).saturating_add(3);
        let requested_bytes = i64::try_from(requested_bytes)
            .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
        let row = sqlx::query(
            r#"
SELECT event.turn_id,
       accepted.accepted_input_id,
       accepted.acceptance_position,
       octet_length(parts.content_text)::numeric AS total_bytes,
       substring(
           convert_to(parts.content_text, 'UTF8')
           FROM (least(
               $3::numeric,
               octet_length(parts.content_text)::numeric
           ) + 1)::integer
           FOR $4::integer
       ) AS content_bytes
  FROM input_accepted_outbox_event AS event
  JOIN accepted_input AS accepted
    ON accepted.accepted_input_id = event.accepted_input_id
   AND accepted.session_id = event.session_id
   AND accepted.acceptance_position = event.acceptance_position
   AND accepted.origin_turn_id = event.turn_id
  JOIN LATERAL (
       SELECT COALESCE(
                  string_agg(part.text_value, '' ORDER BY part.position)
                      FILTER (WHERE part.part_kind = 'text'),
                  ''
              ) AS content_text
         FROM accepted_input_content_part AS part
        WHERE part.accepted_input_id = accepted.accepted_input_id
  ) AS parts ON TRUE
  LEFT JOIN submit_input_command AS command
    ON command.command_id = accepted.accepting_command_id
   AND command.session_id = event.session_id
   AND command.result_session_id = event.session_id
   AND command.result_kind = 'applied'
   AND command.result_accepted_input_id = event.accepted_input_id
   AND accepted_input_parts_match_command(
       accepted.accepted_input_id
   )
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
       (accepted.accepting_command_id IS NOT NULL AND (
           (accepted.disposition_kind = 'origin_of'
               AND command.result_turn_id = event.turn_id)
           OR (accepted.disposition_kind = 'reclassified_as_turn_origin'
               AND command.command_id IS NOT NULL
               AND command.result_turn_id IS NULL
               AND accepted.expected_active_turn_id IS NOT NULL
               AND source.state_kind = 'terminal')
       ))
       OR (accepted.accepting_command_id IS NULL
           AND command.command_id IS NULL
           AND accepted.disposition_kind = 'origin_of'
           AND goal.turn_id = event.turn_id)
   )
"#,
        )
        .bind(Decimal::from(sequence))
        .bind(session.into_uuid())
        .bind(Decimal::from(offset))
        .bind(requested_bytes)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(SessionTimelineCorruption::MissingDetailRecord)?;
        input_position_from_numeric(row.try_get("acceptance_position")?)
            .map_err(|_| SessionTimelineCorruption::InvalidOrdinal("input acceptance position"))?;
        let accepted_input_id: uuid::Uuid = row.try_get("accepted_input_id")?;
        let attachment_rows = sqlx::query(
            "SELECT part.blob_digest, blob.byte_length, part.declared_media_type
               FROM accepted_input_content_part AS part
               JOIN blob ON blob.digest = part.blob_digest
              WHERE part.accepted_input_id = $1
                AND part.part_kind = 'attachment'
              ORDER BY part.position",
        )
        .bind(accepted_input_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut attachments = Vec::with_capacity(attachment_rows.len());
        for attachment in attachment_rows {
            let digest: [u8; 32] = attachment
                .try_get::<Vec<u8>, _>("blob_digest")?
                .try_into()
                .map_err(|_| SessionTimelineCorruption::InvalidOrdinal("attachment digest"))?;
            attachments.push(TimelineBlobReference {
                blob_id: BlobDigest::from_bytes(digest),
                length_bytes: nonnegative(
                    attachment.try_get("byte_length")?,
                    "attachment byte length",
                )?,
                media_type: attachment.try_get("declared_media_type")?,
            });
        }
        let total_bytes = nonnegative(row.try_get("total_bytes")?, "input byte length")?;
        if offset > total_bytes {
            return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
        }
        return Ok(Some(DetailEvent::InputAccepted {
            sequence,
            turn: TurnId::from_uuid(row.try_get("turn_id")?),
            content: ModelResponseSlice {
                bytes: row.try_get("content_bytes")?,
                offset_bytes: offset,
                total_bytes,
            },
            attachments,
        }));
    }
    let (_, event_beyond_allocated, event) =
        crate::outbox::load_event(transaction, sequence).await?;
    if event_beyond_allocated {
        return Err(SessionTimelineCorruption::MissingDetailRecord.into());
    }
    Ok(event
        .filter(|event| event.session() == Some(session))
        .map(DetailEvent::Decoded))
}

async fn project_detail_event(
    transaction: &mut Transaction<'_, Postgres>,
    event: &DetailEvent,
    cursor: Option<TimelineDetailCursor>,
    max_bytes: u32,
) -> Result<
    (
        Option<SessionTimelineDetail>,
        Option<TimelineDetailContinuation>,
    ),
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
    let mut remaining = max_bytes - DETAIL_ENVELOPE_BYTES;
    let (kind, body, body_continuation) = match event {
        DetailEvent::InputAccepted {
            turn,
            content,
            attachments,
            ..
        } => {
            let text = bounded_text_excerpt(
                content,
                address,
                TimelineBodyField::InputText,
                &mut remaining,
            )?;
            let continuation = text.continuation.map(TimelineDetailContinuation::MoreBody);
            (
                SessionTimelineEventKind::InputAccepted,
                SessionTimelineDetailBody::UserInput {
                    turn_id: *turn,
                    text,
                    attachments: attachments.clone(),
                },
                continuation,
            )
        }
        DetailEvent::DelegationUpdate {
            update, content, ..
        } => {
            let (body, continuation) = project_delegation_update(
                address,
                update,
                content.as_ref(),
                cursor,
                &mut remaining,
            )?;
            (
                SessionTimelineEventKind::DelegationUpdate,
                body,
                continuation,
            )
        }
        DetailEvent::Decoded(event) => {
            let kind = dispatched_event_kind(event.kind());
            let (body, body_continuation) = match event.kind() {
                DispatchedOutboxEventKind::InputAccepted { .. } => {
                    return Err(SessionTimelineCorruption::MissingDetailRecord.into());
                }
                DispatchedOutboxEventKind::SessionCreated(creation) => {
                    require_no_body_cursor(cursor)?;
                    let imported_evidence = load_imported_evidence(
                        transaction,
                        event
                            .session()
                            .ok_or(SessionTimelineCorruption::Missing("event session"))?,
                    )
                    .await?;
                    (
                        SessionTimelineDetailBody::SessionCreated {
                            workspace_root_kind: crate::session_workspace::read_binding(
                                transaction,
                                event
                                    .session()
                                    .ok_or(SessionTimelineCorruption::Missing("event session"))?,
                            )
                            .await?,
                            cause: creation.cause,
                            imported_evidence,
                        },
                        None,
                    )
                }
                DispatchedOutboxEventKind::SessionModelSettingsChanged(settings) => {
                    require_no_body_cursor(cursor)?;
                    (
                        SessionTimelineDetailBody::ModelSettings {
                            detail: TimelineModelSettingsDetail::SessionDefaultsChanged {
                                command_id: settings.command_id(),
                                prior_defaults_version: settings.prior_defaults_version(),
                                installed_defaults_version: settings.installed_defaults_version(),
                                prior_model: settings.prior_model(),
                                installed_model: settings.installed_model(),
                                prior_settings: settings.prior_settings(),
                                installed_settings: settings.installed_settings(),
                                caller_override: settings.caller_override(),
                                adjustments: settings.adjustments().to_vec(),
                            },
                        },
                        None,
                    )
                }
                DispatchedOutboxEventKind::TurnModelSettingsResolved(settings) => {
                    require_no_body_cursor(cursor)?;
                    (
                        SessionTimelineDetailBody::ModelSettings {
                            detail: TimelineModelSettingsDetail::TurnResolved {
                                accepted_input_id: settings.accepted_input(),
                                turn_id: settings.turn(),
                                defaults_version: settings.defaults_version(),
                                selection: *settings.selection(),
                                per_call_override: settings.per_call_override(),
                                settings: settings.settings(),
                                adjusted_from_selection_id: settings.adjusted_from_selection(),
                                adjustments: settings.adjustments().to_vec(),
                            },
                        },
                        None,
                    )
                }
                DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
                    require_cursor_field(cursor, TimelineBodyField::ModelResponse, 0)?;
                    let response_offset = cursor.map_or(0, |cursor| cursor.offset_bytes);
                    let include_terminal_evidence = match state {
                        DispatchedModelCallState::Prepared
                        | DispatchedModelCallState::InFlight
                        | DispatchedModelCallState::CancellationRequested => false,
                        DispatchedModelCallState::Terminal(_) => true,
                    };
                    let include_response = matches!(
                        state,
                        DispatchedModelCallState::Terminal(
                            DispatchedModelCallDisposition::Completed
                        )
                    );
                    let row = load_model_detail(
                        transaction,
                        *call,
                        include_response,
                        include_terminal_evidence,
                        response_offset,
                        remaining,
                    )
                    .await?;
                    let response = match row.response {
                        Some(response) => {
                            Some(response_excerpt(response, address, &mut remaining)?)
                        }
                        None if cursor.is_none() || cursor.is_some_and(is_item_start_cursor) => {
                            None
                        }
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
                            provider_failure_cause: row.provider_failure_cause,
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
                    let offset_bytes = cursor.map_or(0, |cursor| cursor.offset_bytes);
                    let requested_bytes = u64::from(remaining).saturating_add(3);
                    let requested_bytes = i64::try_from(requested_bytes)
                        .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
                    let row = sqlx::query(
                        r#"
SELECT octet_length(context_summary_value)::numeric AS total_bytes,
       substring(
           convert_to(context_summary_value, 'UTF8')
           FROM (least(
               $2::numeric,
               octet_length(context_summary_value)::numeric
           ) + 1)::integer
           FOR $3::integer
       ) AS content_bytes
  FROM semantic_transcript_entry
 WHERE semantic_entry_id = $1
   AND payload_kind = 'context_summary'
"#,
                    )
                    .bind(summary_entry.into_uuid())
                    .bind(Decimal::from(offset_bytes))
                    .bind(requested_bytes)
                    .fetch_one(&mut **transaction)
                    .await?;
                    let total_bytes =
                        nonnegative(row.try_get("total_bytes")?, "context summary byte length")?;
                    if offset_bytes > total_bytes {
                        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
                    }
                    let summary = bounded_text_excerpt(
                        &ModelResponseSlice {
                            bytes: row.try_get("content_bytes")?,
                            offset_bytes,
                            total_bytes,
                        },
                        address,
                        TimelineBodyField::CompactionSummary,
                        &mut remaining,
                    )?;
                    let continuation = summary
                        .continuation
                        .map(TimelineDetailContinuation::MoreBody);
                    (
                        SessionTimelineDetailBody::ContextCompaction {
                            compaction_id: *compaction,
                            model_call_id: *call,
                            through_position: *through_position,
                            summary_entry_id: *summary_entry,
                            result_frontier_id: *result_frontier,
                            summary,
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
                DispatchedOutboxEventKind::TurnTerminal {
                    turn,
                    disposition: DispatchedTurnTerminalDisposition::Failed { .. },
                } => terminal_turn_body(*turn, "failed", cursor)?,
                DispatchedOutboxEventKind::TurnTerminal {
                    turn,
                    disposition: DispatchedTurnTerminalDisposition::Completed { .. },
                } => terminal_turn_body(*turn, "completed", cursor)?,
                DispatchedOutboxEventKind::TurnTerminal {
                    turn,
                    disposition: DispatchedTurnTerminalDisposition::Refused { .. },
                } => terminal_turn_body(*turn, "refused", cursor)?,
                DispatchedOutboxEventKind::TurnTerminal {
                    turn,
                    disposition: DispatchedTurnTerminalDisposition::Cancelled { .. },
                } => terminal_turn_body(*turn, "cancelled", cursor)?,
                DispatchedOutboxEventKind::TurnTerminal {
                    turn,
                    disposition:
                        DispatchedTurnTerminalDisposition::ReconciliationRequired {
                            operation,
                            terminal_frontier,
                        },
                } => {
                    require_no_body_cursor(cursor)?;
                    (
                        SessionTimelineDetailBody::Reconciliation {
                            turn_id: *turn,
                            operation: reconciliation_operation(*operation),
                            terminal_frontier_id: *terminal_frontier,
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
                    if let Some(directory) = working_directory {
                        let directory_bytes = u32::try_from(directory.as_str().len())
                            .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
                        let Some(after_directory) = remaining.checked_sub(directory_bytes) else {
                            return Ok((None, Some(TimelineDetailContinuation::MoreAt(address))));
                        };
                        remaining = after_directory;
                    }
                    (
                        SessionTimelineDetailBody::Runner {
                            runner_id: *runner,
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
                DispatchedOutboxEventKind::DelegationUpdate(_) => {
                    return Err(SessionTimelineCorruption::MissingDetailRecord.into());
                }
                DispatchedOutboxEventKind::DelegationWake(wake) => {
                    require_no_body_cursor(cursor)?;
                    (delegation_wake_body(*wake), None)
                }
                DispatchedOutboxEventKind::GoalChanged(change) => {
                    let session = event
                        .session()
                        .ok_or(SessionTimelineCorruption::Missing("event session"))?;
                    let goal = load_goal_change_event(
                        transaction,
                        session,
                        change.event_ordinal,
                        address,
                        cursor,
                        &mut remaining,
                    )
                    .await?;
                    let continuation =
                        goal_event_continuation(&goal).map(TimelineDetailContinuation::MoreBody);
                    (
                        SessionTimelineDetailBody::GoalEvent {
                            session_id: session,
                            event: goal,
                        },
                        continuation,
                    )
                }
                DispatchedOutboxEventKind::SessionStateChanged(change) => {
                    require_no_body_cursor(cursor)?;
                    let state = match change.state {
                        signalbox_domain::SessionLifecycleState::Created => {
                            TimelineSessionState::Created
                        }
                        signalbox_domain::SessionLifecycleState::Dispatched => {
                            TimelineSessionState::Dispatched
                        }
                        signalbox_domain::SessionLifecycleState::Active => {
                            TimelineSessionState::Active
                        }
                        signalbox_domain::SessionLifecycleState::Waiting { .. } => {
                            TimelineSessionState::Waiting
                        }
                        signalbox_domain::SessionLifecycleState::Recovering { .. } => {
                            TimelineSessionState::Recovering
                        }
                        signalbox_domain::SessionLifecycleState::Blocked { .. } => {
                            TimelineSessionState::Blocked
                        }
                        signalbox_domain::SessionLifecycleState::Parked { .. } => {
                            TimelineSessionState::Parked
                        }
                        signalbox_domain::SessionLifecycleState::Terminal { .. } => {
                            return Err(SessionTimelineCorruption::InvalidStoredValue(
                                "nonterminal session state",
                            )
                            .into());
                        }
                    };
                    (SessionTimelineDetailBody::SessionState { state }, None)
                }
                DispatchedOutboxEventKind::SessionTerminal(terminal) => {
                    require_no_body_cursor(cursor)?;
                    let outcome = match terminal.outcome {
                        signalbox_domain::SessionTerminalOutcome::AchievedVerified => {
                            TimelineSessionOutcome::AchievedVerified
                        }
                        signalbox_domain::SessionTerminalOutcome::AchievedDeclared => {
                            TimelineSessionOutcome::AchievedDeclared
                        }
                        signalbox_domain::SessionTerminalOutcome::FailedRetryable { .. } => {
                            TimelineSessionOutcome::FailedRetryable
                        }
                        signalbox_domain::SessionTerminalOutcome::FailedStructural { .. } => {
                            TimelineSessionOutcome::FailedStructural
                        }
                        signalbox_domain::SessionTerminalOutcome::FailedUnknown => {
                            TimelineSessionOutcome::FailedUnknown
                        }
                        signalbox_domain::SessionTerminalOutcome::Stopped { .. } => {
                            TimelineSessionOutcome::Stopped
                        }
                        signalbox_domain::SessionTerminalOutcome::Superseded { .. } => {
                            TimelineSessionOutcome::Superseded
                        }
                        signalbox_domain::SessionTerminalOutcome::Abandoned => {
                            TimelineSessionOutcome::Abandoned
                        }
                        signalbox_domain::SessionTerminalOutcome::Retired { .. } => {
                            TimelineSessionOutcome::Retired
                        }
                    };
                    (SessionTimelineDetailBody::SessionTerminal { outcome }, None)
                }
                DispatchedOutboxEventKind::CommandSettled { command, result } => {
                    require_no_body_cursor(cursor)?;
                    let rejection = match result {
                        crate::outbox::DispatchedCommandSettlement::Applied => None,
                        crate::outbox::DispatchedCommandSettlement::Rejected { kind } => {
                            Some(kind.clone())
                        }
                    };
                    (
                        SessionTimelineDetailBody::CommandSettlement {
                            command_id: *command,
                            rejection,
                        },
                        None,
                    )
                }
                DispatchedOutboxEventKind::InjectionSettled { command, outcome } => {
                    require_no_body_cursor(cursor)?;
                    let (delivered, turn_id, rejection) = match outcome {
                        crate::outbox::DispatchedInjectionOutcome::Delivered { turn } => {
                            (true, *turn, None)
                        }
                        crate::outbox::DispatchedInjectionOutcome::NotDelivered => {
                            (false, None, None)
                        }
                        crate::outbox::DispatchedInjectionOutcome::Rejected { kind } => {
                            (false, None, Some(kind.clone()))
                        }
                    };
                    (
                        SessionTimelineDetailBody::InjectionSettlement {
                            command_id: *command,
                            delivered,
                            turn_id,
                            rejection,
                        },
                        None,
                    )
                }
                DispatchedOutboxEventKind::SessionOwnershipChanged(change) => {
                    require_no_body_cursor(cursor)?;
                    let transition = match change.transition {
                        signalbox_domain::SessionOwnershipTransition::CreatedOwned
                        | signalbox_domain::SessionOwnershipTransition::CreatedUnmonitored => {
                            return Err(SessionTimelineCorruption::InvalidStoredValue(
                                "ownership transition",
                            )
                            .into());
                        }
                        signalbox_domain::SessionOwnershipTransition::Adopted => {
                            TimelineOwnershipTransition::Adopted
                        }
                        signalbox_domain::SessionOwnershipTransition::Released => {
                            TimelineOwnershipTransition::Released
                        }
                    };
                    (SessionTimelineDetailBody::Ownership { transition }, None)
                }
                DispatchedOutboxEventKind::AutomaticReconciliationExhausted(_) => {
                    require_no_body_cursor(cursor)?;
                    (SessionTimelineDetailBody::EventFact { kind }, None)
                }
                DispatchedOutboxEventKind::CredentialPoolExhausted(evidence) => {
                    terminal_turn_body(TurnId::from_uuid(evidence.turn_id), "failed", cursor)?
                }
                DispatchedOutboxEventKind::TurnTerminal {
                    disposition: DispatchedTurnTerminalDisposition::Retired,
                    ..
                } => {
                    require_no_body_cursor(cursor)?;
                    (SessionTimelineDetailBody::EventFact { kind }, None)
                }
            };
            (kind, body, body_continuation)
        }
    };
    let projected_body_bytes = max_bytes
        .checked_sub(remaining)
        .ok_or(SessionTimelineCorruption::DetailProjectionOverflow)?;
    Ok((
        Some(SessionTimelineDetail {
            address,
            kind,
            body,
            projected_body_bytes,
        }),
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

async fn load_imported_evidence(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
) -> Result<Option<TimelineImportedEvidence>, SessionTimelineRepositoryError> {
    let row = sqlx::query(
        "SELECT imported_conversation_id, imported_frontier_entry_id,
                imported_frontier_position, imported_relationship_kind
           FROM session WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    let conversation = row.try_get::<Option<uuid::Uuid>, _>("imported_conversation_id")?;
    let entry = row.try_get::<Option<uuid::Uuid>, _>("imported_frontier_entry_id")?;
    let position = row.try_get::<Option<Decimal>, _>("imported_frontier_position")?;
    let relationship = row.try_get::<Option<String>, _>("imported_relationship_kind")?;
    match (conversation, entry, position, relationship.as_deref()) {
        (None, None, None, None) => Ok(None),
        (Some(conversation), Some(entry), Some(position), Some(relationship)) => {
            let relationship = match relationship {
                "resume" => ImportedSessionRelationship::Resume,
                "fork" => ImportedSessionRelationship::Fork,
                _ => {
                    return Err(SessionTimelineCorruption::InvalidStoredValue(
                        "imported relationship",
                    )
                    .into());
                }
            };
            Ok(Some(TimelineImportedEvidence {
                imported_conversation_id: ImportedConversationId::from_uuid(conversation),
                imported_entry_id: ImportedTranscriptEntryId::from_uuid(entry),
                imported_position: nonnegative(position, "imported frontier position")?,
                relationship,
            }))
        }
        _ => Err(SessionTimelineCorruption::Missing("imported frontier evidence").into()),
    }
}

async fn load_goal_change_event(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    goal_event_ordinal: u64,
    address: TimelineAddress,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<TimelineGoalEvent, SessionTimelineRepositoryError> {
    require_cursor_field(cursor, TimelineBodyField::GoalText, 0)?;
    let offset = cursor.map_or(0, |cursor| cursor.offset_bytes);
    let row = sqlx::query(
        "SELECT event.generation, event.event_kind, event.blocked_reason, event.session_outcome_kind,
                octet_length(body.content)::numeric AS total_bytes,
                substring(convert_to(body.content, 'UTF8')
                    FROM (least($3::numeric, octet_length(body.content)::numeric) + 1)::integer
                    FOR $4::integer) AS content_bytes
           FROM goal_event AS event
           CROSS JOIN LATERAL (
               SELECT COALESCE(event.statement, event.need, event.guidance, event.report) AS content
           ) AS body
          WHERE event.session_id = $1
            AND event.event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(goal_event_ordinal))
    .bind(Decimal::from(offset))
    .bind(i64::from(*remaining) + i64::from(MAX_UTF8_SCALAR_BYTES) - 1)
    .fetch_one(&mut **transaction)
    .await?;
    let text = optional_text_slice(&row, offset)?
        .map(|text| bounded_text_excerpt(&text, address, TimelineBodyField::GoalText, remaining))
        .transpose()?;
    // A textless retiring event is a legitimate stored shape, so a
    // caller-supplied `goal_text` cursor naming it is an inapplicable query,
    // not stored corruption.
    if text.is_none() && cursor.is_some_and(|cursor| !is_item_start_cursor(cursor)) {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    let generation = nonnegative(row.try_get("generation")?, "goal generation")?;
    let event_kind: String = row.try_get("event_kind")?;
    let reason: Option<String> = row.try_get("blocked_reason")?;
    let mut event = timeline_goal_event(
        generation,
        &event_kind,
        reason.as_deref(),
        row.try_get::<Option<String>, _>("session_outcome_kind")?
            .as_deref(),
        text,
    )?;
    if let TimelineGoalEvent::UserStopped {
        settling_turn,
        abandoned_actions,
        ..
    } = &mut event
    {
        let stop = sqlx::query("SELECT turn_id, abandoned_actions FROM goal_stop_settlement WHERE session_id = $1 AND event_ordinal = $2")
            .bind(session.into_uuid()).bind(Decimal::from(goal_event_ordinal))
            .fetch_one(&mut **transaction).await?;
        *settling_turn = stop
            .try_get::<Option<uuid::Uuid>, _>("turn_id")?
            .map(TurnId::from_uuid);
        *abandoned_actions = stop
            .try_get::<Option<i64>, _>("abandoned_actions")?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| SessionTimelineCorruption::InvalidStoredValue("abandoned actions"))?;
    }
    Ok(event)
}

fn timeline_goal_event(
    generation: u64,
    event_kind: &str,
    reason: Option<&str>,
    outcome: Option<&str>,
    text: Option<TimelineTextExcerpt>,
) -> Result<TimelineGoalEvent, SessionTimelineCorruption> {
    if event_kind == "session_closed" {
        let outcome = match outcome {
            Some("failed_retryable") => TimelineSessionOutcome::FailedRetryable,
            Some("failed_structural") => TimelineSessionOutcome::FailedStructural,
            Some("failed_unknown") => TimelineSessionOutcome::FailedUnknown,
            Some("stopped") => TimelineSessionOutcome::Stopped,
            Some("superseded") => TimelineSessionOutcome::Superseded,
            Some("abandoned") => TimelineSessionOutcome::Abandoned,
            Some("retired") => TimelineSessionOutcome::Retired,
            _ => {
                return Err(SessionTimelineCorruption::InvalidStoredValue(
                    "goal session outcome",
                ));
            }
        };
        return Ok(TimelineGoalEvent::SessionClosed {
            generation,
            outcome,
        });
    }
    match goal_event_kind(event_kind)? {
        StoredGoalEventKind::Commissioned => Ok(TimelineGoalEvent::Commissioned {
            generation,
            text: text.ok_or(SessionTimelineCorruption::InvalidStoredValue(
                "commissioned goal text",
            ))?,
        }),
        StoredGoalEventKind::Blocked => Ok(TimelineGoalEvent::Blocked {
            generation,
            reason: reason.map(goal_blocked_reason).transpose()?.ok_or(
                SessionTimelineCorruption::InvalidStoredValue("blocked goal reason"),
            )?,
            text: text.ok_or(SessionTimelineCorruption::InvalidStoredValue(
                "blocked goal text",
            ))?,
        }),
        StoredGoalEventKind::Resumed if reason.is_none() => {
            Ok(TimelineGoalEvent::Resumed { generation, text })
        }
        StoredGoalEventKind::Achieved if reason.is_none() => Ok(TimelineGoalEvent::Achieved {
            generation,
            text: text.ok_or(SessionTimelineCorruption::InvalidStoredValue(
                "achieved goal text",
            ))?,
        }),
        StoredGoalEventKind::UserStopped if reason.is_none() && text.is_none() => {
            Ok(TimelineGoalEvent::UserStopped {
                generation,
                settling_turn: None,
                abandoned_actions: None,
            })
        }
        StoredGoalEventKind::Superseded if reason.is_none() => Ok(TimelineGoalEvent::Superseded {
            generation,
            text: text.ok_or(SessionTimelineCorruption::InvalidStoredValue(
                "superseded goal text",
            ))?,
        }),
        StoredGoalEventKind::Resumed
        | StoredGoalEventKind::Achieved
        | StoredGoalEventKind::UserStopped
        | StoredGoalEventKind::Superseded => Err(SessionTimelineCorruption::InvalidStoredValue(
            "goal event shape",
        )),
    }
}

const fn goal_event_continuation(event: &TimelineGoalEvent) -> Option<TimelineBodyContinuation> {
    match event {
        TimelineGoalEvent::Commissioned { text, .. }
        | TimelineGoalEvent::Blocked { text, .. }
        | TimelineGoalEvent::Achieved { text, .. }
        | TimelineGoalEvent::Superseded { text, .. } => text.continuation,
        TimelineGoalEvent::Resumed { text, .. } => match text {
            Some(text) => text.continuation,
            None => None,
        },
        TimelineGoalEvent::UserStopped { .. } | TimelineGoalEvent::SessionClosed { .. } => None,
    }
}

// The fallback follows only the transition's sealed frontier or recovery attempt.
// Terminal attempt payloads and their authorization facts are immutable.
const TOOL_DETAIL_MEMBERS_SQL: &str = "WITH transition AS (
    SELECT event_sequence, session_id, producing_model_call_id,
           transition_kind, frontier_id, tool_attempt_id
      FROM tool_batch_transition_outbox_event WHERE event_sequence = $1
), frozen AS NOT MATERIALIZED (
    SELECT * FROM tool_batch_transition_detail_member WHERE event_sequence = $1
), tool_members AS NOT MATERIALIZED (
    SELECT member_index, request_id, attempt_id, approval_judge_escalated,
           attempt_state_kind, attempt_terminal_disposition_kind,
           attempt_error_kind, attempt_has_result, attempt_has_failure,
           attempt_sandbox_posture, attempt_result_text, attempt_error_detail
      FROM frozen WHERE member_kind = 'tool'
    UNION ALL
         SELECT request.request_ordinal,
                request.request_id, attempt.attempt_id, EXISTS (
                    SELECT 1
                      FROM tool_approval_judge_model_call AS judge
                      JOIN tool_approval_decided_outbox_event AS decision
                        ON decision.request_id = judge.request_id
                       AND decision.event_sequence < transition.event_sequence
                     WHERE judge.request_id = request.request_id
                       AND judge.recommendation_kind = 'escalate_to_human'
                ),
                attempt.state_kind, attempt.terminal_disposition_kind,
                attempt.error_kind,
                CASE WHEN attempt.attempt_id IS NULL THEN NULL
                     ELSE attempt.result_text IS NOT NULL END,
                CASE WHEN attempt.attempt_id IS NULL THEN NULL
                     ELSE attempt.error_detail IS NOT NULL END,
                (
                    SELECT CASE placement.requested_sandbox_profile
                        WHEN 'ambient' THEN 'unsandboxed'
                        WHEN 'workspace_restricted' THEN 'sandboxed'
                    END
                      FROM runner_physical_attempt_lease_binding
                           AS sandbox_binding
                      JOIN runner_lease_generation AS sandbox_lease
                        ON sandbox_lease.lease_id = sandbox_binding.lease_id
                       AND sandbox_lease.attempt_id = sandbox_binding.attempt_id
                      JOIN runner_session_placement_record AS placement
                        ON placement.session_id = sandbox_lease.session_id
                       AND placement.event_ordinal =
                           sandbox_lease.placement_event_ordinal
                     WHERE sandbox_binding.attempt_id = attempt.attempt_id
                     ORDER BY sandbox_lease.generation DESC
                     LIMIT 1
                ),
                attempt.result_text, attempt.error_detail
           FROM transition
           JOIN tool_round AS round
             ON round.producing_model_call_id = transition.producing_model_call_id
           JOIN context_frontier AS boundary
             ON boundary.context_frontier_id = round.boundary_frontier_id
            AND boundary.owning_session_id = transition.session_id
           JOIN tool_request AS request
             ON request.producing_model_call_id = transition.producing_model_call_id
           LEFT JOIN context_frontier_member AS member
             ON transition.transition_kind = 'results_projected'
            AND member.owning_session_id = transition.session_id
            AND member.context_frontier_id = transition.frontier_id
            AND member.member_position = boundary.member_count + request.request_ordinal + 1
           LEFT JOIN semantic_transcript_entry AS payload
             ON payload.source_session_id = member.source_session_id
            AND payload.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN tool_attempt AS attempt
             ON attempt.attempt_id = CASE transition.transition_kind
                 WHEN 'results_projected' THEN payload.tool_result_attempt_id
                 WHEN 'recovery_required' THEN transition.tool_attempt_id
             END
            AND attempt.request_id = request.request_id
            AND attempt.state_kind = 'terminal'
          WHERE NOT EXISTS (SELECT 1 FROM frozen)
), goal_members AS (
    SELECT member_index, session_id, goal_event_ordinal
      FROM frozen WHERE member_kind = 'goal'
    UNION ALL
    SELECT row_number() OVER (ORDER BY request.request_ordinal, event.event_ordinal) - 1,
           event.session_id, event.event_ordinal
      FROM transition
      JOIN tool_request AS request
        ON request.producing_model_call_id = transition.producing_model_call_id
      JOIN goal_event AS event
        ON event.session_id = transition.session_id
       AND event.model_tool_request_id = request.request_id
      JOIN goal_changed_outbox_event AS goal_header
        ON goal_header.session_id = event.session_id
       AND goal_header.event_ordinal = event.event_ordinal
       AND goal_header.event_sequence < transition.event_sequence
     WHERE NOT EXISTS (SELECT 1 FROM frozen)
)";

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
    if let Some(cursor) = cursor
        && cursor.field.is_none()
        && (cursor.member_index != 0 || cursor.offset_bytes != 0)
    {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    if let Some(goal_cursor) =
        cursor.filter(|cursor| cursor.field == Some(TimelineBodyField::GoalText))
    {
        let goal_row = load_goal_event_row(transaction, address, goal_cursor, *remaining)
            .await?
            .ok_or(SessionTimelineRepositoryError::InvalidDetailQuery)?;
        return project_tool_goal(
            address,
            turn,
            producing_call,
            state,
            goal_cursor,
            goal_row,
            remaining,
        );
    }
    let requested_field = cursor
        .and_then(|cursor| cursor.field)
        .unwrap_or(TimelineBodyField::ToolArguments);
    if !matches!(
        requested_field,
        TimelineBodyField::ToolArguments
            | TimelineBodyField::ToolResult
            | TimelineBodyField::ToolFailure
    ) {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    let member_index = cursor.map_or(0, |cursor| cursor.member_index);
    let selected_field = match requested_field {
        TimelineBodyField::ToolArguments => "tool_arguments",
        TimelineBodyField::ToolResult => "tool_result",
        TimelineBodyField::ToolFailure => "tool_failure",
        _ => return Err(SessionTimelineRepositoryError::InvalidDetailQuery),
    };
    let offset_bytes = cursor.map_or(0, |cursor| cursor.offset_bytes);
    let mut query = sqlx::QueryBuilder::<Postgres>::new(TOOL_DETAIL_MEMBERS_SQL);
    query.push(
        ", selected_member AS (
            SELECT request_id, attempt_id, approval_judge_escalated,
                   attempt_state_kind, attempt_terminal_disposition_kind,
                   attempt_error_kind, attempt_has_result, attempt_has_failure,
                   attempt_sandbox_posture, attempt_result_text,
                   attempt_error_detail
              FROM tool_members
             WHERE member_index = $2
        )
        SELECT request.request_id, request.tool_name,
                octet_length(body.content)::numeric AS total_bytes,
                substring(
                    convert_to(body.content, 'UTF8')
                    FROM (least($4::numeric, octet_length(body.content)::numeric) + 1)::integer
                    FOR $5::integer
                ) AS selected_body,
                request.approval_posture, selected.attempt_id,
                attempt.effect_class,
                selected.attempt_state_kind AS state_kind,
                selected.attempt_terminal_disposition_kind
                    AS terminal_disposition_kind,
                selected.attempt_error_kind AS error_kind,
                COALESCE(selected.attempt_has_result, FALSE) AS has_result,
                COALESCE(selected.attempt_has_failure, FALSE) AS has_failure,
                EXISTS (
                    SELECT 1
                      FROM tool_members AS probe
                     WHERE probe.member_index = $2 + 1
                ) AS has_next,
                EXISTS (
                    SELECT 1 FROM goal_members
                ) AS has_goal_events,
                selected.attempt_sandbox_posture AS sandbox_posture,
                selected.approval_judge_escalated AS judge_escalated
           FROM selected_member AS selected
           JOIN tool_request AS request
             ON request.request_id = selected.request_id
           LEFT JOIN tool_attempt AS attempt
             ON attempt.attempt_id = selected.attempt_id
           CROSS JOIN LATERAL (
               SELECT CASE $3::text
                   WHEN 'tool_arguments' THEN request.arguments_text
                   WHEN 'tool_result' THEN selected.attempt_result_text
                   WHEN 'tool_failure' THEN selected.attempt_error_detail
               END AS content
           ) AS body",
    );
    let row = query
        .build()
        .bind(Decimal::from(address.sequence().get()))
        .bind(i64::from(member_index))
        .bind(selected_field)
        .bind(Decimal::from(offset_bytes))
        .bind(i64::from(*remaining) + i64::from(MAX_UTF8_SCALAR_BYTES) - 1)
        .fetch_optional(&mut **transaction)
        .await?;
    let mut tools = Vec::new();
    let mut continuation = None;
    if let Some(row) = row {
        let selected_body: Option<Vec<u8>> = row.try_get("selected_body")?;
        let selected = selected_body.ok_or(SessionTimelineRepositoryError::InvalidDetailQuery)?;
        let total_bytes = nonnegative(row.try_get("total_bytes")?, "tool text byte length")?;
        if offset_bytes > total_bytes {
            return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
        }
        let mut excerpt = bounded_text_excerpt(
            &ModelResponseSlice {
                bytes: selected,
                offset_bytes,
                total_bytes,
            },
            address,
            requested_field,
            remaining,
        )?;
        if let Some(next) = &mut excerpt.continuation {
            next.member_index = member_index;
        }
        let has_result: bool = row.try_get("has_result")?;
        let has_failure: bool = row.try_get("has_failure")?;
        let has_next: bool = row.try_get("has_next")?;
        continuation = excerpt
            .continuation
            .map(TimelineDetailContinuation::MoreBody)
            .or_else(|| {
                next_tool_field(
                    address,
                    requested_field,
                    member_index,
                    has_result,
                    has_failure,
                    has_next,
                )
            });
        let has_goal_events: bool = row.try_get("has_goal_events")?;
        if continuation.is_none() && has_goal_events {
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
        let approval_posture = tool_approval_posture(&approval_posture)?;
        let approval_judge_escalated: bool = row.try_get("judge_escalated")?;
        let attempt_id: Option<uuid::Uuid> = row.try_get("attempt_id")?;
        tools.push(TimelineToolAttempt {
            request_id: ToolRequestId::from_uuid(row.try_get("request_id")?),
            attempt_id: attempt_id.map(ToolAttemptId::from_uuid),
            tool_name: ToolName::try_new(row.try_get("tool_name")?)
                .map_err(|_| SessionTimelineCorruption::InvalidStoredValue("tool name"))?,
            arguments: (requested_field == TimelineBodyField::ToolArguments)
                .then_some(excerpt.clone()),
            result: (requested_field == TimelineBodyField::ToolResult).then_some(excerpt.clone()),
            failure: (requested_field == TimelineBodyField::ToolFailure).then_some(excerpt),
            has_result,
            has_failure,
            approval_posture,
            approval_judge_escalated,
            effect_posture: row
                .try_get::<Option<String>, _>("effect_class")?
                .as_deref()
                .map(tool_effect_posture)
                .transpose()?,
            sandbox_posture: row
                .try_get::<Option<String>, _>("sandbox_posture")?
                .as_deref()
                .map(tool_sandbox_posture)
                .transpose()?,
            state: tool_state(
                attempt_id.is_some(),
                state_kind.as_deref(),
                disposition.as_deref(),
            )?,
            cause_code: row.try_get("error_kind")?,
        });
    } else if cursor.is_some() {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    Ok((
        SessionTimelineDetailBody::ToolBatch {
            turn_id: turn,
            producing_model_call_id: producing_call,
            state: tool_batch_state(state),
            projected_member_index: (!tools.is_empty()).then_some(member_index),
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
        TimelineBodyField::ToolArguments | TimelineBodyField::ToolResult if has_failure => {
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

fn tool_state(
    attempt_present: bool,
    state: Option<&str>,
    disposition: Option<&str>,
) -> Result<Option<TimelineToolState>, SessionTimelineCorruption> {
    match (attempt_present, state, disposition) {
        (false, None, None) => Ok(None),
        (true, Some("prepared"), None) => Ok(Some(TimelineToolState::Prepared)),
        (true, Some("in_flight"), None) => Ok(Some(TimelineToolState::InFlight)),
        (true, Some("terminal"), Some("awaiting_child")) => {
            Ok(Some(TimelineToolState::AwaitingChild))
        }
        (true, Some("terminal"), Some("completed")) => Ok(Some(TimelineToolState::Completed)),
        (true, Some("terminal"), Some("known_failed")) => Ok(Some(TimelineToolState::KnownFailed)),
        (true, Some("terminal"), Some("ambiguous")) => Ok(Some(TimelineToolState::Ambiguous)),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "tool attempt state",
        )),
    }
}

const fn tool_batch_state(state: DispatchedToolBatchState) -> TimelineToolBatchState {
    match state {
        DispatchedToolBatchState::Proposed { frontier } => TimelineToolBatchState::Proposed {
            frontier_id: frontier,
        },
        DispatchedToolBatchState::ResultsProjected { frontier } => {
            TimelineToolBatchState::ResultsProjected {
                frontier_id: frontier,
            }
        }
        DispatchedToolBatchState::RecoveryRequired { attempt } => {
            TimelineToolBatchState::RecoveryRequired {
                attempt_id: attempt,
            }
        }
    }
}

#[derive(Debug)]
struct StoredGoalEvent {
    generation: u64,
    event_kind: String,
    reason: Option<String>,
    outcome: Option<String>,
    text: Option<ModelResponseSlice>,
    has_next: bool,
}

async fn load_goal_event_row(
    transaction: &mut Transaction<'_, Postgres>,
    address: TimelineAddress,
    cursor: TimelineDetailCursor,
    max_bytes: u32,
) -> Result<Option<StoredGoalEvent>, SessionTimelineRepositoryError> {
    let mut query = sqlx::QueryBuilder::<Postgres>::new(TOOL_DETAIL_MEMBERS_SQL);
    query.push(
        " SELECT event.generation, event.event_kind, event.blocked_reason, event.session_outcome_kind,
               octet_length(body.content)::numeric AS total_bytes,
               substring(convert_to(body.content, 'UTF8')
                   FROM (least($3::numeric, octet_length(body.content)::numeric) + 1)::integer
                   FOR $4::integer) AS content_bytes,
               EXISTS (
                   SELECT 1
                     FROM goal_members AS probe
                    WHERE probe.member_index = $2 + 1
               ) AS has_next
          FROM goal_members AS selected
          JOIN goal_event AS event
            ON event.session_id = selected.session_id
           AND event.event_ordinal = selected.goal_event_ordinal
          CROSS JOIN LATERAL (
              SELECT COALESCE(event.statement, event.need, event.guidance, event.report) AS content
          ) AS body
         WHERE selected.member_index = $2",
    );
    let row = query
        .build()
        .bind(Decimal::from(address.sequence().get()))
        .bind(i64::from(cursor.member_index))
        .bind(Decimal::from(cursor.offset_bytes))
        .bind(i64::from(max_bytes) + i64::from(MAX_UTF8_SCALAR_BYTES) - 1)
        .fetch_optional(&mut **transaction)
        .await?;
    row.map(|row| {
        Ok(StoredGoalEvent {
            generation: nonnegative(row.try_get("generation")?, "goal generation")?,
            event_kind: row.try_get("event_kind")?,
            reason: row.try_get("blocked_reason")?,
            outcome: row.try_get("session_outcome_kind")?,
            text: optional_text_slice(&row, cursor.offset_bytes)?,
            has_next: row.try_get("has_next")?,
        })
    })
    .transpose()
}

fn project_tool_goal(
    address: TimelineAddress,
    turn: TurnId,
    producing_call: ModelCallId,
    state: DispatchedToolBatchState,
    cursor: TimelineDetailCursor,
    row: StoredGoalEvent,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    if row.text.is_none() && cursor.offset_bytes != 0 {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    let text = row
        .text
        .as_ref()
        .map(|text| -> Result<_, SessionTimelineRepositoryError> {
            let mut excerpt =
                bounded_text_excerpt(text, address, TimelineBodyField::GoalText, remaining)?;
            if let Some(next) = &mut excerpt.continuation {
                next.member_index = cursor.member_index;
            }
            Ok(excerpt)
        })
        .transpose()?;
    let continuation = text
        .as_ref()
        .and_then(|text| text.continuation)
        .map(TimelineDetailContinuation::MoreBody)
        .or_else(|| {
            row.has_next.then_some(TimelineDetailContinuation::MoreBody(
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
            turn_id: turn,
            producing_model_call_id: producing_call,
            state: tool_batch_state(state),
            projected_member_index: Some(cursor.member_index),
            tools: Vec::new(),
            goal_events: vec![timeline_goal_event(
                row.generation,
                &row.event_kind,
                row.reason.as_deref(),
                row.outcome.as_deref(),
                text,
            )?],
        },
        continuation,
    ))
}

async fn project_tool_approval(
    transaction: &mut Transaction<'_, Postgres>,
    address: TimelineAddress,
    turn: TurnId,
    approval: &ToolApprovalResolution,
    decider: &ToolApprovalDecider,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let row = sqlx::query(
        "SELECT request.tool_name, EXISTS (
             SELECT 1 FROM tool_approval_judge_model_call AS judge
              WHERE judge.request_id = request.request_id
                AND judge.recommendation_kind = 'escalate_to_human'
         ) AS judge_escalated
           FROM tool_request AS request
          WHERE request.request_id = $1",
    )
    .bind(approval.request().into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    let tool_name = ToolName::try_new(row.try_get("tool_name")?)
        .map_err(|_| SessionTimelineCorruption::InvalidStoredValue("tool name"))?;
    let approval_judge_escalated: bool = row.try_get("judge_escalated")?;
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
        ToolApprovalDecision::Approve => TimelineApprovalDecision::Approve,
        ToolApprovalDecision::Deny { .. } => TimelineApprovalDecision::Deny,
    };
    let actor = match (approval.source(), decider) {
        (ToolDecisionSource::UserCommand, ToolApprovalDecider::User { command }) => {
            TimelineApprovalActor::User {
                command_id: *command,
            }
        }
        (ToolDecisionSource::Delegate, ToolApprovalDecider::Delegate { model, call }) => {
            TimelineApprovalActor::Delegate {
                model_selection_id: *model,
                model_call_id: *call,
            }
        }
        (
            ToolDecisionSource::UserOverride,
            ToolApprovalDecider::UserOverride {
                command,
                denied_request,
            },
        ) => TimelineApprovalActor::UserOverride {
            command_id: *command,
            denied_request_id: *denied_request,
        },
        (ToolDecisionSource::RuntimeSafety, _)
        | (ToolDecisionSource::LifecycleClosure, _)
        | (ToolDecisionSource::PolicyAuto, _)
        | (ToolDecisionSource::SessionBlanket, _)
        | (ToolDecisionSource::SessionOverride, _) => TimelineApprovalActor::Policy,
        (
            ToolDecisionSource::UserCommand | ToolDecisionSource::Delegate,
            ToolApprovalDecider::UserOverride { .. },
        )
        | (
            ToolDecisionSource::UserOverride,
            ToolApprovalDecider::User { .. } | ToolApprovalDecider::Delegate { .. },
        )
        | (ToolDecisionSource::UserCommand, ToolApprovalDecider::Delegate { .. })
        | (ToolDecisionSource::Delegate, ToolApprovalDecider::User { .. }) => {
            return Err(
                SessionTimelineCorruption::InvalidStoredValue("tool approval actor").into(),
            );
        }
    };
    Ok((
        SessionTimelineDetailBody::ToolApprovalDecision {
            turn_id: turn,
            request_id: approval.request(),
            tool_name,
            decision,
            actor,
            rationale,
            approval_judge_escalated,
        },
        continuation,
    ))
}

const fn reconciliation_operation(
    operation: DispatchedReconciliationOperation,
) -> TimelineReconciliationOperation {
    match operation {
        DispatchedReconciliationOperation::ModelCall(call) => {
            TimelineReconciliationOperation::ModelCall(call)
        }
        DispatchedReconciliationOperation::ToolAttempt(attempt) => {
            TimelineReconciliationOperation::ToolAttempt(attempt)
        }
    }
}

fn tool_approval_posture(
    value: &str,
) -> Result<TimelineToolApprovalPosture, SessionTimelineCorruption> {
    match value {
        "auto" => Ok(TimelineToolApprovalPosture::Auto),
        "delegated" => Ok(TimelineToolApprovalPosture::Delegated),
        "human" => Ok(TimelineToolApprovalPosture::Human),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "tool approval posture",
        )),
    }
}

fn tool_effect_posture(
    value: &str,
) -> Result<TimelineToolEffectPosture, SessionTimelineCorruption> {
    match value {
        "effect_free" => Ok(TimelineToolEffectPosture::EffectFree),
        "external_effect" => Ok(TimelineToolEffectPosture::ExternalEffect),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "tool effect posture",
        )),
    }
}

fn tool_sandbox_posture(
    value: &str,
) -> Result<TimelineToolSandboxPosture, SessionTimelineCorruption> {
    match value {
        "unsandboxed" => Ok(TimelineToolSandboxPosture::Unsandboxed),
        "sandboxed" => Ok(TimelineToolSandboxPosture::Sandboxed),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "tool sandbox posture",
        )),
    }
}

fn runner_sandbox(sandbox: RunnerSandboxProfile) -> TimelineRunnerSandboxPosture {
    match sandbox {
        RunnerSandboxProfile::Ambient => TimelineRunnerSandboxPosture::Unsandboxed,
        RunnerSandboxProfile::WorkspaceRestricted => TimelineRunnerSandboxPosture::Sandboxed,
    }
}

fn runner_state(state: DispatchedRunnerState) -> TimelineRunnerState {
    match state {
        DispatchedRunnerState::Pinned => TimelineRunnerState::Pinned,
        DispatchedRunnerState::Suspect => TimelineRunnerState::Suspect,
        DispatchedRunnerState::Connected => TimelineRunnerState::Connected,
        DispatchedRunnerState::RunnerLostBeforePin => TimelineRunnerState::RunnerLostBeforePin,
        DispatchedRunnerState::RunnerLost => TimelineRunnerState::RunnerLost,
        DispatchedRunnerState::Replaced => TimelineRunnerState::Replaced,
        DispatchedRunnerState::WorkingDirectoryChanged => {
            TimelineRunnerState::WorkingDirectoryChanged
        }
        DispatchedRunnerState::Abandoned => TimelineRunnerState::Abandoned,
    }
}

fn project_delegation_update(
    address: TimelineAddress,
    update: &DispatchedDelegationUpdate,
    content_slice: Option<&ModelResponseSlice>,
    cursor: Option<TimelineDetailCursor>,
    remaining: &mut u32,
) -> Result<
    (
        SessionTimelineDetailBody,
        Option<TimelineDetailContinuation>,
    ),
    SessionTimelineRepositoryError,
> {
    let (detail, continuation) = match update {
        DispatchedDelegationUpdate::ChildSpawned {
            spawning_request,
            child,
            policy,
        } => {
            require_no_body_cursor(cursor)?;
            (
                TimelineDelegationDetail::ChildSpawned {
                    relationship_id: *spawning_request,
                    child: *child,
                    policy: delegation_policy(*policy),
                },
                None,
            )
        }
        DispatchedDelegationUpdate::ChildWaiting {
            spawning_request,
            child,
            awaiting_request,
            mode,
        } => {
            require_no_body_cursor(cursor)?;
            (
                TimelineDelegationDetail::ChildWaiting {
                    relationship_id: *spawning_request,
                    child: *child,
                    awaiting_request: *awaiting_request,
                    mode: delegation_wait_mode(*mode),
                },
                None,
            )
        }
        DispatchedDelegationUpdate::ChildLifecycleDisposition {
            spawning_request,
            child,
            event_ordinal,
            outcome,
            reason,
            provenance,
        } => {
            require_no_body_cursor(cursor)?;
            (
                TimelineDelegationDetail::ChildLifecycleDisposition {
                    relationship_id: *spawning_request,
                    child: *child,
                    event_ordinal: *event_ordinal,
                    outcome: delegation_outcome(*outcome),
                    reason: delegation_reason(*reason),
                    provenance: delegation_provenance(*provenance),
                },
                None,
            )
        }
        DispatchedDelegationUpdate::ChildResult {
            spawning_request,
            child,
            outcome,
            reason,
            provenance,
            ..
        } => {
            let content = match content_slice {
                Some(content) => {
                    require_cursor_field(cursor, TimelineBodyField::DelegationContent, 0)?;
                    Some(bounded_text_excerpt(
                        content,
                        address,
                        TimelineBodyField::DelegationContent,
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
            (
                TimelineDelegationDetail::ChildResult {
                    relationship_id: *spawning_request,
                    child: *child,
                    outcome: delegation_outcome(*outcome),
                    reason: delegation_reason(*reason),
                    provenance: delegation_provenance(*provenance),
                    content,
                },
                continuation,
            )
        }
        DispatchedDelegationUpdate::SessionMessage {
            spawning_request,
            message,
            sender,
            recipient,
            message_ordinal,
            delivery_sequence,
            ..
        } => {
            require_cursor_field(cursor, TimelineBodyField::DelegationContent, 0)?;
            let content = bounded_text_excerpt(
                content_slice.ok_or(SessionTimelineCorruption::MissingDetailRecord)?,
                address,
                TimelineBodyField::DelegationContent,
                remaining,
            )?;
            let continuation = content
                .continuation
                .map(TimelineDetailContinuation::MoreBody);
            (
                TimelineDelegationDetail::SessionMessage {
                    relationship_id: *spawning_request,
                    message: *message,
                    sender: *sender,
                    recipient: *recipient,
                    message_ordinal: *message_ordinal,
                    delivery_sequence: *delivery_sequence,
                    content,
                },
                continuation,
            )
        }
    };
    Ok((SessionTimelineDetailBody::Delegation(detail), continuation))
}

fn delegation_wake_body(wake: DispatchedDelegationWake) -> SessionTimelineDetailBody {
    let detail = match wake {
        DispatchedDelegationWake::Result {
            spawning_request,
            awaiting_request,
        } => TimelineDelegationDetail::ResultWake {
            relationship_id: spawning_request,
            awaiting_request,
        },
        DispatchedDelegationWake::Message {
            spawning_request,
            message,
        } => TimelineDelegationDetail::MessageWake {
            relationship_id: spawning_request,
            message,
        },
    };
    SessionTimelineDetailBody::Delegation(detail)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredGoalEventKind {
    Commissioned,
    Blocked,
    Resumed,
    Achieved,
    UserStopped,
    Superseded,
}

fn goal_event_kind(value: &str) -> Result<StoredGoalEventKind, SessionTimelineCorruption> {
    match value {
        "commissioned" => Ok(StoredGoalEventKind::Commissioned),
        "blocked" => Ok(StoredGoalEventKind::Blocked),
        "resumed" => Ok(StoredGoalEventKind::Resumed),
        "achieved" => Ok(StoredGoalEventKind::Achieved),
        "user_stopped" => Ok(StoredGoalEventKind::UserStopped),
        "superseded" => Ok(StoredGoalEventKind::Superseded),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "goal event kind",
        )),
    }
}

fn goal_blocked_reason(
    value: &str,
) -> Result<TimelineGoalBlockedReason, SessionTimelineCorruption> {
    match value {
        "user_input_required" => Ok(TimelineGoalBlockedReason::UserInputRequired),
        "external_change_required" => Ok(TimelineGoalBlockedReason::ExternalChangeRequired),
        "authorization_required" => Ok(TimelineGoalBlockedReason::AuthorizationRequired),
        "execution_failure" => Ok(TimelineGoalBlockedReason::ExecutionFailure),
        "finish_check_failed" => Ok(TimelineGoalBlockedReason::FinishCheckFailed),
        _ => Err(SessionTimelineCorruption::InvalidStoredValue(
            "goal blocked reason",
        )),
    }
}

const fn delegation_policy(policy: DispatchedDelegationPolicy) -> TimelineDelegationPolicy {
    match policy {
        DispatchedDelegationPolicy::Background => TimelineDelegationPolicy::Background,
        DispatchedDelegationPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => TimelineDelegationPolicy::Bound {
            on_parent_stopped: bound_child_action(on_parent_stopped),
            on_parent_cancelled: bound_child_action(on_parent_cancelled),
        },
    }
}

const fn bound_child_action(action: DispatchedBoundChildAction) -> TimelineBoundChildAction {
    match action {
        DispatchedBoundChildAction::KeepRunning => TimelineBoundChildAction::KeepRunning,
        DispatchedBoundChildAction::Stop => TimelineBoundChildAction::Stop,
        DispatchedBoundChildAction::Cancel => TimelineBoundChildAction::Cancel,
    }
}

const fn delegation_wait_mode(mode: DispatchedDelegationWaitMode) -> TimelineDelegationWaitMode {
    match mode {
        DispatchedDelegationWaitMode::Foreground => TimelineDelegationWaitMode::Foreground,
        DispatchedDelegationWaitMode::Background => TimelineDelegationWaitMode::Background,
    }
}

const fn delegation_outcome(outcome: DispatchedDelegationOutcome) -> TimelineDelegationOutcome {
    match outcome {
        DispatchedDelegationOutcome::ResultReturned => TimelineDelegationOutcome::ResultReturned,
        DispatchedDelegationOutcome::ChildFailed => TimelineDelegationOutcome::ChildFailed,
        DispatchedDelegationOutcome::ChildStopped => TimelineDelegationOutcome::ChildStopped,
        DispatchedDelegationOutcome::ChildCancelled => TimelineDelegationOutcome::ChildCancelled,
        DispatchedDelegationOutcome::ContinueRunning => TimelineDelegationOutcome::ContinueRunning,
        DispatchedDelegationOutcome::AlreadyTerminal => TimelineDelegationOutcome::AlreadyTerminal,
    }
}

const fn delegation_reason(reason: DispatchedDelegationReason) -> TimelineDelegationReason {
    match reason {
        DispatchedDelegationReason::ChildCompleted => TimelineDelegationReason::ChildCompleted,
        DispatchedDelegationReason::ChildExecutionFailed => {
            TimelineDelegationReason::ChildExecutionFailed
        }
        DispatchedDelegationReason::ChildResultUnavailable => {
            TimelineDelegationReason::ChildResultUnavailable
        }
        DispatchedDelegationReason::ChildCancelled => TimelineDelegationReason::ChildCancelled,
        DispatchedDelegationReason::ParentStoppedWithDescendants => {
            TimelineDelegationReason::ParentStoppedWithDescendants
        }
        DispatchedDelegationReason::ParentCancelledWithDescendants => {
            TimelineDelegationReason::ParentCancelledWithDescendants
        }
    }
}

const fn delegation_provenance(
    provenance: DispatchedDelegationProvenance,
) -> TimelineDelegationProvenance {
    match provenance {
        DispatchedDelegationProvenance::ParentLifecycleCommand { session, command } => {
            TimelineDelegationProvenance::ParentLifecycleCommand { session, command }
        }
        DispatchedDelegationProvenance::ChildTurn { session, turn } => {
            TimelineDelegationProvenance::ChildTurn { session, turn }
        }
        DispatchedDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        } => TimelineDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        },
        DispatchedDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        } => TimelineDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        },
    }
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
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
}

fn optional_text_slice(
    row: &sqlx::postgres::PgRow,
    offset_bytes: u64,
) -> Result<Option<ModelResponseSlice>, SessionTimelineRepositoryError> {
    let Some(bytes) = row.try_get::<Option<Vec<u8>>, _>("content_bytes")? else {
        return Ok(None);
    };
    let total_bytes = nonnegative(row.try_get("total_bytes")?, "detail text byte length")?;
    if offset_bytes > total_bytes {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    Ok(Some(ModelResponseSlice {
        bytes,
        offset_bytes,
        total_bytes,
    }))
}

#[derive(Debug)]
struct ModelResponseSlice {
    bytes: Vec<u8>,
    offset_bytes: u64,
    total_bytes: u64,
}

async fn load_model_detail(
    transaction: &mut Transaction<'_, Postgres>,
    call: signalbox_domain::ModelCallId,
    include_response: bool,
    include_terminal_evidence: bool,
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
           SELECT entry.assistant_response_text_start_bytes
                  + octet_length(entry.assistant_text_value)::numeric
             FROM semantic_transcript_entry AS entry
            WHERE entry.source_session_id = call.session_id
              AND entry.producing_model_call_id = call.model_call_id
              AND entry.payload_kind = 'assistant_text'
              AND $2::boolean
            ORDER BY entry.assistant_response_text_start_bytes DESC
            LIMIT 1
       ) AS response_total_bytes
  FROM model_call AS call
  JOIN context_frontier AS frontier
    ON frontier.owning_session_id = call.session_id
   AND frontier.context_frontier_id = call.context_frontier_id
 WHERE call.model_call_id = $1
"#,
    )
    .bind(call.into_uuid())
    .bind(include_response)
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
SELECT substring(
           convert_to(entry.assistant_text_value, 'UTF8')
           FROM (
               greatest($3::numeric, entry.assistant_response_text_start_bytes)
               - entry.assistant_response_text_start_bytes + 1
           )::integer
           FOR (
               least(
                   entry.assistant_response_text_start_bytes
                       + octet_length(entry.assistant_text_value)::numeric,
                   $4::numeric
               )
               - greatest($3::numeric, entry.assistant_response_text_start_bytes)
           )::integer
       )
  FROM semantic_transcript_entry AS entry
 WHERE entry.source_session_id = $1
   AND entry.producing_model_call_id = $2
   AND entry.payload_kind = 'assistant_text'
   AND entry.assistant_response_text_start_bytes < $4::numeric
   AND entry.assistant_response_text_start_bytes
           + octet_length(entry.assistant_text_value)::numeric > $3::numeric
 ORDER BY entry.assistant_response_part_ordinal
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
            input_tokens: if include_terminal_evidence {
                optional_nonnegative(row.try_get("usage_input_tokens")?, "input usage")?
            } else {
                None
            },
            output_tokens: if include_terminal_evidence {
                optional_nonnegative(row.try_get("usage_output_tokens")?, "output usage")?
            } else {
                None
            },
            cache_creation_input_tokens: if include_terminal_evidence {
                optional_nonnegative(
                    row.try_get("usage_cache_creation_input_tokens")?,
                    "cache creation usage",
                )?
            } else {
                None
            },
            cache_read_input_tokens: if include_terminal_evidence {
                optional_nonnegative(
                    row.try_get("usage_cache_read_input_tokens")?,
                    "cache read usage",
                )?
            } else {
                None
            },
        },
        provider_failure_cause: if include_terminal_evidence {
            row.try_get::<Option<String>, _>("terminal_provider_failure_cause")?
                .map(|value| provider_failure_cause_from_str(&value))
                .transpose()?
        } else {
            None
        },
    })
}

fn provider_failure_cause_from_str(
    value: &str,
) -> Result<ProviderModelCallFailureCause, SessionTimelineRepositoryError> {
    if value == "credential_rejected" {
        return Ok(ProviderModelCallFailureCause::CredentialRejected);
    }
    match value {
        "permission_denied" => Ok(ProviderModelCallFailureCause::PermissionDenied),
        "invalid_request" => Ok(ProviderModelCallFailureCause::InvalidRequest),
        "target_not_found" => Ok(ProviderModelCallFailureCause::TargetNotFound),
        "request_too_large" => Ok(ProviderModelCallFailureCause::RequestTooLarge),
        "rate_limited" => Ok(ProviderModelCallFailureCause::RateLimited),
        "quota_exhausted" => Ok(ProviderModelCallFailureCause::QuotaExhausted),
        "overloaded" => Ok(ProviderModelCallFailureCause::Overloaded),
        "provider_internal" => Ok(ProviderModelCallFailureCause::ProviderInternal),
        "unrecognized" => Ok(ProviderModelCallFailureCause::Unrecognized),
        value => Err(SessionTimelineCorruption::UnsupportedEventKind(value.to_owned()).into()),
    }
}

fn call_session_uuid(row: &sqlx::postgres::PgRow) -> Result<uuid::Uuid, sqlx::Error> {
    row.try_get("session_id")
}

fn bounded_text_excerpt(
    response: &ModelResponseSlice,
    address: TimelineAddress,
    field: TimelineBodyField,
    remaining: &mut u32,
) -> Result<TimelineTextExcerpt, SessionTimelineRepositoryError> {
    // A cursor that lands inside a multi-byte scalar makes the slice begin on a
    // UTF-8 continuation byte. Stored text is validated UTF-8, so at a nonzero
    // offset that shape proves a malformed client offset rather than corrupt
    // stored text, and the detail contract requires offsets to be scalar
    // boundaries. Classifying it as a query defect keeps the corruption arm
    // reserved for genuinely unreadable stored bytes.
    if response.offset_bytes > 0
        && response
            .bytes
            .first()
            .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
    {
        return Err(SessionTimelineRepositoryError::InvalidDetailQuery);
    }
    let valid_bytes = match std::str::from_utf8(&response.bytes) {
        Ok(_) => response.bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return Err(SessionTimelineRepositoryError::InvalidStoredUtf8),
    };
    let available = usize::try_from(*remaining)
        .map_err(|_| SessionTimelineCorruption::DetailProjectionOverflow)?;
    let mut selected = valid_bytes.min(available);
    while selected > 0 && std::str::from_utf8(&response.bytes[..selected]).is_err() {
        selected -= 1;
    }
    let text = std::str::from_utf8(&response.bytes[..selected])
        .map_err(|_| SessionTimelineRepositoryError::InvalidStoredUtf8)?
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
    // A continuation at the offset the caller already asked for never advances:
    // the caller re-requests the same page forever, or treats the address as
    // delivered and loses the body. The page budget reserves
    // MAX_UTF8_SCALAR_BYTES per admitted item so this cannot arise from a legal
    // read; anything that reaches it is a broken budget, so fail closed rather
    // than hand back a cursor that cannot make progress.
    if next_offset == response.offset_bytes && next_offset < response.total_bytes {
        return Err(SessionTimelineCorruption::DetailProjectionOverflow.into());
    }
    let continuation = (next_offset < response.total_bytes).then_some(TimelineBodyContinuation {
        address,
        field,
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

fn response_excerpt(
    response: ModelResponseSlice,
    address: TimelineAddress,
    remaining: &mut u32,
) -> Result<TimelineTextExcerpt, SessionTimelineRepositoryError> {
    bounded_text_excerpt(
        &response,
        address,
        TimelineBodyField::ModelResponse,
        remaining,
    )
}

fn dispatched_event_kind(kind: &DispatchedOutboxEventKind) -> SessionTimelineEventKind {
    match kind {
        DispatchedOutboxEventKind::AutomaticReconciliationExhausted(_) => {
            SessionTimelineEventKind::AutomaticReconciliationExhausted
        }
        DispatchedOutboxEventKind::CredentialPoolExhausted(_) => {
            SessionTimelineEventKind::TurnFailed
        }
        DispatchedOutboxEventKind::SessionCreated(_) => SessionTimelineEventKind::SessionCreated,
        DispatchedOutboxEventKind::SessionStateChanged(_) => {
            SessionTimelineEventKind::SessionStateChanged
        }
        DispatchedOutboxEventKind::SessionTerminal(_) => SessionTimelineEventKind::SessionTerminal,
        DispatchedOutboxEventKind::GoalChanged(_) => SessionTimelineEventKind::GoalChanged,
        DispatchedOutboxEventKind::CommandSettled { .. } => {
            SessionTimelineEventKind::CommandSettled
        }
        DispatchedOutboxEventKind::InjectionSettled { .. } => {
            SessionTimelineEventKind::InjectionSettled
        }
        DispatchedOutboxEventKind::SessionOwnershipChanged(_) => {
            SessionTimelineEventKind::SessionOwnershipChanged
        }
        DispatchedOutboxEventKind::SessionModelSettingsChanged(_) => {
            SessionTimelineEventKind::SessionModelSettingsChanged
        }
        DispatchedOutboxEventKind::TurnModelSettingsResolved(_) => {
            SessionTimelineEventKind::TurnModelSettingsResolved
        }
        DispatchedOutboxEventKind::InputAccepted { .. } => SessionTimelineEventKind::InputAccepted,
        DispatchedOutboxEventKind::TurnActivated { .. } => SessionTimelineEventKind::TurnActivated,
        DispatchedOutboxEventKind::TurnTerminal { disposition, .. } => {
            terminal_turn_kind(disposition_storage_kind(disposition))
        }
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
SELECT session.session_id, workspace.workspace_root_kind,
       facts.session_id IS NOT NULL AS facts_present,
       facts.item_count, facts.first_sequence,
       facts.latest_sequence,
       facts.item_count * $2 + facts.event_kind_bytes AS structured_bytes,
       facts.projected_text_bytes AS text_bytes,
       facts.active_turn_count AS active_count, facts.queued_turn_count AS queued_count,
       (SELECT last_sequence FROM outbox_sequence_state WHERE singleton) AS last_sequence
  FROM session
  LEFT JOIN session_timeline_fact AS facts USING (session_id)
  LEFT JOIN session_workspace_binding AS workspace USING (session_id)
 WHERE session.session_id = $1
"#;

macro_rules! window_sql {
    ($tail:literal) => {
        concat!(
            "WITH session_events AS (",
            "SELECT event_sequence, event_kind, turn_disposition FROM session_timeline_item WHERE session_id = $1",
            ") ",
            $tail
        )
    };
}

const FIRST_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind, turn_disposition FROM session_events ORDER BY event_sequence ASC LIMIT $2"
);
const LATEST_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind, turn_disposition FROM session_events ORDER BY event_sequence DESC LIMIT $2"
);
const BEFORE_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind, turn_disposition FROM session_events WHERE event_sequence < $2 ORDER BY event_sequence DESC LIMIT $3"
);
const AFTER_WINDOW_SQL: &str = window_sql!(
    "SELECT event_sequence, event_kind, turn_disposition FROM session_events WHERE event_sequence > $2 ORDER BY event_sequence ASC LIMIT $3"
);
const AROUND_WINDOW_SQL: &str = r#"
WITH before_events AS (
    SELECT event_sequence, event_kind, turn_disposition FROM session_timeline_item
     WHERE session_id = $1 AND event_sequence <= $2
     ORDER BY event_sequence DESC LIMIT $3
), after_events AS (
    SELECT event_sequence, event_kind, turn_disposition FROM session_timeline_item
     WHERE session_id = $1 AND event_sequence > $2
     ORDER BY event_sequence ASC LIMIT $3
), candidates AS (
    SELECT event_sequence, event_kind, turn_disposition FROM before_events
    UNION ALL
    SELECT event_sequence, event_kind, turn_disposition FROM after_events
)
SELECT event_sequence, event_kind, turn_disposition FROM candidates
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
    let item_count = nonnegative(row.try_get("item_count")?, "item count")?;
    if item_count == 0 {
        return Err(SessionTimelineCorruption::InvalidOrdinal("item count").into());
    }
    let first = optional_address(row.try_get("first_sequence")?, "first address")?
        .ok_or(SessionTimelineCorruption::Missing("first address"))?;
    let latest = optional_address(row.try_get("latest_sequence")?, "latest address")?
        .ok_or(SessionTimelineCorruption::Missing("latest address"))?;
    let observed_through = nonnegative(
        row.try_get::<Option<Decimal>, _>("last_sequence")?
            .ok_or(SessionTimelineCorruption::Missing("observation cursor"))?,
        "observation cursor",
    )?;
    let address_span = latest
        .sequence()
        .get()
        .checked_sub(first.sequence().get())
        .and_then(|span| span.checked_add(1));
    if first > latest
        || (item_count == 1 && first != latest)
        || latest.sequence().get() > observed_through
        || address_span.is_none_or(|span| item_count > span)
    {
        return Err(SessionTimelineCorruption::InvalidOrdinal("timeline bounds").into());
    }
    let projected_structured_bytes = nonnegative(
        row.try_get("structured_bytes")?,
        "projected structured bytes",
    )?;
    let minimum_item_bytes = u64::from(PROJECTED_ITEM_ENVELOPE_BYTES)
        .checked_add(OUTBOX_EVENT_KIND_UTF8_BYTE_BOUNDS.0)
        .ok_or(SessionTimelineCorruption::ItemProjectionOverflow)?;
    let maximum_item_bytes = u64::from(PROJECTED_ITEM_ENVELOPE_BYTES)
        .checked_add(OUTBOX_EVENT_KIND_UTF8_BYTE_BOUNDS.1)
        .ok_or(SessionTimelineCorruption::ItemProjectionOverflow)?;
    let minimum_structured_bytes = item_count
        .checked_mul(minimum_item_bytes)
        .ok_or(SessionTimelineCorruption::ItemProjectionOverflow)?;
    let maximum_structured_bytes = item_count
        .checked_mul(maximum_item_bytes)
        .ok_or(SessionTimelineCorruption::ItemProjectionOverflow)?;
    if projected_structured_bytes < minimum_structured_bytes
        || projected_structured_bytes > maximum_structured_bytes
    {
        return Err(SessionTimelineCorruption::InvalidOrdinal("projected structured bytes").into());
    }
    Ok(Some(SessionTimelineDescriptor {
        workspace_root_kind: crate::session_workspace::decode(row.try_get("workspace_root_kind")?)?,
        session,
        sizes: SessionTimelineSizeFacts {
            item_count,
            projected_text_bytes: nonnegative(row.try_get("text_bytes")?, "projected text bytes")?,
            projected_structured_bytes,
            referenced_blob_count: 0,
            referenced_blob_bytes: 0,
        },
        bounds: SessionTimelineBounds {
            first: Some(first),
            latest: Some(latest),
        },
        work: SessionWorkFacts {
            active_turn_count: nonnegative(row.try_get("active_count")?, "active turn count")?,
            queued_turn_count: nonnegative(row.try_get("queued_count")?, "queued turn count")?,
        },
        observed_through,
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
    let disposition: Option<String> = row.try_get("turn_disposition")?;
    let (kind, kind_text) = decode_kind(&kind_text, disposition.as_deref())?;
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

/// Decodes one header into its timeline kind and the spelling that kind is
/// projected under, which is what the item's byte accounting charges.
fn decode_kind(
    value: &str,
    turn_disposition: Option<&str>,
) -> Result<(SessionTimelineEventKind, &'static str), SessionTimelineCorruption> {
    let discriminator = outbox_event_discriminator_from_str(value)
        .ok_or_else(|| SessionTimelineCorruption::UnsupportedEventKind(value.to_owned()))?;
    let disposition = match (discriminator, turn_disposition) {
        (OutboxEventDiscriminator::TurnTerminal, Some(disposition)) => {
            Some(turn_disposition_kind_from_str(disposition).ok_or_else(|| {
                SessionTimelineCorruption::UnsupportedTurnDisposition(disposition.to_owned())
            })?)
        }
        (OutboxEventDiscriminator::TurnTerminal, None) | (_, Some(_)) => {
            return Err(SessionTimelineCorruption::Inconsistent("turn disposition"));
        }
        (_, None) => None,
    };
    let kind = match (discriminator, disposition) {
        (OutboxEventDiscriminator::AutomaticReconciliationExhausted, _) => {
            SessionTimelineEventKind::AutomaticReconciliationExhausted
        }
        (OutboxEventDiscriminator::CredentialPoolExhausted, _) => {
            SessionTimelineEventKind::TurnFailed
        }
        (OutboxEventDiscriminator::SessionCreated, _) => SessionTimelineEventKind::SessionCreated,
        (OutboxEventDiscriminator::SessionStateChanged, _) => {
            SessionTimelineEventKind::SessionStateChanged
        }
        (OutboxEventDiscriminator::SessionTerminal, _) => SessionTimelineEventKind::SessionTerminal,
        (OutboxEventDiscriminator::GoalChanged, _) => SessionTimelineEventKind::GoalChanged,
        (OutboxEventDiscriminator::CommandSettled, _) => SessionTimelineEventKind::CommandSettled,
        (OutboxEventDiscriminator::InjectionSettled, _) => {
            SessionTimelineEventKind::InjectionSettled
        }
        (OutboxEventDiscriminator::SessionOwnershipChanged, _) => {
            SessionTimelineEventKind::SessionOwnershipChanged
        }
        (OutboxEventDiscriminator::SessionModelSettingsChanged, _) => {
            SessionTimelineEventKind::SessionModelSettingsChanged
        }
        (OutboxEventDiscriminator::TurnModelSettingsResolved, _) => {
            SessionTimelineEventKind::TurnModelSettingsResolved
        }
        (OutboxEventDiscriminator::InputAccepted, _) => SessionTimelineEventKind::InputAccepted,
        (OutboxEventDiscriminator::TurnActivated, _) => SessionTimelineEventKind::TurnActivated,
        (OutboxEventDiscriminator::TurnTerminal, Some(disposition)) => {
            terminal_turn_kind(disposition)
        }
        (OutboxEventDiscriminator::TurnTerminal, None) => {
            return Err(SessionTimelineCorruption::Inconsistent("turn disposition"));
        }
        (OutboxEventDiscriminator::ModelCallTransition, _) => {
            SessionTimelineEventKind::ModelCallTransition
        }
        (OutboxEventDiscriminator::ToolBatchTransition, _) => {
            SessionTimelineEventKind::ToolBatchTransition
        }
        (OutboxEventDiscriminator::ToolApprovalDecided, _) => {
            SessionTimelineEventKind::ToolApprovalDecided
        }
        (OutboxEventDiscriminator::ContextCompacted, _) => {
            SessionTimelineEventKind::ContextCompacted
        }
        (OutboxEventDiscriminator::RunnerStateTransition, _) => {
            SessionTimelineEventKind::RunnerStateTransition
        }
        (OutboxEventDiscriminator::DelegationUpdate, _) => {
            SessionTimelineEventKind::DelegationUpdate
        }
        (OutboxEventDiscriminator::DelegationWake, _) => SessionTimelineEventKind::DelegationWake,
    };
    let spelling = timeline_event_kind_str(discriminator, disposition)
        .ok_or(SessionTimelineCorruption::Inconsistent("turn disposition"))?;
    Ok((kind, spelling))
}

/// The timeline kind a `turn_terminal` header projects under.
const fn terminal_turn_kind(disposition: TurnDispositionStorageKind) -> SessionTimelineEventKind {
    match disposition {
        TurnDispositionStorageKind::Completed => SessionTimelineEventKind::TurnCompleted,
        TurnDispositionStorageKind::Refused => SessionTimelineEventKind::TurnRefused,
        TurnDispositionStorageKind::Failed => SessionTimelineEventKind::TurnFailed,
        TurnDispositionStorageKind::Cancelled => SessionTimelineEventKind::TurnCancelled,
        TurnDispositionStorageKind::ReconciliationRequired => {
            SessionTimelineEventKind::TurnReconciliationRequired
        }
        TurnDispositionStorageKind::Retired => SessionTimelineEventKind::GoalTurnRetired,
    }
}

const fn disposition_storage_kind(
    disposition: &DispatchedTurnTerminalDisposition,
) -> TurnDispositionStorageKind {
    match disposition {
        DispatchedTurnTerminalDisposition::Completed { .. } => {
            TurnDispositionStorageKind::Completed
        }
        DispatchedTurnTerminalDisposition::Refused { .. } => TurnDispositionStorageKind::Refused,
        DispatchedTurnTerminalDisposition::Failed { .. } => TurnDispositionStorageKind::Failed,
        DispatchedTurnTerminalDisposition::Cancelled { .. } => {
            TurnDispositionStorageKind::Cancelled
        }
        DispatchedTurnTerminalDisposition::ReconciliationRequired { .. } => {
            TurnDispositionStorageKind::ReconciliationRequired
        }
        DispatchedTurnTerminalDisposition::Retired => TurnDispositionStorageKind::Retired,
    }
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
            decode_kind("delegation_wake", None),
            Ok((SessionTimelineEventKind::DelegationWake, "delegation_wake"))
        );
        assert_eq!(
            decode_kind("turn_terminal", Some("retired")),
            Ok((
                SessionTimelineEventKind::GoalTurnRetired,
                "goal_turn_retired"
            ))
        );
        assert!(matches!(
            decode_kind("future_event", None),
            Err(SessionTimelineCorruption::UnsupportedEventKind(_))
        ));
        assert!(matches!(
            decode_kind("turn_terminal", None),
            Err(SessionTimelineCorruption::Inconsistent(_))
        ));
        assert!(matches!(
            decode_kind("turn_activated", Some("completed")),
            Err(SessionTimelineCorruption::Inconsistent(_))
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
    fn model_response_slice_rejects_a_zero_progress_body_cursor() {
        let address =
            TimelineAddress::new(NonZeroU64::new(11).expect("fixture address is positive"));
        let response = ModelResponseSlice {
            bytes: "abc".as_bytes().to_vec(),
            offset_bytes: 0,
            total_bytes: 3,
        };
        // An exhausted body budget would otherwise select an empty excerpt and
        // hand back a continuation at offset 0 — the offset the caller already
        // asked for.
        let mut budget = 0;

        assert!(matches!(
            response_excerpt(response, address, &mut budget),
            Err(SessionTimelineRepositoryError::Corruption(
                SessionTimelineCorruption::DetailProjectionOverflow
            ))
        ));
    }

    #[test]
    fn model_response_slice_rejects_a_budget_below_one_scalar() {
        let address =
            TimelineAddress::new(NonZeroU64::new(12).expect("fixture address is positive"));
        let response = ModelResponseSlice {
            bytes: "\u{20ac}z".as_bytes().to_vec(),
            offset_bytes: 0,
            total_bytes: 4,
        };
        // One byte cannot seat the three-byte leading scalar, so the UTF-8
        // backoff empties the excerpt and the cursor would not advance.
        let mut budget = 1;

        assert!(matches!(
            response_excerpt(response, address, &mut budget),
            Err(SessionTimelineRepositoryError::Corruption(
                SessionTimelineCorruption::DetailProjectionOverflow
            ))
        ));
    }

    #[test]
    fn a_page_admits_an_item_only_with_room_for_one_scalar() {
        // The page guard reserves the envelope plus the widest single scalar,
        // which is what makes the two cases above unreachable from a legal read.
        assert_eq!(
            DETAIL_PROGRESS_BYTES,
            DETAIL_ENVELOPE_BYTES + MAX_UTF8_SCALAR_BYTES
        );
        assert!(MAX_UTF8_SCALAR_BYTES as usize == "\u{10348}".len());
        assert!(signalbox_application::min_timeline_detail_bytes() >= DETAIL_PROGRESS_BYTES);
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
            Err(SessionTimelineRepositoryError::InvalidStoredUtf8)
        ));
    }

    #[test]
    fn a_mid_scalar_cursor_is_a_query_defect_rather_than_corruption() {
        let address =
            TimelineAddress::new(NonZeroU64::new(11).expect("fixture address is positive"));
        // "€" encodes as E2 82 AC; a cursor one byte into it slices from the
        // continuation byte 0x82, which the production read must classify as a
        // malformed offset rather than unreadable stored text.
        let response = ModelResponseSlice {
            bytes: vec![0x82, 0xac],
            offset_bytes: 1,
            total_bytes: 3,
        };
        let mut budget = 8;

        assert!(matches!(
            response_excerpt(response, address, &mut budget),
            Err(SessionTimelineRepositoryError::InvalidDetailQuery)
        ));
    }

    #[test]
    fn a_scalar_boundary_cursor_still_reads_its_excerpt() {
        let address =
            TimelineAddress::new(NonZeroU64::new(12).expect("fixture address is positive"));
        // The same response read from the boundary after "a" returns the whole
        // remaining scalar, proving the new classification only rejects offsets
        // that genuinely land inside one.
        let response = ModelResponseSlice {
            bytes: "€".as_bytes().to_vec(),
            offset_bytes: 1,
            total_bytes: 4,
        };
        let mut budget = 8;

        let excerpt =
            response_excerpt(response, address, &mut budget).expect("boundary cursor is readable");
        assert_eq!(excerpt.text, "€");
    }
}
