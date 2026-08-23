//! Bounded PostgreSQL discovery projections over immutable imported conversations.
//!
//! These reads deliberately avoid [`crate::conversation_import::ImportedConversationRepository`]:
//! catalog and entry-window callers never reconstruct a complete imported aggregate.

use std::{error::Error, fmt, num::NonZeroU32};

use rust_decimal::Decimal;
use signalbox_domain::{
    ImportedConversationDisplayTitle, ImportedConversationFormat, ImportedConversationId,
    ImportedSourceAttestation, ImportedSpeaker, ImportedTranscriptEntryId,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

use crate::conversation_import::{
    DISPLAY_TITLE_STATE_DERIVED, DISPLAY_TITLE_STATE_PENDING, DISPLAY_TITLE_STATE_UNDERIVABLE,
    decode_format, decode_source_speaker, encode_format, positive_u64,
};

/// Exact filters and exclusive keyset position for one imports page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationPageRequest {
    /// Exclusive imported-conversation identity cursor.
    pub after: Option<ImportedConversationId>,
    /// Exact source/converter filter.
    pub format: Option<ImportedConversationFormat>,
    /// Exact converter-attested source-session identifier bytes.
    pub source_session_id: Option<Vec<u8>>,
    /// Maximum source-session evidence bytes projected per returned row.
    pub source_session_maximum_bytes: NonZeroU32,
    /// Requested nonzero row count.
    pub limit: NonZeroU32,
}

/// One byte-bounded UTF-8 evidence projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTextProjection {
    /// Leading complete UTF-8 scalar values within the requested byte bound.
    pub leading_text: String,
    /// Whether the projection contains the complete stored value.
    pub complete: bool,
}

/// One row in a bounded imports page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationSummary {
    /// Immutable imported-conversation identity.
    pub conversation: ImportedConversationId,
    /// Evidence-derived display title.
    pub display_title: Option<ImportedConversationDisplayTitle>,
    /// Exact source/converter interpretation.
    pub format: ImportedConversationFormat,
    /// Byte-bounded consistent source-session evidence.
    pub source_session_id: Option<ImportedTextProjection>,
    /// SHA-256 of the complete source-session identifier, when present.
    pub source_session_digest: Option<[u8; 32]>,
    /// Declared normalized entry count.
    pub entry_count: u64,
}

/// One stable keyset page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationPage {
    /// Rows in ascending UUID order.
    pub items: Vec<ImportedConversationSummary>,
    /// Exclusive next-page identity.
    pub next_after: Option<ImportedConversationId>,
}

/// Byte facts projected from immutable stored members.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportedConversationSizeFacts {
    /// Exact raw source-record occurrence bytes.
    pub raw_source_bytes: u64,
    /// Normalized source-record encoding bytes.
    pub normalized_source_record_bytes: u64,
    /// Normalized entry and source-metadata encoding bytes.
    pub normalized_entry_bytes: u64,
}

/// One immutable imported frontier discovered without complete reconstruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportedContinuationReference {
    /// Owning imported conversation.
    pub conversation: ImportedConversationId,
    /// Exact imported entry.
    pub entry: ImportedTranscriptEntryId,
    /// One-based immutable position.
    pub position: u64,
}

/// Bounded descriptor over one immutable imported conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationDescriptor {
    /// Immutable imported-conversation identity.
    pub conversation: ImportedConversationId,
    /// Evidence-derived display title.
    pub display_title: Option<ImportedConversationDisplayTitle>,
    /// Exact source/converter interpretation.
    pub format: ImportedConversationFormat,
    /// SHA-256 source digest.
    pub source_digest: [u8; 32],
    /// Byte-bounded consistent source-session evidence.
    pub source_session_id: Option<ImportedTextProjection>,
    /// Declared raw-record count.
    pub raw_record_count: u64,
    /// Declared normalized entry count.
    pub entry_count: u64,
    /// Stored byte projections.
    pub sizes: ImportedConversationSizeFacts,
    /// First immutable frontier.
    pub first: ImportedContinuationReference,
    /// Latest immutable frontier.
    pub latest: ImportedContinuationReference,
}

/// Logical center for an imported-entry window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedEntryWindowAnchor {
    /// Position one.
    First,
    /// The header's immutable declared entry count.
    Latest,
    /// One exact one-based position.
    Position(u64),
}

