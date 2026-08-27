//! Unified conversation listing over the authoritative session and
//! imported-conversation tables.
//!
//! The page is one plain keyset read inside a repeatable-read, read-only
//! transaction — no materialized view, cache, or analytical artifact — so
//! every listed row is transactionally fresh (docs/spec/process-protocol.md).

use std::{error::Error, fmt};

use signalbox_application::{
    ConversationListCursor, ConversationListItem, ConversationListQuery, ConversationLister,
    ConversationPageReader,
};
use signalbox_domain::{ImportedConversationDisplayTitle, ImportedConversationId, SessionId};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    conversation_import::{
        DISPLAY_TITLE_STATE_DERIVED, DISPLAY_TITLE_STATE_UNDERIVABLE, decode_format, positive_u64,
    },
    mapping::{PositiveOrdinalMappingError, defaults_version_from_numeric},
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// The unified order is by identity UUID value, native before imported for a
/// theoretical equal identity. These ranks encode that tiebreak in SQL and in
/// cursor binds.
const NATIVE_ORIGIN_RANK: i32 = 0;
const IMPORTED_ORIGIN_RANK: i32 = 1;

/// One matching unified row in strict cursor order. The lookahead probe below
/// repeats this filter and keyset predicate exactly. `$1`/`$2` are the
/// exclusive cursor identity and origin rank, `$3` selects native rows, `$4`
/// includes archived native rows, `$5` is the exact case-sensitive title
/// substring, and `$6` selects imported rows. A `NULL` native title or
/// resolved-absent imported display title matches no title filter.
const UNIFIED_PAGE_ITEM_SQL: &str = "SELECT unified.origin_rank, unified.conversation_id,
           unified.title, unified.archived, unified.defaults_version,
           unified.source_format, unified.converter_version,
           unified.entry_count, unified.display_title_state
      FROM (
        SELECT 0 AS origin_rank,
               session_row.session_id AS conversation_id,
               metadata.title AS title,
               COALESCE(metadata.archived, false) AS archived,
               current_defaults.current_version AS defaults_version,
               NULL::text AS source_format,
               NULL::smallint AS converter_version,
               NULL::numeric(20, 0) AS entry_count,
               NULL::text AS display_title_state
          FROM session AS session_row
          LEFT JOIN session_current_defaults AS current_defaults
            ON current_defaults.session_id = session_row.session_id
          LEFT JOIN session_metadata AS metadata
            ON metadata.session_id = session_row.session_id
         WHERE $3
           AND ($4 OR NOT COALESCE(metadata.archived, false))
           AND ($5::text IS NULL OR strpos(metadata.title, $5) > 0)
        UNION ALL
        SELECT 1 AS origin_rank,
               imported.imported_conversation_id AS conversation_id,
               imported.display_title AS title,
               false AS archived,
               NULL::numeric(20, 0) AS defaults_version,
               imported.source_format,
               imported.converter_version,
               imported.declared_entry_count AS entry_count,
               imported.display_title_state
          FROM imported_conversation AS imported
         WHERE $6
           AND ($5::text IS NULL OR strpos(imported.display_title, $5) > 0)
    ) AS unified
    WHERE (
        $1::uuid IS NULL
        OR (unified.conversation_id, unified.origin_rank) > ($1, $2)
    )
    ORDER BY unified.conversation_id, unified.origin_rank
    LIMIT 1";

/// The lookahead probe over the exact item filter and keyset predicate.
const UNIFIED_PAGE_PROBE_SQL: &str = "SELECT EXISTS (
        SELECT 1
      FROM (
        SELECT 0 AS origin_rank,
               session_row.session_id AS conversation_id
          FROM session AS session_row
          LEFT JOIN session_metadata AS metadata
            ON metadata.session_id = session_row.session_id
         WHERE $3
           AND ($4 OR NOT COALESCE(metadata.archived, false))
           AND ($5::text IS NULL OR strpos(metadata.title, $5) > 0)
        UNION ALL
        SELECT 1 AS origin_rank,
               imported.imported_conversation_id AS conversation_id
          FROM imported_conversation AS imported
         WHERE $6
           AND ($5::text IS NULL OR strpos(imported.display_title, $5) > 0)
    ) AS unified
    WHERE (
        $1::uuid IS NULL
        OR (unified.conversation_id, unified.origin_rank) > ($1, $2)
    )
    )";

/// A durable unified-listing row failed checked decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationListingCorruption {
    /// One required durable value is absent.
    Missing(&'static str),
    /// A closed discriminator is unsupported.
    Unsupported {
        /// Durable field being decoded.
        field: &'static str,
        /// Unsupported non-content spelling.
        value: String,
    },
    /// One stored positive ordinal cannot construct its domain value.
    InvalidOrdinal {
        /// Durable field being decoded.
        field: &'static str,
        /// Why the numeric value is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// A stored display title violates the derived-shape contract.
    InvalidDisplayTitle,
    /// Two durable facts that must agree do not.
    Inconsistent(&'static str),
}

impl fmt::Display for ConversationListingCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => {
                write!(formatter, "missing unified conversation-listing {field}")
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported unified conversation-listing {field}: {value}"
                )
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(
                    formatter,
                    "invalid unified conversation-listing {field}: {reason}"
                )
            }
            Self::InvalidDisplayTitle => {
                formatter.write_str("stored imported display title violates its shape contract")
            }
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "inconsistent unified conversation-listing state: {relationship}"
                )
            }
        }
    }
}

