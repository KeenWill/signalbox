//! Append-only PostgreSQL storage for imported conversation snapshots.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ImportedConversationStore, ImportedConversationStoreOutcome};
use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationReconstitutionFailure, ImportedConversationReconstitutionInput,
    ImportedConversationSourceDigest, ImportedRawRecordHash, ImportedRawRecordPosition,
    ImportedRawSourceRecordReconstitutionInput, ImportedRecordEntryPosition,
    ImportedSourceAttestation, ImportedSpeaker, ImportedTranscriptEntryId,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    conversation_import_codec::{
        ImportedConversationEncodingFailure as CodecFailure, decode_content,
        decode_source_metadata, decode_structured, encode_content, encode_source_metadata,
        encode_structured,
    },
    mapping::PositiveOrdinalMappingError,
};

const STORAGE_VERSION: i16 = 1;
const CLAUDE_CODE_FORMAT: &str = "claude_code_session_jsonl";
const CLAUDE_CODE_VERSION: i16 = 1;

/// Why a versioned imported domain-algebra encoding is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationEncodingCorruption {
    /// A collection or byte-string length cannot be represented safely.
    LengthOutOfRange,
    /// The encoding ended before a declared value was complete.
    UnexpectedEnd,
    /// Bytes remained after one complete value.
    TrailingBytes,
    /// The adapter encoding version is not supported.
    UnsupportedVersion(u8),
    /// A closed algebra discriminator is not supported.
    UnsupportedTag {
        /// Algebra value whose tag was decoded.
        kind: &'static str,
        /// Unsupported tag byte.
        value: u8,
    },
    /// A stored textual value is not valid UTF-8.
    InvalidUtf8(&'static str),
    /// A stored number spelling violates the JSON number grammar.
    InvalidJsonNumber,
    /// A stored structured value exceeds the admitted container depth.
    ContainerDepthExceeded,
}

/// A globally unique imported identity collided with another durable record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationIdentityCollision {
    /// The candidate conversation identity already names another snapshot.
    Conversation,
    /// A candidate imported-entry identity already names another entry.
    TranscriptEntry,
}

/// A durable imported-conversation shape failed checked reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedConversationCorruption {
    /// One required durable value is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// Durable field being decoded.
        field: &'static str,
        /// Unsupported non-content spelling.
        value: String,
    },
    /// A fixed-size digest or content hash has another byte length.
    InvalidDigestSize(&'static str),
    /// One stored positive ordinal cannot construct its domain type.
    InvalidOrdinal {
        /// Durable field being decoded.
        field: &'static str,
        /// Why the numeric value is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// One versioned domain-algebra encoding is invalid.
    Encoding {
        /// Durable field being decoded.
        field: &'static str,
        /// Content-silent codec failure.
        failure: ImportedConversationEncodingCorruption,
    },
    /// A content hash resolved to different exact raw bytes.
    RawRecordHashCollision,
    /// One source digest resolved to a structurally different snapshot.
    ExistingSnapshotMismatch,
    /// Complete durable fields failed domain-owned correlation.
    Domain(ImportedConversationReconstitutionFailure),
}

impl fmt::Display for ImportedConversationCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing imported conversation {field}"),
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported imported conversation {field}: {value}"
                )
            }
            Self::InvalidDigestSize(field) => {
                write!(formatter, "invalid imported conversation {field} size")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid imported conversation {field}: {reason}")
            }
            Self::Encoding { field, failure } => {
                write!(
                    formatter,
                    "invalid imported conversation {field} encoding: {failure:?}"
                )
            }
            Self::RawRecordHashCollision => {
                formatter.write_str("imported raw-record hash resolved to different bytes")
            }
            Self::ExistingSnapshotMismatch => {
                formatter.write_str("imported source digest resolved to a different snapshot")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "imported conversation domain reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for ImportedConversationCorruption {}

/// PostgreSQL imported-conversation repository failure.
#[derive(Debug)]
pub enum ImportedConversationRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// A candidate identity collided with a different durable record.
    IdentityCollision(ImportedConversationIdentityCollision),
    /// Candidate or durable data cannot satisfy the imported-record contract.
    Corruption(ImportedConversationCorruption),
}