/// One selectively decoded imported entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedEntryProjection {
    /// Exact immutable continuation reference.
    pub frontier: ImportedContinuationReference,
    /// One-based source-record occurrence.
    pub raw_record_position: u64,
    /// One-based normalized position within the source record.
    pub record_entry_position: u64,
    /// Source speaker evidence.
    pub source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    /// Byte-bounded browser discovery content facts.
    pub content: ImportedEntryContentProjection,
}

/// Browser discovery facts decoded from a byte-bounded stored-content projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedEntryContentProjection {
    /// A source event whose potentially large details are not projected.
    SourceEvent,
    /// Text attestation with a byte-bounded UTF-8 projection.
    Text(ImportedSourceAttestation<ImportedTextProjection>),
    /// A tool call whose potentially large details are not projected.
    ToolCall,
    /// A tool result whose potentially large details are not projected.
    ToolResult,
    /// Thinking whose potentially large details are not projected.
    Thinking,
    /// Redacted thinking whose potentially large details are not projected.
    RedactedThinking,
    /// A document whose potentially large details are not projected.
    Document,
    /// An explicit message-content absence.
    MessageContentAbsent,
    /// A source message block whose potentially large details are not projected.
    SourceMessageBlock,
}

/// One bounded imported-entry window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedEntryWindow {
    /// Resolved immutable anchor position.
    pub anchor_position: u64,
    /// First returned position.
    pub first_position: u64,
    /// Last returned position.
    pub last_position: u64,
    /// Whether earlier entries exist.
    pub has_before: bool,
    /// Whether later entries exist.
    pub has_after: bool,
    /// Selectively decoded entries in ascending position order.
    pub items: Vec<ImportedEntryProjection>,
}

/// A durable import projection failed checked decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedConversationDiscoveryCorruption {
    /// A required stored value is missing.
    Missing(&'static str),
    /// A closed discriminator is unsupported.
    Unsupported(&'static str),
    /// A stored positive ordinal is invalid.
    InvalidOrdinal(&'static str),
    /// UTF-8 source evidence is malformed.
    InvalidUtf8(&'static str),
    /// A fixed-size digest has the wrong width.
    InvalidDigest,
    /// A resolved display title violates its shape contract.
    InvalidDisplayTitle,
    /// A supposedly complete immutable range has a gap or mismatch.
    Inconsistent(&'static str),
    /// A normalized entry encoding is malformed.
    InvalidEntryEncoding,
}

impl fmt::Display for ImportedConversationDiscoveryCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing imported discovery {field}"),
            Self::Unsupported(field) => write!(formatter, "unsupported imported discovery {field}"),
            Self::InvalidOrdinal(field) => write!(formatter, "invalid imported discovery {field}"),
            Self::InvalidUtf8(field) => {
                write!(formatter, "invalid imported discovery {field} UTF-8")
            }
            Self::InvalidDigest => formatter.write_str("invalid imported discovery source digest"),
            Self::InvalidDisplayTitle => {
                formatter.write_str("invalid imported discovery display title")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent imported discovery {relationship}")
            }
            Self::InvalidEntryEncoding => {
                formatter.write_str("invalid imported discovery entry encoding")
            }
        }
    }
}

impl Error for ImportedConversationDiscoveryCorruption {}

/// A caller requested a region outside the selective-read contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationDiscoveryRequestError {
    /// The exact one-based anchor is outside the immutable timeline.
    PositionOutOfRange,
    /// The requested region exceeds the caller-supplied hard bound.
    WindowTooLarge,
}

impl fmt::Display for ImportedConversationDiscoveryRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PositionOutOfRange => formatter.write_str("imported position is out of range"),
            Self::WindowTooLarge => formatter.write_str("imported entry window exceeds its bound"),
        }
    }
}

impl Error for ImportedConversationDiscoveryRequestError {}

/// PostgreSQL imported-discovery failure.
#[derive(Debug)]
pub enum ImportedConversationDiscoveryError {
    /// PostgreSQL could not complete the bounded read.
    Database(sqlx::Error),
    /// The caller's requested selective region is invalid.
    Request(ImportedConversationDiscoveryRequestError),
    /// Durable data cannot satisfy the projection contract.
    Corruption(ImportedConversationDiscoveryCorruption),
}

