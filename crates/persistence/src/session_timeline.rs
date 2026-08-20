//! PostgreSQL adapter for bounded historical session-timeline reads.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SessionTimelineBounds, SessionTimelineDescriptor, SessionTimelineEventKind,
    SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts, SessionTimelineWindow,
    SessionWorkFacts, TimelineAddress, TimelineWindowAnchor, TimelineWindowLimits,
};
use signalbox_domain::SessionId;
use sqlx::{PgConnection, PgPool, Row};

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
        let has_more_before = first
            .zip(descriptor.bounds.first)
            .is_some_and(|(loaded, bound)| loaded > bound);
        let has_more_after = latest
            .zip(descriptor.bounds.latest)
            .is_some_and(|(loaded, bound)| loaded < bound);
        transaction.commit().await?;

        Ok(Some(SessionTimelineWindow {
            session,
            items,
            projected_structured_bytes: projected_bytes,
            has_more_before,
            has_more_after,
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
WITH session_events AS (
    SELECT event_sequence, event_kind FROM outbox_event WHERE session_id = $1
    UNION ALL
    SELECT event_sequence, event_kind FROM delegation_outbox_event WHERE session_id = $1
), event_facts AS (
    SELECT count(*)::numeric AS item_count,
           min(event_sequence) AS first_sequence,
           max(event_sequence) AS latest_sequence,
           coalesce(sum(64 + octet_length(event_kind)), 0)::numeric AS structured_bytes
      FROM session_events
), text_facts AS (
    SELECT (
        coalesce((SELECT sum(octet_length(convert_to(content_text, 'UTF8')))::numeric
                    FROM accepted_input WHERE session_id = $1), 0)
        + coalesce((SELECT sum(octet_length(convert_to(assistant_text_value, 'UTF8')))::numeric
                      FROM semantic_transcript_entry
                     WHERE source_session_id = $1 AND assistant_text_value IS NOT NULL), 0)
        + coalesce((SELECT sum(octet_length(convert_to(context_summary_value, 'UTF8')))::numeric
                      FROM semantic_transcript_entry
                     WHERE source_session_id = $1 AND context_summary_value IS NOT NULL), 0)
    ) AS text_bytes
), work_facts AS (
    SELECT count(*) FILTER (WHERE state_kind = 'active')::numeric AS active_count,
           count(*) FILTER (WHERE state_kind = 'queued')::numeric AS queued_count
      FROM turn_lifecycle WHERE session_id = $1
)
SELECT session.session_id, event_facts.item_count, event_facts.first_sequence,
       event_facts.latest_sequence, event_facts.structured_bytes, text_facts.text_bytes,
       work_facts.active_count, work_facts.queued_count, allocator.last_sequence
  FROM session
 CROSS JOIN event_facts
 CROSS JOIN text_facts
 CROSS JOIN work_facts
 CROSS JOIN outbox_sequence_state AS allocator
 WHERE session.session_id = $1 AND allocator.singleton
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
        .fetch_optional(connection)
        .await?;
    let Some(row) = row else { return Ok(None) };
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
        observed_through: nonnegative(row.try_get("last_sequence")?, "observation cursor")?,
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
    Ok(match value {
        "session_created" => SessionTimelineEventKind::SessionCreated,
        "session_model_settings_changed" => SessionTimelineEventKind::SessionModelSettingsChanged,
        "turn_model_settings_resolved" => SessionTimelineEventKind::TurnModelSettingsResolved,
        "input_accepted" => SessionTimelineEventKind::InputAccepted,
        "goal_turn_retired" => SessionTimelineEventKind::GoalTurnRetired,
        "turn_activated" => SessionTimelineEventKind::TurnActivated,
        "turn_failed" => SessionTimelineEventKind::TurnFailed,
        "model_call_transition" => SessionTimelineEventKind::ModelCallTransition,
        "tool_batch_transition" => SessionTimelineEventKind::ToolBatchTransition,
        "tool_approval_decided" => SessionTimelineEventKind::ToolApprovalDecided,
        "context_compacted" => SessionTimelineEventKind::ContextCompacted,
        "turn_completed" => SessionTimelineEventKind::TurnCompleted,
        "turn_refused" => SessionTimelineEventKind::TurnRefused,
        "turn_cancelled" => SessionTimelineEventKind::TurnCancelled,
        "turn_reconciliation_required" => SessionTimelineEventKind::TurnReconciliationRequired,
        "runner_state_transition" => SessionTimelineEventKind::RunnerStateTransition,
        "delegation_update" => SessionTimelineEventKind::DelegationUpdate,
        "delegation_wake" => SessionTimelineEventKind::DelegationWake,
        other => {
            return Err(SessionTimelineCorruption::UnsupportedEventKind(
                other.to_owned(),
            ));
        }
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