impl fmt::Display for ImportedConversationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "conversation import database failure: {error}")
            }
            Self::IdentityCollision(collision) => {
                write!(
                    formatter,
                    "conversation import identity collision: {collision:?}"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ImportedConversationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::IdentityCollision(_) => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ImportedConversationRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ImportedConversationCorruption> for ImportedConversationRepositoryError {
    fn from(error: ImportedConversationCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of pure, idempotent conversation ingestion.
#[derive(Clone, Debug)]
pub struct ImportedConversationRepository {
    pool: PgPool,
}

impl ImportedConversationRepository {
    /// Uses the supplied pool for atomic insertion and checked complete loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts one complete snapshot or resolves its exact durable duplicate.
    pub async fn resolve_or_insert(
        &self,
        conversation: ImportedConversation,
    ) -> Result<ImportedConversationStoreOutcome, ImportedConversationRepositoryError> {
        let encoded = EncodedConversation::from_domain(&conversation)?;
        let candidate_id = conversation.id();
        let source_digest = conversation.source_digest();
        let declared_raw_record_count =
            usize_to_u64(encoded.raws.len(), "declared raw-record count")?;
        let declared_entry_count = usize_to_u64(encoded.entries.len(), "declared entry count")?;
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO imported_conversation
                (imported_conversation_id, storage_version, source_format,
                 converter_version, source_digest, declared_raw_record_count,
                 declared_entry_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT DO NOTHING",
        )
        .bind(candidate_id.into_uuid())
        .bind(STORAGE_VERSION)
        .bind(encoded.format)
        .bind(encoded.converter_version)
        .bind(source_digest.as_bytes().as_slice())
        .bind(Decimal::from(declared_raw_record_count))
        .bind(Decimal::from(declared_entry_count))
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !inserted {
            let existing_id = load_identity_by_source_digest(
                &mut transaction,
                encoded.format,
                encoded.converter_version,
                source_digest,
            )
            .await?;
            let Some(existing_id) = existing_id else {
                transaction.rollback().await?;
                return Err(ImportedConversationRepositoryError::IdentityCollision(
                    ImportedConversationIdentityCollision::Conversation,
                ));
            };
            let existing = load_from_connection(&mut transaction, existing_id)
                .await?
                .ok_or(ImportedConversationCorruption::ExistingSnapshotMismatch)?;
            if !equivalent_snapshot(&conversation, &existing) {
                transaction.rollback().await?;
                return Err(ImportedConversationCorruption::ExistingSnapshotMismatch.into());
            }
            transaction.rollback().await?;
            return Ok(ImportedConversationStoreOutcome::AlreadyImported {
                conversation: existing.id(),
                source_digest: existing.source_digest(),
            });
        }

        if any_entry_identity_exists(&mut transaction, &encoded.entries).await? {
            transaction.rollback().await?;
            return Err(ImportedConversationRepositoryError::IdentityCollision(
                ImportedConversationIdentityCollision::TranscriptEntry,
            ));
        }
        insert_raws(&mut transaction, candidate_id, &encoded.raws).await?;
        insert_entries(&mut transaction, candidate_id, &encoded.entries).await?;
        transaction.commit().await?;
        Ok(ImportedConversationStoreOutcome::Inserted {
            conversation: candidate_id,
            source_digest,
        })
    }

    /// Loads one complete snapshot, returning `None` only for an absent header.
    pub async fn load(
        &self,
        conversation: ImportedConversationId,
    ) -> Result<Option<ImportedConversation>, ImportedConversationRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_from_connection(&mut connection, conversation).await
    }
}

impl ImportedConversationStore for ImportedConversationRepository {
    type Error = ImportedConversationRepositoryError;

    async fn resolve_or_insert(
        &mut self,
        conversation: ImportedConversation,
    ) -> Result<ImportedConversationStoreOutcome, Self::Error> {
        ImportedConversationRepository::resolve_or_insert(self, conversation).await
    }
}

struct EncodedConversation {
    format: &'static str,
    converter_version: i16,
    raws: Vec<EncodedRawRecord>,
    entries: Vec<EncodedEntry>,
}

impl EncodedConversation {
    fn from_domain(
        conversation: &ImportedConversation,
    ) -> Result<Self, ImportedConversationRepositoryError> {
        let (format, converter_version) = encode_format(conversation.format());
        let mut entry_counts = vec![0_u64; conversation.raw_records().len()];
        for entry in conversation.entries() {
            let raw_index = usize::try_from(entry.raw_record_position().as_u64())
                .ok()
                .and_then(|position| position.checked_sub(1))
                .ok_or_else(|| invalid_ordinal("entry raw-record position"))?;
            let count = entry_counts
                .get_mut(raw_index)
                .ok_or_else(|| invalid_ordinal("entry raw-record position"))?;
            *count = count
                .checked_add(1)
                .ok_or_else(|| invalid_ordinal("raw-record entry count"))?;
        }
        let raws = conversation
            .raw_records()
            .iter()
            .zip(entry_counts)
            .map(|(raw, declared_entry_count)| {
                Ok(EncodedRawRecord {
                    content_hash: raw.content_hash(),
                    bytes: raw.bytes().to_vec(),
                    normalized: encode_structured(raw.normalized())
                        .map_err(|failure| encoding_corruption("normalized value", failure))?,
                    declared_entry_count,
                })
            })
            .collect::<Result<Vec<_>, ImportedConversationRepositoryError>>()?;
        let entries = conversation
            .entries()
            .iter()
            .map(|entry| {
                Ok(EncodedEntry {
                    identity: entry.identity(),
                    position: entry.position(),
                    raw_position: entry.raw_record_position(),
                    within_position: entry.record_entry_position(),
                    source_speaker: encode_source_speaker(entry.source_speaker()),
                    content: encode_content(entry.content())
                        .map_err(|failure| encoding_corruption("content", failure))?,
                    source: encode_source_metadata(entry.source())
                        .map_err(|failure| encoding_corruption("source metadata", failure))?,
                })
            })
            .collect::<Result<Vec<_>, ImportedConversationRepositoryError>>()?;
        Ok(Self {
            format,
            converter_version,
            raws,
            entries,
        })
    }
}

struct EncodedRawRecord {
    content_hash: ImportedRawRecordHash,
    bytes: Vec<u8>,
    normalized: Vec<u8>,
    declared_entry_count: u64,
}

struct EncodedEntry {
    identity: ImportedTranscriptEntryId,
    position: ImportedTranscriptPosition,
    raw_position: ImportedRawRecordPosition,
    within_position: ImportedRecordEntryPosition,
    source_speaker: &'static str,
    content: Vec<u8>,
    source: Vec<u8>,
}

async fn any_entry_identity_exists(
    connection: &mut PgConnection,
    entries: &[EncodedEntry],
) -> Result<bool, sqlx::Error> {
    let identities = entries
        .iter()
        .map(|entry| entry.identity.into_uuid())
        .collect::<Vec<_>>();
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM imported_transcript_entry
              WHERE imported_transcript_entry_id = ANY($1)
         )",
    )
    .bind(identities)
    .fetch_one(connection)
    .await
}

async fn insert_raws(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
    raws: &[EncodedRawRecord],
) -> Result<(), ImportedConversationRepositoryError> {
    for (index, raw) in raws.iter().enumerate() {
        let hash = raw.content_hash.as_bytes().as_slice();
        sqlx::query(
            "INSERT INTO imported_raw_source_record (content_hash, raw_bytes)
             VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(hash)
        .bind(&raw.bytes)
        .execute(&mut *connection)
        .await?;
        let durable_bytes: Vec<u8> = sqlx::query_scalar(
            "SELECT raw_bytes
               FROM imported_raw_source_record
              WHERE content_hash = $1",
        )
        .bind(hash)
        .fetch_one(&mut *connection)
        .await?;
        if durable_bytes != raw.bytes {
            return Err(ImportedConversationCorruption::RawRecordHashCollision.into());
        }
        sqlx::query(
            "INSERT INTO imported_conversation_raw_record
                (imported_conversation_id, raw_record_position, content_hash,
                 normalized_value_encoding, declared_entry_count)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(conversation.into_uuid())
        .bind(Decimal::from(ordinal(index)?))
        .bind(hash)
        .bind(&raw.normalized)
        .bind(Decimal::from(raw.declared_entry_count))
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn insert_entries(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
    entries: &[EncodedEntry],
) -> Result<(), ImportedConversationRepositoryError> {
    for entry in entries {
        sqlx::query(
            "INSERT INTO imported_transcript_entry
                (imported_conversation_id, imported_entry_position,
                 imported_transcript_entry_id, raw_record_position,
                 record_entry_position, source_speaker_kind, content_encoding,
                 source_metadata_encoding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(conversation.into_uuid())
        .bind(Decimal::from(entry.position.as_u64()))
        .bind(entry.identity.into_uuid())
        .bind(Decimal::from(entry.raw_position.as_u64()))
        .bind(Decimal::from(entry.within_position.as_u64()))
        .bind(entry.source_speaker)
        .bind(&entry.content)
        .bind(&entry.source)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn load_identity_by_source_digest(
    connection: &mut PgConnection,
    format: &str,
    converter_version: i16,
    source_digest: ImportedConversationSourceDigest,
) -> Result<Option<ImportedConversationId>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT imported_conversation_id
           FROM imported_conversation
          WHERE source_format = $1
            AND converter_version = $2
            AND source_digest = $3",
    )
    .bind(format)
    .bind(converter_version)
    .bind(source_digest.as_bytes().as_slice())
    .fetch_optional(connection)
    .await
    .map(|identity| identity.map(ImportedConversationId::from_uuid))
}

async fn load_from_connection(
    connection: &mut PgConnection,
    requested: ImportedConversationId,
) -> Result<Option<ImportedConversation>, ImportedConversationRepositoryError> {
    let header = sqlx::query(
        "SELECT imported_conversation_id, storage_version, source_format,
                converter_version, source_digest, declared_raw_record_count,
                declared_entry_count
           FROM imported_conversation
          WHERE imported_conversation_id = $1",
    )
    .bind(requested.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(header) = header else {
        return Ok(None);
    };
    decode_complete(connection, requested, header)
        .await
        .map(Some)
}

async fn decode_complete(
    connection: &mut PgConnection,
    requested: ImportedConversationId,
    header: PgRow,
) -> Result<ImportedConversation, ImportedConversationRepositoryError> {
    let stored = ImportedConversationId::from_uuid(header.try_get("imported_conversation_id")?);
    require_i16(&header, "storage_version", STORAGE_VERSION)?;
    let source_format: String = header.try_get("source_format")?;
    let converter_version: i16 = header.try_get("converter_version")?;
    let format = decode_format(&source_format, converter_version)?;
    let source_digest = digest_from_bytes(
        header.try_get("source_digest")?,
        "source digest",
        ImportedConversationSourceDigest::from_bytes,
    )?;
    let declared_raw_record_count = positive_u64(header.try_get("declared_raw_record_count")?)
        .map_err(|reason| invalid_ordinal_with_reason("declared raw-record count", reason))?;
    let declared_entry_count = positive_u64(header.try_get("declared_entry_count")?)
        .map_err(|reason| invalid_ordinal_with_reason("declared entry count", reason))?;

    let raw_rows = sqlx::query(
        "SELECT occurrence.raw_record_position, occurrence.content_hash,
                occurrence.normalized_value_encoding, blob.raw_bytes
           FROM imported_conversation_raw_record AS occurrence
           LEFT JOIN imported_raw_source_record AS blob
             ON blob.content_hash = occurrence.content_hash
          WHERE occurrence.imported_conversation_id = $1
          ORDER BY occurrence.raw_record_position",
    )
    .bind(stored.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut raws = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        let position = decode_raw_position(row.try_get("raw_record_position")?)?;
        let hash = digest_from_bytes(
            row.try_get("content_hash")?,
            "raw-record hash",
            ImportedRawRecordHash::from_bytes,
        )?;
        let bytes: Option<Vec<u8>> = row.try_get("raw_bytes")?;
        let bytes = bytes.ok_or(ImportedConversationCorruption::Missing("raw bytes"))?;
        let normalized_encoding: Vec<u8> = row.try_get("normalized_value_encoding")?;
        let normalized = decode_structured(&normalized_encoding)
            .map_err(|failure| encoding_corruption("normalized value", failure))?;
        raws.push(ImportedRawSourceRecordReconstitutionInput::new(
            position, hash, bytes, normalized,
        ));
    }

    let entry_rows = sqlx::query(
        "SELECT imported_entry_position, imported_transcript_entry_id,
                raw_record_position, record_entry_position,
                source_speaker_kind, content_encoding,
                source_metadata_encoding
           FROM imported_transcript_entry
          WHERE imported_conversation_id = $1
          ORDER BY imported_entry_position",
    )
    .bind(stored.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut entries = Vec::with_capacity(entry_rows.len());
    for row in entry_rows {
        let position = decode_entry_position(row.try_get("imported_entry_position")?)?;
        let identity =
            ImportedTranscriptEntryId::from_uuid(row.try_get("imported_transcript_entry_id")?);
        let raw_position = decode_raw_position(row.try_get("raw_record_position")?)?;
        let within_position = decode_within_position(row.try_get("record_entry_position")?)?;
        let source_speaker =
            decode_source_speaker(row.try_get::<String, _>("source_speaker_kind")?.as_str())?;
        let content_encoding: Vec<u8> = row.try_get("content_encoding")?;
        let content = decode_content(&content_encoding)
            .map_err(|failure| encoding_corruption("content", failure))?;
        let source_encoding: Vec<u8> = row.try_get("source_metadata_encoding")?;
        let source = decode_source_metadata(&source_encoding)
            .map_err(|failure| encoding_corruption("source metadata", failure))?;
        entries.push(ImportedTranscriptEntryInput::new(
            identity,
            stored,
            position,
            raw_position,
            within_position,
            source_speaker,
            content,
            source,
        ));
    }

    ImportedConversationReconstitutionInput::new(
        requested,
        stored,
        format,
        source_digest,
        declared_raw_record_count,
        raws,
        declared_entry_count,
        entries,
    )
    .reconstitute()
    .map_err(|error| ImportedConversationCorruption::Domain(error.failure()).into())
}

fn equivalent_snapshot(candidate: &ImportedConversation, existing: &ImportedConversation) -> bool {
    candidate.format() == existing.format()
        && candidate.source_digest() == existing.source_digest()
        && candidate.raw_records() == existing.raw_records()
        && candidate.entries().len() == existing.entries().len()
        && candidate
            .entries()
            .iter()
            .zip(existing.entries())
            .all(|(candidate, existing)| {
                candidate.position() == existing.position()
                    && candidate.raw_record_position() == existing.raw_record_position()
                    && candidate.record_entry_position() == existing.record_entry_position()
                    && candidate.source_speaker() == existing.source_speaker()
                    && candidate.content() == existing.content()
                    && candidate.source() == existing.source()
            })
}

fn encode_format(format: ImportedConversationFormat) -> (&'static str, i16) {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION)
        }
    }
}

fn decode_format(
    format: &str,
    converter_version: i16,
) -> Result<ImportedConversationFormat, ImportedConversationRepositoryError> {
    match (format, converter_version) {
        (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION) => {
            Ok(ImportedConversationFormat::ClaudeCodeSessionJsonlV1)
        }
        (_, version) if format == CLAUDE_CODE_FORMAT => {
            Err(ImportedConversationCorruption::Unsupported {
                field: "converter version",
                value: version.to_string(),
            }
            .into())
        }
        _ => Err(ImportedConversationCorruption::Unsupported {
            field: "source format",
            value: String::from(format),
        }
        .into()),
    }
}

fn encode_source_speaker(speaker: &ImportedSourceAttestation<ImportedSpeaker>) -> &'static str {
    match speaker {
        ImportedSourceAttestation::NotAttested => "not_attested",
        ImportedSourceAttestation::AttestedAbsent => "attested_absent",
        ImportedSourceAttestation::Attested(ImportedSpeaker::User) => "attested_user",
        ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant) => "attested_assistant",
    }
}

fn decode_source_speaker(
    value: &str,
) -> Result<ImportedSourceAttestation<ImportedSpeaker>, ImportedConversationRepositoryError> {
    match value {
        "not_attested" => Ok(ImportedSourceAttestation::NotAttested),
        "attested_absent" => Ok(ImportedSourceAttestation::AttestedAbsent),
        "attested_user" => Ok(ImportedSourceAttestation::Attested(ImportedSpeaker::User)),
        "attested_assistant" => Ok(ImportedSourceAttestation::Attested(
            ImportedSpeaker::Assistant,
        )),
        _ => Err(ImportedConversationCorruption::Unsupported {
            field: "source speaker",
            value: String::from(value),
        }
        .into()),
    }
}

fn digest_from_bytes<Value>(
    bytes: Vec<u8>,
    field: &'static str,
    constructor: impl FnOnce([u8; 32]) -> Value,
) -> Result<Value, ImportedConversationRepositoryError> {
    let bytes = <[u8; 32]>::try_from(bytes)
        .map_err(|_| ImportedConversationCorruption::InvalidDigestSize(field))?;
    Ok(constructor(bytes))
}

fn require_i16(
    row: &PgRow,
    field: &'static str,
    expected: i16,
) -> Result<(), ImportedConversationRepositoryError> {
    let actual: i16 = row.try_get(field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ImportedConversationCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn positive_u64(value: Decimal) -> Result<u64, PositiveOrdinalMappingError> {
    if !value.fract().is_zero() {
        return Err(PositiveOrdinalMappingError::Fractional);
    }
    if value <= Decimal::ZERO {
        return Err(PositiveOrdinalMappingError::NonPositive);
    }
    u64::try_from(value).map_err(|_| PositiveOrdinalMappingError::OutOfRange)
}

fn decode_raw_position(
    value: Decimal,
) -> Result<ImportedRawRecordPosition, ImportedConversationRepositoryError> {
    let value = positive_u64(value)
        .map_err(|reason| invalid_ordinal_with_reason("raw-record position", reason))?;
    ImportedRawRecordPosition::try_from_u64(value)
        .ok_or_else(|| invalid_ordinal("raw-record position"))
}

fn decode_entry_position(
    value: Decimal,
) -> Result<ImportedTranscriptPosition, ImportedConversationRepositoryError> {
    let value = positive_u64(value)
        .map_err(|reason| invalid_ordinal_with_reason("entry position", reason))?;
    ImportedTranscriptPosition::try_from_u64(value).ok_or_else(|| invalid_ordinal("entry position"))
}

fn decode_within_position(
    value: Decimal,
) -> Result<ImportedRecordEntryPosition, ImportedConversationRepositoryError> {
    let value = positive_u64(value)
        .map_err(|reason| invalid_ordinal_with_reason("record entry position", reason))?;
    ImportedRecordEntryPosition::try_from_u64(value)
        .ok_or_else(|| invalid_ordinal("record entry position"))
}

fn ordinal(index: usize) -> Result<u64, ImportedConversationRepositoryError> {
    u64::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| invalid_ordinal("raw-record position"))
}

fn usize_to_u64(
    value: usize,
    field: &'static str,
) -> Result<u64, ImportedConversationRepositoryError> {
    u64::try_from(value).map_err(|_| invalid_ordinal(field))
}

fn invalid_ordinal(field: &'static str) -> ImportedConversationRepositoryError {
    invalid_ordinal_with_reason(field, PositiveOrdinalMappingError::OutOfRange)
}

fn invalid_ordinal_with_reason(
    field: &'static str,
    reason: PositiveOrdinalMappingError,
) -> ImportedConversationRepositoryError {
    ImportedConversationCorruption::InvalidOrdinal { field, reason }.into()
}

fn encoding_corruption(
    field: &'static str,
    failure: CodecFailure,
) -> ImportedConversationRepositoryError {
    ImportedConversationCorruption::Encoding {
        field,
        failure: failure.into(),
    }
    .into()
}

impl From<CodecFailure> for ImportedConversationEncodingCorruption {
    fn from(failure: CodecFailure) -> Self {
        match failure {
            CodecFailure::LengthOutOfRange => Self::LengthOutOfRange,
            CodecFailure::UnexpectedEnd => Self::UnexpectedEnd,
            CodecFailure::TrailingBytes => Self::TrailingBytes,
            CodecFailure::UnsupportedVersion(value) => Self::UnsupportedVersion(value),
            CodecFailure::UnsupportedTag { kind, value } => Self::UnsupportedTag { kind, value },
            CodecFailure::InvalidUtf8(kind) => Self::InvalidUtf8(kind),
            CodecFailure::InvalidJsonNumber => Self::InvalidJsonNumber,
            CodecFailure::ContainerDepthExceeded => Self::ContainerDepthExceeded,
        }
    }
}