impl fmt::Display for ImportedConversationDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "imported discovery read failed: {error}"),
            Self::Request(error) => error.fmt(formatter),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ImportedConversationDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ImportedConversationDiscoveryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ImportedConversationDiscoveryCorruption> for ImportedConversationDiscoveryError {
    fn from(error: ImportedConversationDiscoveryCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<ImportedConversationDiscoveryRequestError> for ImportedConversationDiscoveryError {
    fn from(error: ImportedConversationDiscoveryRequestError) -> Self {
        Self::Request(error)
    }
}

/// PostgreSQL adapter for bounded imported-conversation discovery.
#[derive(Clone, Debug)]
pub struct ImportedConversationDiscoveryRepository {
    pool: PgPool,
}

impl ImportedConversationDiscoveryRepository {
    /// Uses the supplied pool for short read-only projections.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads one stable UUID-keyset page plus one bounded lookahead row.
    pub async fn list(
        &self,
        request: ImportedConversationPageRequest,
    ) -> Result<ImportedConversationPage, ImportedConversationDiscoveryError> {
        let (source_format, converter_version) = request
            .format
            .map(encode_format)
            .map_or((None, None), |(format, version)| {
                (Some(format), Some(version))
            });
        let after = request.after.map(ImportedConversationId::into_uuid);
        let limit = i64::from(request.limit.get()) + 1;
        let source_session_maximum_bytes =
            i64::from(request.source_session_maximum_bytes.get()) + 3;
        let rows = if let Some(source_session_id) = request.source_session_id {
            sqlx::query(
                "SELECT imported_conversation_id, source_format, converter_version,
                    substring(source_session_id FROM 1 FOR $6::integer) AS source_session_prefix,
                    octet_length(source_session_id)::bigint AS source_session_bytes,
                    sha256(source_session_id) AS source_session_digest,
                    $6::bigint AS source_session_maximum_bytes,
                    declared_entry_count, display_title, display_title_state
               FROM imported_conversation
              WHERE ($1::uuid IS NULL OR imported_conversation_id > $1)
                AND ($2::text IS NULL OR source_format = $2)
                AND ($3::smallint IS NULL OR converter_version = $3)
                AND sha256(source_session_id) = sha256($4)
                AND source_session_id = $4
              ORDER BY imported_conversation_id
              LIMIT $5",
            )
            .bind(after)
            .bind(source_format.as_deref())
            .bind(converter_version)
            .bind(source_session_id)
            .bind(limit)
            .bind(source_session_maximum_bytes)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT imported_conversation_id, source_format, converter_version,
                    substring(source_session_id FROM 1 FOR $5::integer) AS source_session_prefix,
                    octet_length(source_session_id)::bigint AS source_session_bytes,
                    NULL::bytea AS source_session_digest,
                    $5::bigint AS source_session_maximum_bytes,
                    declared_entry_count, display_title, display_title_state
               FROM imported_conversation
              WHERE ($1::uuid IS NULL OR imported_conversation_id > $1)
                AND ($2::text IS NULL OR source_format = $2)
                AND ($3::smallint IS NULL OR converter_version = $3)
              ORDER BY imported_conversation_id
              LIMIT $4",
            )
            .bind(after)
            .bind(source_format.as_deref())
            .bind(converter_version)
            .bind(limit)
            .bind(source_session_maximum_bytes)
            .fetch_all(&self.pool)
            .await?
        };
        let mut items = rows
            .iter()
            .map(decode_summary)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > request.limit.get() as usize;
        if has_more {
            items.pop();
        }
        let next_after = if has_more {
            items.last().map(|item| item.conversation)
        } else {
            None
        };
        Ok(ImportedConversationPage { items, next_after })
    }

    /// Reads one descriptor without decoding raw records or normalized entries.
    pub async fn descriptor(
        &self,
        conversation: ImportedConversationId,
        source_session_maximum_bytes: NonZeroU32,
    ) -> Result<Option<ImportedConversationDescriptor>, ImportedConversationDiscoveryError> {
        let row = sqlx::query(
            "SELECT imported.imported_conversation_id, imported.source_format,
                    imported.converter_version, imported.source_digest,
                    substring(imported.source_session_id FROM 1 FOR $2::integer)
                        AS source_session_prefix,
                    octet_length(imported.source_session_id)::bigint AS source_session_bytes,
                    $2::bigint AS source_session_maximum_bytes,
                    imported.declared_raw_record_count,
                    imported.declared_entry_count, imported.display_title,
                    imported.display_title_state,
                    sizes.raw_source_bytes,
                    sizes.normalized_source_record_bytes,
                    sizes.normalized_entry_bytes,
                    (SELECT imported_transcript_entry_id
                       FROM imported_transcript_entry AS first_entry
                      WHERE first_entry.imported_conversation_id =
                            imported.imported_conversation_id
                      ORDER BY imported_entry_position ASC LIMIT 1) AS first_entry_id,
                    (SELECT imported_entry_position
                       FROM imported_transcript_entry AS first_entry
                      WHERE first_entry.imported_conversation_id =
                            imported.imported_conversation_id
                      ORDER BY imported_entry_position ASC LIMIT 1) AS first_entry_position,
                    (SELECT imported_transcript_entry_id
                       FROM imported_transcript_entry AS latest_entry
                      WHERE latest_entry.imported_conversation_id =
                            imported.imported_conversation_id
                      ORDER BY imported_entry_position DESC LIMIT 1) AS latest_entry_id,
                    (SELECT imported_entry_position
                       FROM imported_transcript_entry AS latest_entry
                      WHERE latest_entry.imported_conversation_id =
                            imported.imported_conversation_id
                      ORDER BY imported_entry_position DESC LIMIT 1) AS latest_entry_position
               FROM imported_conversation AS imported
               JOIN imported_conversation_size_totals AS sizes
                 ON sizes.imported_conversation_id = imported.imported_conversation_id
              WHERE imported.imported_conversation_id = $1",
        )
        .bind(conversation.into_uuid())
        .bind(i64::from(source_session_maximum_bytes.get()) + 3)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_descriptor(&row)).transpose()
    }

    /// Reads only the selected immutable position range.
    pub async fn entry_window(
        &self,
        conversation: ImportedConversationId,
        anchor: ImportedEntryWindowAnchor,
        before: u32,
        after: u32,
        maximum_items: NonZeroU32,
        maximum_text_bytes: NonZeroU32,
    ) -> Result<Option<ImportedEntryWindow>, ImportedConversationDiscoveryError> {
        let count: Option<Decimal> = sqlx::query_scalar(
            "SELECT declared_entry_count
               FROM imported_conversation
              WHERE imported_conversation_id = $1",
        )
        .bind(conversation.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(count) = count else {
            return Ok(None);
        };
        let count = positive(count, "declared entry count")?;
        let anchor_position = match anchor {
            ImportedEntryWindowAnchor::First => 1,
            ImportedEntryWindowAnchor::Latest => count,
            ImportedEntryWindowAnchor::Position(position) if position > 0 && position <= count => {
                position
            }
            ImportedEntryWindowAnchor::Position(_) => {
                return Err(ImportedConversationDiscoveryRequestError::PositionOutOfRange.into());
            }
        };
        let first_position = anchor_position.saturating_sub(u64::from(before)).max(1);
        let last_position = anchor_position.saturating_add(u64::from(after)).min(count);
        let projected_items = last_position - first_position + 1;
        if projected_items > u64::from(maximum_items.get()) {
            return Err(ImportedConversationDiscoveryRequestError::WindowTooLarge.into());
        }
        let rows = sqlx::query(
            "SELECT imported_entry_position, imported_transcript_entry_id,
                    raw_record_position, record_entry_position, source_speaker_kind,
                    substring(content_encoding FROM 1 FOR 12) AS content_header,
                    CASE WHEN get_byte(content_encoding, 2) = 1
                              AND get_byte(content_encoding, 3) = 2
                         THEN substring(content_encoding FROM 13 FOR $4::integer) END
                         AS content_text_prefix,
                    content_kind,
                    octet_length(content_encoding)::bigint AS content_bytes,
                    $4::bigint AS content_projected_bytes
               FROM imported_transcript_entry
              WHERE imported_conversation_id = $1
                AND imported_entry_position BETWEEN $2 AND $3
              ORDER BY imported_entry_position",
        )
        .bind(conversation.into_uuid())
        .bind(Decimal::from(first_position))
        .bind(Decimal::from(last_position))
        .bind(i64::from(maximum_text_bytes.get()) + 3)
        .fetch_all(&self.pool)
        .await?;
        let items = rows
            .iter()
            .map(|row| decode_entry(conversation, row))
            .collect::<Result<Vec<_>, _>>()?;
        if items.len() as u64 != projected_items
            || items.first().map(|entry| entry.frontier.position) != Some(first_position)
            || items.last().map(|entry| entry.frontier.position) != Some(last_position)
        {
            return Err(ImportedConversationDiscoveryCorruption::Inconsistent(
                "immutable entry window",
            )
            .into());
        }
        Ok(Some(ImportedEntryWindow {
            anchor_position,
            first_position,
            last_position,
            has_before: first_position > 1,
            has_after: last_position < count,
            items,
        }))
    }
}

fn decode_summary(
    row: &PgRow,
) -> Result<ImportedConversationSummary, ImportedConversationDiscoveryError> {
    let format = checked_format(row)?;
    Ok(ImportedConversationSummary {
        conversation: ImportedConversationId::from_uuid(row.try_get("imported_conversation_id")?),
        display_title: checked_display_title(row)?,
        format,
        source_session_id: checked_source_session_id(row)?,
        source_session_digest: checked_optional_digest(row, "source_session_digest")?,
        entry_count: positive(row.try_get("declared_entry_count")?, "declared entry count")?,
    })
}

fn checked_optional_digest(
    row: &PgRow,
    field: &'static str,
) -> Result<Option<[u8; 32]>, ImportedConversationDiscoveryError> {
    let digest: Option<Vec<u8>> = row.try_get(field)?;
    digest
        .map(|digest| {
            digest
                .try_into()
                .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidDigest.into())
        })
        .transpose()
}