impl Error for ConversationListingCorruption {}

/// PostgreSQL unified conversation-listing failure.
#[derive(Debug)]
pub enum ConversationListingRepositoryError {
    /// PostgreSQL could not complete the read.
    Database(sqlx::Error),
    /// Durable data cannot satisfy the unified-listing contract.
    Corruption(ConversationListingCorruption),
}

impl fmt::Display for ConversationListingRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "unified conversation listing failed: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ConversationListingRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ConversationListingRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ConversationListingCorruption> for ConversationListingRepositoryError {
    fn from(error: ConversationListingCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL unified conversation-listing repository.
#[derive(Clone, Debug)]
pub struct ConversationListingRepository {
    pool: PgPool,
}

impl ConversationListingRepository {
    /// Uses the supplied pool for bounded repeatable-read pages.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Opens one bounded page inside a repeatable-read, read-only
    /// transaction.
    pub async fn open_page(
        &self,
        query: ConversationListQuery,
    ) -> Result<PostgresConversationPage, ConversationListingRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let keyset_position = query.after();
        Ok(PostgresConversationPage {
            transaction: Some(transaction),
            query,
            keyset_position,
            yielded: 0,
            last_emitted: None,
            continuation: None,
            complete: false,
        })
    }
}

impl ConversationLister for ConversationListingRepository {
    type Error = ConversationListingRepositoryError;
    type Page = PostgresConversationPage;

    async fn open_conversation_page(
        &self,
        query: ConversationListQuery,
    ) -> Result<Self::Page, Self::Error> {
        ConversationListingRepository::open_page(self, query).await
    }
}

/// One bounded unified page backed by an open repeatable-read transaction.
#[derive(Debug)]
pub struct PostgresConversationPage {
    transaction: Option<Transaction<'static, Postgres>>,
    query: ConversationListQuery,
    keyset_position: Option<ConversationListCursor>,
    yielded: u64,
    last_emitted: Option<ConversationListCursor>,
    continuation: Option<ConversationListCursor>,
    complete: bool,
}

impl PostgresConversationPage {
    /// Yields one matching row in strict unified cursor order.
    pub async fn next_item(
        &mut self,
    ) -> Result<Option<ConversationListItem>, ConversationListingRepositoryError> {
        if self.complete {
            return Ok(None);
        }
        if self.yielded == self.query.page_size() {
            let has_more = self.has_later_match().await?;
            self.continuation = has_more.then_some(self.last_emitted.ok_or(
                ConversationListingCorruption::Inconsistent(
                    "nonempty full page lacks last emitted conversation",
                ),
            )?);
            self.finish().await?;
            return Ok(None);
        }

        let binds = CursorBinds::from_cursor(self.keyset_position);
        let selects_native = self.query.origin().selects_native();
        let include_archived = self.query.include_archived();
        let title_contains = self.query.title_contains().map(str::to_owned);
        let selects_imported = self.query.origin().selects_imported();
        let transaction = self.transaction_mut()?;
        let row = sqlx::query(UNIFIED_PAGE_ITEM_SQL)
            .bind(binds.identity)
            .bind(binds.origin_rank)
            .bind(selects_native)
            .bind(include_archived)
            .bind(title_contains)
            .bind(selects_imported)
            .fetch_optional(&mut **transaction)
            .await?;

        let Some(row) = row else {
            self.finish().await?;
            return Ok(None);
        };
        let item = decode_list_item(&row)?;
        self.keyset_position = Some(item.cursor());
        self.last_emitted = Some(item.cursor());
        self.yielded =
            self.yielded
                .checked_add(1)
                .ok_or(ConversationListingCorruption::Inconsistent(
                    "page item count overflowed",
                ))?;
        Ok(Some(item))
    }

    /// Returns whether another match exists past the full page's last row.
    async fn has_later_match(&mut self) -> Result<bool, ConversationListingRepositoryError> {
        let binds = CursorBinds::from_cursor(self.keyset_position);
        let selects_native = self.query.origin().selects_native();
        let include_archived = self.query.include_archived();
        let title_contains = self.query.title_contains().map(str::to_owned);
        let selects_imported = self.query.origin().selects_imported();
        let transaction = self.transaction_mut()?;
        let exists: bool = sqlx::query_scalar(UNIFIED_PAGE_PROBE_SQL)
            .bind(binds.identity)
            .bind(binds.origin_rank)
            .bind(selects_native)
            .bind(include_archived)
            .bind(title_contains)
            .bind(selects_imported)
            .fetch_one(&mut **transaction)
            .await?;
        Ok(exists)
    }

