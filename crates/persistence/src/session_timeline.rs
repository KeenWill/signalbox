//! PostgreSQL adapter for bounded historical session-timeline reads.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SessionTimelineBounds, SessionTimelineDescriptor, SessionTimelineEventKind,
    SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts, SessionTimelineWindow,
    SessionWorkFacts, TimelineAddress, TimelineContinuation, TimelineWindowAnchor,
    TimelineWindowLimits,
};
use signalbox_domain::SessionId;
use sqlx::{PgConnection, PgPool, Row};

use crate::mapping::{
    OUTBOX_EVENT_KIND_UTF8_BYTE_BOUNDS, OutboxEventDiscriminator,
    outbox_event_discriminator_from_str,
};

const PROJECTED_ITEM_ENVELOPE_BYTES: u32 = 64;

/// Integrity failure in the durable timeline projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTimelineCorruption {
    Missing(&'static str),
    InvalidOrdinal(&'static str),
    UnsupportedEventKind(String),
    ItemProjectionOverflow,
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
        }
    }
}

impl Error for SessionTimelineCorruption {}

/// Database or fail-closed projection failure.
#[derive(Debug)]
pub enum SessionTimelineRepositoryError {
    Database(sqlx::Error),
    Corruption(SessionTimelineCorruption),
}

impl fmt::Display for SessionTimelineRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "session timeline database failure: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionTimelineRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
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
}