fn decode_descriptor(
    row: &PgRow,
) -> Result<ImportedConversationDescriptor, ImportedConversationDiscoveryError> {
    let conversation = ImportedConversationId::from_uuid(row.try_get("imported_conversation_id")?);
    let entry_count = positive(row.try_get("declared_entry_count")?, "declared entry count")?;
    let first_entry: Option<Uuid> = row.try_get("first_entry_id")?;
    let latest_entry: Option<Uuid> = row.try_get("latest_entry_id")?;
    let first_position: Option<Decimal> = row.try_get("first_entry_position")?;
    let latest_position: Option<Decimal> = row.try_get("latest_entry_position")?;
    let first_position = positive(
        first_position.ok_or(ImportedConversationDiscoveryCorruption::Missing(
            "first entry position",
        ))?,
        "first entry position",
    )?;
    let latest_position = positive(
        latest_position.ok_or(ImportedConversationDiscoveryCorruption::Missing(
            "latest entry position",
        ))?,
        "latest entry position",
    )?;
    if first_position != 1 || latest_position != entry_count {
        return Err(ImportedConversationDiscoveryCorruption::Inconsistent(
            "descriptor frontier positions",
        )
        .into());
    }
    let source_digest: Vec<u8> = row.try_get("source_digest")?;
    let source_digest = source_digest
        .try_into()
        .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidDigest)?;
    Ok(ImportedConversationDescriptor {
        conversation,
        display_title: checked_display_title(row)?,
        format: checked_format(row)?,
        source_digest,
        source_session_id: checked_source_session_id(row)?,
        raw_record_count: positive(
            row.try_get("declared_raw_record_count")?,
            "declared raw-record count",
        )?,
        entry_count,
        sizes: ImportedConversationSizeFacts {
            raw_source_bytes: positive(row.try_get("raw_source_bytes")?, "raw source bytes")?,
            normalized_source_record_bytes: positive(
                row.try_get("normalized_source_record_bytes")?,
                "normalized source-record bytes",
            )?,
            normalized_entry_bytes: positive(
                row.try_get("normalized_entry_bytes")?,
                "normalized entry bytes",
            )?,
        },
        first: ImportedContinuationReference {
            conversation,
            entry: ImportedTranscriptEntryId::from_uuid(first_entry.ok_or(
                ImportedConversationDiscoveryCorruption::Missing("first entry"),
            )?),
            position: first_position,
        },
        latest: ImportedContinuationReference {
            conversation,
            entry: ImportedTranscriptEntryId::from_uuid(latest_entry.ok_or(
                ImportedConversationDiscoveryCorruption::Missing("latest entry"),
            )?),
            position: latest_position,
        },
    })
}