    async fn finish(&mut self) -> Result<(), ConversationListingRepositoryError> {
        let transaction = self
            .transaction
            .take()
            .ok_or(ConversationListingCorruption::Missing(
                "conversation page transaction",
            ))?;
        transaction.commit().await?;
        self.complete = true;
        Ok(())
    }

    fn transaction_mut(
        &mut self,
    ) -> Result<&mut Transaction<'static, Postgres>, ConversationListingRepositoryError> {
        self.transaction
            .as_mut()
            .ok_or(ConversationListingCorruption::Missing("conversation page transaction").into())
    }
}

impl ConversationPageReader for PostgresConversationPage {
    type Error = ConversationListingRepositoryError;

    async fn next_item(&mut self) -> Result<Option<ConversationListItem>, Self::Error> {
        PostgresConversationPage::next_item(self).await
    }

    fn next_after(&self) -> Option<ConversationListCursor> {
        self.continuation
    }
}

/// The exclusive keyset cursor as its two SQL binds.
struct CursorBinds {
    identity: Option<Uuid>,
    origin_rank: i32,
}

impl CursorBinds {
    fn from_cursor(cursor: Option<ConversationListCursor>) -> Self {
        match cursor {
            None => Self {
                identity: None,
                origin_rank: NATIVE_ORIGIN_RANK,
            },
            Some(ConversationListCursor::NativeSession(session)) => Self {
                identity: Some(session.into_uuid()),
                origin_rank: NATIVE_ORIGIN_RANK,
            },
            Some(ConversationListCursor::ImportedConversation(conversation)) => Self {
                identity: Some(conversation.into_uuid()),
                origin_rank: IMPORTED_ORIGIN_RANK,
            },
        }
    }
}

fn decode_list_item(
    row: &PgRow,
) -> Result<ConversationListItem, ConversationListingRepositoryError> {
    let origin_rank: i32 = required(row, "origin_rank")?;
    let conversation_id: Uuid = required(row, "conversation_id")?;
    let title: Option<String> = required(row, "title")?;
    match origin_rank {
        NATIVE_ORIGIN_RANK => {
            let archived: bool = required(row, "archived")?;
            let defaults_version: Option<rust_decimal::Decimal> =
                required(row, "defaults_version")?;
            let defaults_version = defaults_version
                .ok_or(ConversationListingCorruption::Missing("defaults version"))?;
            let defaults_version =
                defaults_version_from_numeric(defaults_version).map_err(|reason| {
                    ConversationListingCorruption::InvalidOrdinal {
                        field: "defaults version",
                        reason,
                    }
                })?;
            Ok(ConversationListItem::NativeSession {
                session: SessionId::from_uuid(conversation_id),
                title,
                archived,
                defaults_version,
            })
        }
        IMPORTED_ORIGIN_RANK => {
            let source_format: String = required(row, "source_format")?;
            let converter_version: i16 = required(row, "converter_version")?;
            let format = decode_format(&source_format, converter_version).map_err(|_| {
                ConversationListingCorruption::Unsupported {
                    field: "source format",
                    value: source_format.clone(),
                }
            })?;
            let entry_count = positive_u64(required(row, "entry_count")?).map_err(|reason| {
                ConversationListingCorruption::InvalidOrdinal {
                    field: "entry count",
                    reason,
                }
            })?;
            let display_title_state: String = required(row, "display_title_state")?;
            let title = decode_resolved_display_title(title, &display_title_state)?;
            Ok(ConversationListItem::ImportedConversation {
                conversation: ImportedConversationId::from_uuid(conversation_id),
                title,
                entry_count,
                format,
            })
        }
        other => Err(ConversationListingCorruption::Unsupported {
            field: "origin rank",
            value: other.to_string(),
        }
        .into()),
    }
}

/// Requires a final display-title state and validates the stored shape.
fn decode_resolved_display_title(
    title: Option<String>,
    display_title_state: &str,
) -> Result<Option<String>, ConversationListingRepositoryError> {
    match display_title_state {
        DISPLAY_TITLE_STATE_DERIVED => {
            let title = title.ok_or(ConversationListingCorruption::Missing("display title"))?;
            let title = ImportedConversationDisplayTitle::try_new(title)
                .map_err(|_| ConversationListingCorruption::InvalidDisplayTitle)?;
            Ok(Some(title.into_string()))
        }
        DISPLAY_TITLE_STATE_UNDERIVABLE => {
            if title.is_some() {
                return Err(ConversationListingCorruption::Inconsistent(
                    "underivable display-title state carries a title",
                )
                .into());
            }
            Ok(None)
        }
        other => Err(ConversationListingCorruption::Unsupported {
            field: "display-title state",
            value: String::from(other),
        }
        .into()),
    }
}

fn required<'row, T>(
    row: &'row PgRow,
    field: &'static str,
) -> Result<T, ConversationListingRepositoryError>
where
    T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>,
{
    row.try_get(field)
        .map_err(ConversationListingRepositoryError::Database)
}