fn decode_entry(
    conversation: ImportedConversationId,
    row: &PgRow,
) -> Result<ImportedEntryProjection, ImportedConversationDiscoveryError> {
    let source_speaker_kind: String = row.try_get("source_speaker_kind")?;
    let source_speaker = decode_source_speaker(&source_speaker_kind)
        .map_err(|_| ImportedConversationDiscoveryCorruption::Unsupported("source speaker"))?;
    let content = checked_content_projection(row)?;
    Ok(ImportedEntryProjection {
        frontier: ImportedContinuationReference {
            conversation,
            entry: ImportedTranscriptEntryId::from_uuid(
                row.try_get("imported_transcript_entry_id")?,
            ),
            position: positive(row.try_get("imported_entry_position")?, "entry position")?,
        },
        raw_record_position: positive(row.try_get("raw_record_position")?, "raw-record position")?,
        record_entry_position: positive(
            row.try_get("record_entry_position")?,
            "record-entry position",
        )?,
        source_speaker,
        content,
    })
}

fn checked_content_projection(
    row: &PgRow,
) -> Result<ImportedEntryContentProjection, ImportedConversationDiscoveryError> {
    let header: Vec<u8> = row.try_get("content_header")?;
    let total_bytes: i64 = row.try_get("content_bytes")?;
    let projected_bytes: i64 = row.try_get("content_projected_bytes")?;
    if header.len() < 3 || !matches!(header[0], 1 | 2) || header[1] != 1 || total_bytes < 3 {
        return Err(ImportedConversationDiscoveryCorruption::InvalidEntryEncoding.into());
    }
    let content_kind: i16 = row.try_get("content_kind")?;
    if i16::from(header[2]) != content_kind {
        return Err(ImportedConversationDiscoveryCorruption::InvalidEntryEncoding.into());
    }
    match content_kind {
        0 => Ok(ImportedEntryContentProjection::SourceEvent),
        1 => checked_text_projection(&header, row, total_bytes, projected_bytes)
            .map(ImportedEntryContentProjection::Text),
        2 => Ok(ImportedEntryContentProjection::ToolCall),
        3 => Ok(ImportedEntryContentProjection::ToolResult),
        4 => Ok(ImportedEntryContentProjection::Thinking),
        5 => Ok(ImportedEntryContentProjection::RedactedThinking),
        6 => Ok(ImportedEntryContentProjection::Document),
        7 if total_bytes == 4 && matches!(header.get(3), Some(0..=4)) => {
            Ok(ImportedEntryContentProjection::MessageContentAbsent)
        }
        8 => Ok(ImportedEntryContentProjection::SourceMessageBlock),
        _ => Err(ImportedConversationDiscoveryCorruption::InvalidEntryEncoding.into()),
    }
}

fn checked_text_projection(
    header: &[u8],
    row: &PgRow,
    total_bytes: i64,
    projected_bytes: i64,
) -> Result<ImportedSourceAttestation<ImportedTextProjection>, ImportedConversationDiscoveryError> {
    match header.get(3) {
        Some(0) if total_bytes == 4 => Ok(ImportedSourceAttestation::NotAttested),
        Some(1) if total_bytes == 4 => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(2) if header.len() == 12 => {
            let declared_bytes = u64::from_be_bytes(
                header[4..12]
                    .try_into()
                    .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidEntryEncoding)?,
            );
            let declared_bytes = i64::try_from(declared_bytes)
                .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidEntryEncoding)?;
            if total_bytes.checked_sub(12) != Some(declared_bytes) {
                return Err(ImportedConversationDiscoveryCorruption::InvalidEntryEncoding.into());
            }
            let prefix: Vec<u8> = row.try_get("content_text_prefix")?;
            bounded_utf8_projection(prefix, declared_bytes, projected_bytes, "entry text")
                .map(ImportedSourceAttestation::Attested)
        }
        _ => Err(ImportedConversationDiscoveryCorruption::InvalidEntryEncoding.into()),
    }
}

fn checked_format(
    row: &PgRow,
) -> Result<ImportedConversationFormat, ImportedConversationDiscoveryError> {
    let source_format: String = row.try_get("source_format")?;
    let converter_version: i16 = row.try_get("converter_version")?;
    decode_format(&source_format, converter_version)
        .map_err(|_| ImportedConversationDiscoveryCorruption::Unsupported("format").into())
}

fn checked_display_title(
    row: &PgRow,
) -> Result<Option<ImportedConversationDisplayTitle>, ImportedConversationDiscoveryError> {
    let state: String = row.try_get("display_title_state")?;
    let title: Option<String> = row.try_get("display_title")?;
    match (state.as_str(), title) {
        (DISPLAY_TITLE_STATE_DERIVED, Some(title)) => {
            ImportedConversationDisplayTitle::try_new(title)
                .map(Some)
                .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidDisplayTitle.into())
        }
        (DISPLAY_TITLE_STATE_UNDERIVABLE, None) => Ok(None),
        (DISPLAY_TITLE_STATE_PENDING, _) => Err(
            ImportedConversationDiscoveryCorruption::Inconsistent("pending display title").into(),
        ),
        _ => {
            Err(ImportedConversationDiscoveryCorruption::Inconsistent("display-title state").into())
        }
    }
}

fn checked_source_session_id(
    row: &PgRow,
) -> Result<Option<ImportedTextProjection>, ImportedConversationDiscoveryError> {
    let prefix: Option<Vec<u8>> = row.try_get("source_session_prefix")?;
    let total_bytes: Option<i64> = row.try_get("source_session_bytes")?;
    let projected_bytes: i64 = row.try_get("source_session_maximum_bytes")?;
    match (prefix, total_bytes) {
        (None, None) => Ok(None),
        (Some(prefix), Some(total_bytes)) => {
            bounded_utf8_projection(prefix, total_bytes, projected_bytes, "source session")
                .map(Some)
        }
        _ => Err(ImportedConversationDiscoveryCorruption::Inconsistent(
            "source-session projection",
        )
        .into()),
    }
}

fn bounded_utf8_projection(
    prefix: Vec<u8>,
    total_bytes: i64,
    projected_bytes: i64,
    field: &'static str,
) -> Result<ImportedTextProjection, ImportedConversationDiscoveryError> {
    let total_bytes = usize::try_from(total_bytes)
        .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidOrdinal(field))?;
    let projected_bytes = usize::try_from(projected_bytes)
        .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidOrdinal(field))?;
    let maximum_bytes = projected_bytes.checked_sub(3).ok_or(
        ImportedConversationDiscoveryCorruption::InvalidOrdinal(field),
    )?;
    if prefix.len() < total_bytes.min(projected_bytes) {
        return Err(ImportedConversationDiscoveryCorruption::Inconsistent(
            "bounded UTF-8 projection",
        )
        .into());
    }
    let candidate = &prefix[..prefix.len().min(maximum_bytes)];
    let end = match std::str::from_utf8(candidate) {
        Ok(_) => candidate.len(),
        Err(error) if error.error_len().is_none() && total_bytes > maximum_bytes => {
            error.valid_up_to()
        }
        Err(_) => return Err(ImportedConversationDiscoveryCorruption::InvalidUtf8(field).into()),
    };
    let leading_text = std::str::from_utf8(&candidate[..end])
        .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidUtf8(field))?
        .to_owned();
    Ok(ImportedTextProjection {
        leading_text,
        complete: total_bytes <= maximum_bytes,
    })
}

fn positive(
    value: Decimal,
    field: &'static str,
) -> Result<u64, ImportedConversationDiscoveryError> {
    positive_u64(value)
        .map_err(|_| ImportedConversationDiscoveryCorruption::InvalidOrdinal(field).into())
}

#[cfg(test)]
mod tests {
    use super::{
        ImportedConversationDiscoveryCorruption, ImportedConversationDiscoveryError,
        bounded_utf8_projection,
    };

    #[test]
    fn complete_projection_rejects_incomplete_utf8() {
        let error = bounded_utf8_projection(vec![b'a', 0xe2], 2, 5, "fixture")
            .expect_err("a complete stored value must be valid UTF-8");

        assert!(matches!(
            error,
            ImportedConversationDiscoveryError::Corruption(
                ImportedConversationDiscoveryCorruption::InvalidUtf8("fixture")
            )
        ));
    }

    #[test]
    fn truncated_projection_drops_only_an_incomplete_boundary_scalar() {
        let projection =
            bounded_utf8_projection(vec![b'a', 0xe2, 0x82, 0xac, b'b'], 5, 5, "fixture")
                .expect("a shortened prefix may end inside a valid scalar");

        assert_eq!(projection.leading_text, "a");
        assert!(!projection.complete);
    }
}
