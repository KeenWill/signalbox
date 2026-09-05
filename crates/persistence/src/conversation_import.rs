//! Append-only PostgreSQL storage for imported conversation snapshots.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
};

use rust_decimal::Decimal;
use signalbox_application::{ImportedConversationStore, ImportedConversationStoreOutcome};
use signalbox_blob_store::{BlobObjectKey, BlobStoreName, ExpectedBlob};
use signalbox_domain::{
    BlobDigest, ImportedConversation, ImportedConversationDisplayTitle, ImportedConversationFormat,
    ImportedConversationId, ImportedConversationReconstitutionFailure,
    ImportedConversationReconstitutionInput, ImportedConversationSourceDigest,
    ImportedRawRecordConversionDigest, ImportedRawRecordHash, ImportedRawRecordPosition,
    ImportedRawSourceRecordReconstitutionInput, ImportedRecordEntryPosition,
    ImportedSourceAttestation, ImportedSpeaker, ImportedTranscriptEntryId,
    ImportedTranscriptEntryInput, ImportedTranscriptFrontier, ImportedTranscriptPosition,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    blob::{
        BlobCatalogRepositoryError, BlobReplicaRecord, BlobStoreBindingRecord,
        register_verified_replica_in_transaction,
    },
    conversation_import_codec::{
        ImportedConversationEncodingFailure as CodecFailure, decode_content,
        decode_source_metadata, decode_structured, encode_content, encode_source_metadata,
        encode_structured,
    },
    mapping::PositiveOrdinalMappingError,
};

const STORAGE_VERSION: i16 = 1;
const CLAUDE_CODE_FORMAT: &str = "claude_code_session_jsonl";
const CLAUDE_CODE_VERSION_ONE: i16 = 1;
const CLAUDE_CODE_VERSION_TWO: i16 = 2;
const CODEX_FORMAT: &str = "codex_rollout_jsonl";
const CODEX_VERSION_ONE: i16 = 1;
const TRANSCRIPT_ENTRY_IDENTITY_UNIQUE: &str = "imported_transcript_entry_identity_unique";
pub(crate) const DISPLAY_TITLE_STATE_DERIVED: &str = "derived";
pub(crate) const DISPLAY_TITLE_STATE_UNDERIVABLE: &str = "underivable";

/// One exact imported-source blob supplied to the deployment adapter.
#[derive(Clone)]
pub struct ImportedRawBlobInput {
    expected: ExpectedBlob,
    bytes: Arc<[u8]>,
}

impl ImportedRawBlobInput {
    /// Constructs one already-hashed positive-length source record.
    pub const fn new(expected: ExpectedBlob, bytes: Arc<[u8]>) -> Self {
        Self { expected, bytes }
    }

    /// Returns the expected identity and length.
    pub const fn expected(&self) -> ExpectedBlob {
        self.expected
    }

    /// Borrows the exact source bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Clones the shared exact source bytes without copying their allocation.
    pub fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

impl fmt::Debug for ImportedRawBlobInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedRawBlobInput")
            .field("expected", &self.expected)
            .field("bytes", &"<redacted>")
            .finish()
    }
}

/// Verified publication facts registered with the importing aggregate.
#[derive(Clone, Debug)]
pub struct ImportedRawBlobPublication {
    expected: ExpectedBlob,
    store: BlobStoreName,
    namespace_id: Uuid,
    object_key: BlobObjectKey,
}

impl ImportedRawBlobPublication {
    /// Constructs one verified deployment placement.
    pub const fn new(
        expected: ExpectedBlob,
        store: BlobStoreName,
        namespace_id: Uuid,
        object_key: BlobObjectKey,
    ) -> Self {
        Self {
            expected,
            store,
            namespace_id,
            object_key,
        }
    }

    /// Returns the verified immutable identity and byte length.
    pub const fn expected(&self) -> ExpectedBlob {
        self.expected
    }

    /// Returns the deployment store holding the verified object.
    pub const fn store(&self) -> &BlobStoreName {
        &self.store
    }

    /// Returns the deployment namespace bound to the store.
    pub const fn namespace_id(&self) -> Uuid {
        self.namespace_id
    }

    /// Returns the verified immutable object key.
    pub const fn object_key(&self) -> &BlobObjectKey {
        &self.object_key
    }
}

/// Content-silent imported-source store failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedRawBlobStorageError {
    /// No bounded store operation can currently complete.
    Unavailable,
    /// Durable catalog or object bytes disagreed.
    Integrity,
}

impl fmt::Display for ImportedRawBlobStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "imported raw blob storage is unavailable",
            Self::Integrity => "imported raw blob storage failed integrity verification",
        })
    }
}

impl Error for ImportedRawBlobStorageError {}

/// Bounded asynchronous publication result owned by an imported-source adapter.
pub type ImportedRawBlobPublicationFuture<'storage> = Pin<
    Box<
        dyn Future<Output = Result<Box<[ImportedRawBlobPublication]>, ImportedRawBlobStorageError>>
            + Send
            + 'storage,
    >,
>;

/// Bounded asynchronous checked-read result owned by an imported-source adapter.
pub type ImportedRawBlobReadFuture<'storage> = Pin<
    Box<dyn Future<Output = Result<Box<[Vec<u8>]>, ImportedRawBlobStorageError>> + Send + 'storage>,
>;

/// Deployment adapter for sequential publication and checked aggregate reads.
pub trait ImportedRawBlobStorage: fmt::Debug + Send + Sync {
    /// Publishes or verifies each distinct source record in supplied order.
    fn publish(&self, blobs: Box<[ImportedRawBlobInput]>) -> ImportedRawBlobPublicationFuture<'_>;

    /// Reads and verifies each distinct source record in supplied order after
    /// enforcing the complete occurrence-expanded source size.
    fn read(
        &self,
        blobs: Box<[ExpectedBlob]>,
        total_source_bytes: u64,
    ) -> ImportedRawBlobReadFuture<'_>;
}

#[cfg(feature = "postgres-integration")]
#[derive(Debug)]
struct IntegrationImportedRawBlobStorage;

#[cfg(feature = "postgres-integration")]
fn integration_imported_blobs() -> &'static std::sync::Mutex<BTreeMap<BlobDigest, Arc<[u8]>>> {
    static BLOBS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<BlobDigest, Arc<[u8]>>>> =
        std::sync::OnceLock::new();
    BLOBS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Replaces one integration fixture object to prove checked reads fail closed.
#[cfg(feature = "postgres-integration")]
pub fn corrupt_integration_imported_blob(
    digest: BlobDigest,
    bytes: Arc<[u8]>,
) -> Result<(), ImportedRawBlobStorageError> {
    let mut retained = integration_imported_blobs()
        .lock()
        .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
    let stored = retained
        .get_mut(&digest)
        .ok_or(ImportedRawBlobStorageError::Integrity)?;
    *stored = bytes;
    Ok(())
}

#[cfg(feature = "postgres-integration")]
impl ImportedRawBlobStorage for IntegrationImportedRawBlobStorage {
    fn publish(&self, blobs: Box<[ImportedRawBlobInput]>) -> ImportedRawBlobPublicationFuture<'_> {
        Box::pin(async move {
            let store = BlobStoreName::try_new("integration")
                .map_err(|_| ImportedRawBlobStorageError::Integrity)?;
            let namespace = Uuid::from_u128(0x696d706f727465645f736f75726365);
            let mut retained = integration_imported_blobs()
                .lock()
                .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
            let mut publications = Vec::with_capacity(blobs.len());
            for blob in blobs {
                let expected = blob.expected();
                if BlobDigest::digest(blob.bytes()) != expected.digest()
                    || u64::try_from(blob.bytes().len()).ok() != Some(expected.byte_length())
                {
                    return Err(ImportedRawBlobStorageError::Integrity);
                }
                if let Some(existing) = retained.get(&expected.digest()) {
                    if existing.as_ref() != blob.bytes() {
                        return Err(ImportedRawBlobStorageError::Integrity);
                    }
                } else {
                    retained.insert(expected.digest(), blob.shared_bytes());
                }
                publications.push(ImportedRawBlobPublication::new(
                    expected,
                    store.clone(),
                    namespace,
                    BlobObjectKey::for_digest(expected.digest()),
                ));
            }
            Ok(publications.into_boxed_slice())
        })
    }

    fn read(
        &self,
        blobs: Box<[ExpectedBlob]>,
        _total_source_bytes: u64,
    ) -> ImportedRawBlobReadFuture<'_> {
        Box::pin(async move {
            let retained = integration_imported_blobs()
                .lock()
                .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
            let mut contents = Vec::with_capacity(blobs.len());
            for expected in blobs {
                let bytes = retained
                    .get(&expected.digest())
                    .ok_or(ImportedRawBlobStorageError::Integrity)?;
                if u64::try_from(bytes.len()).ok() != Some(expected.byte_length()) {
                    return Err(ImportedRawBlobStorageError::Integrity);
                }
                contents.push(bytes.to_vec());
            }
            Ok(contents.into_boxed_slice())
        })
    }
}

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
    /// A versioned value belongs to another top-level payload kind.
    UnexpectedPayloadKind {
        /// Payload kind required by this column.
        expected: u8,
        /// Payload kind found in the stored bytes.
        actual: u8,
    },
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
    /// A raw occurrence's declared normalized-entry count is not exact.
    RawRecordDeclaredEntryCountMismatch {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
        /// Stored occurrence count.
        declared: u64,
        /// Reconstructed member count.
        actual: u64,
    },
    /// Non-null source-session evidence disagrees with the reconstructed entries.
    SourceSessionLineageMismatch,
    /// A resolved display title disagrees with re-derivation from the records.
    DisplayTitleMismatch,
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
            Self::RawRecordDeclaredEntryCountMismatch {
                position,
                declared,
                actual,
            } => write!(
                formatter,
                "imported raw record {position:?} declares {declared} entries but reconstructs {actual}"
            ),
            Self::SourceSessionLineageMismatch => formatter
                .write_str("imported source-session lineage disagrees with reconstructed entries"),
            Self::DisplayTitleMismatch => formatter
                .write_str("imported display title disagrees with re-derivation from the records"),
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
    /// Blob publication or checked reading could not complete.
    BlobStorage(ImportedRawBlobStorageError),
    /// Published placement facts could not join the importing transaction.
    BlobCatalog(BlobCatalogRepositoryError),
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
            Self::BlobStorage(error) => error.fmt(formatter),
            Self::BlobCatalog(error) => error.fmt(formatter),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ImportedConversationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::IdentityCollision(_) => None,
            Self::BlobStorage(error) => Some(error),
            Self::BlobCatalog(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ImportedConversationRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
            == Some(TRANSCRIPT_ENTRY_IDENTITY_UNIQUE)
        {
            Self::IdentityCollision(ImportedConversationIdentityCollision::TranscriptEntry)
        } else {
            Self::Database(error)
        }
    }
}

impl From<ImportedConversationCorruption> for ImportedConversationRepositoryError {
    fn from(error: ImportedConversationCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<ImportedRawBlobStorageError> for ImportedConversationRepositoryError {
    fn from(error: ImportedRawBlobStorageError) -> Self {
        Self::BlobStorage(error)
    }
}

impl From<BlobCatalogRepositoryError> for ImportedConversationRepositoryError {
    fn from(error: BlobCatalogRepositoryError) -> Self {
        Self::BlobCatalog(error)
    }
}

/// PostgreSQL implementation of pure, idempotent conversation ingestion.
#[derive(Clone, Debug)]
pub struct ImportedConversationRepository {
    pool: PgPool,
    blob_storage: Arc<dyn ImportedRawBlobStorage>,
}

impl ImportedConversationRepository {
    /// Uses the supplied pool for atomic insertion and checked complete loads.
    pub fn with_blob_storage(pool: PgPool, blob_storage: Arc<dyn ImportedRawBlobStorage>) -> Self {
        Self { pool, blob_storage }
    }

    /// Uses the deterministic integration-store fixture.
    #[cfg(feature = "postgres-integration")]
    pub fn new(pool: PgPool) -> Self {
        Self::with_blob_storage(pool, Arc::new(IntegrationImportedRawBlobStorage))
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
        let raw_source_bytes = encoded.raw_source_bytes()?;
        let normalized_source_record_bytes = encoded.normalized_source_record_bytes()?;
        let normalized_entry_bytes = encoded.normalized_entry_bytes()?;
        // Publish before resolving a duplicate. The ingestion contract in
        // `docs/spec/blob-storage.md` makes re-ingest rediscover the object: a
        // missing or corrupt routed object is repaired from the supplied
        // bytes, and an identity whose only healthy replica sits in a
        // historical store gains one in the currently routed store. Resolving
        // first would instead run a checked load against the damaged replica
        // and return `Integrity`, so an exact re-import could never repair the
        // aggregate. Publication short-circuits on a live-verified routed
        // replica, so the healthy duplicate path still uploads nothing, and
        // replica registration is idempotent.
        let publications = publish_raw_blobs(self.blob_storage.as_ref(), &encoded.raws).await?;
        let mut registration = self.pool.begin().await?;
        register_raw_blobs(&mut registration, &publications).await?;
        registration.commit().await?;
        if let Some(existing) = self
            .resolve_existing_snapshot(
                &conversation,
                encoded.format,
                encoded.converter_version,
                source_digest,
            )
            .await?
        {
            return Ok(existing);
        }
        let mut transaction = self.pool.begin().await?;
        register_raw_blobs(&mut transaction, &publications).await?;
        insert_raw_blob_references(&mut transaction, &encoded.raws).await?;
        let inserted = sqlx::query(
            "INSERT INTO imported_conversation
                (imported_conversation_id, storage_version, source_format,
                 converter_version, source_digest, source_session_id,
                 declared_raw_record_count, declared_entry_count,
                 display_title, display_title_state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT DO NOTHING",
        )
        .bind(candidate_id.into_uuid())
        .bind(STORAGE_VERSION)
        .bind(encoded.format)
        .bind(encoded.converter_version)
        .bind(source_digest.as_bytes().as_slice())
        .bind(encoded.source_session_id.as_deref())
        .bind(Decimal::from(declared_raw_record_count))
        .bind(Decimal::from(declared_entry_count))
        .bind(encoded.display_title.as_deref())
        .bind(resolved_display_title_state(
            encoded.display_title.as_deref(),
        ))
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !inserted {
            transaction.rollback().await?;
            let Some(existing) = self
                .resolve_existing_snapshot(
                    &conversation,
                    encoded.format,
                    encoded.converter_version,
                    source_digest,
                )
                .await?
            else {
                return Err(ImportedConversationRepositoryError::IdentityCollision(
                    ImportedConversationIdentityCollision::Conversation,
                ));
            };
            return Ok(existing);
        }

        if any_entry_identity_exists(&mut transaction, &encoded.entries).await? {
            transaction.rollback().await?;
            return Err(ImportedConversationRepositoryError::IdentityCollision(
                ImportedConversationIdentityCollision::TranscriptEntry,
            ));
        }
        insert_raw_occurrences(&mut transaction, candidate_id, &encoded.raws).await?;
        insert_entries(&mut transaction, candidate_id, &encoded.entries).await?;
        insert_size_totals(
            &mut transaction,
            candidate_id,
            raw_source_bytes,
            normalized_source_record_bytes,
            normalized_entry_bytes,
        )
        .await?;
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
        let projection = load_projection_from_connection(&mut connection, conversation).await?;
        drop(connection);
        match projection {
            Some(projection) => self.finish_projection(projection).await.map(Some),
            None => Ok(None),
        }
    }

    async fn resolve_existing_snapshot(
        &self,
        candidate: &ImportedConversation,
        format: &str,
        converter_version: i16,
        source_digest: ImportedConversationSourceDigest,
    ) -> Result<Option<ImportedConversationStoreOutcome>, ImportedConversationRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        let existing_id = load_identity_by_source_digest(
            &mut connection,
            format,
            converter_version,
            source_digest,
        )
        .await?;
        drop(connection);
        let Some(existing_id) = existing_id else {
            return Ok(None);
        };
        let existing = self
            .load(existing_id)
            .await?
            .ok_or(ImportedConversationCorruption::ExistingSnapshotMismatch)?;
        if !equivalent_snapshot(candidate, &existing) {
            return Err(ImportedConversationCorruption::ExistingSnapshotMismatch.into());
        }
        Ok(Some(ImportedConversationStoreOutcome::AlreadyImported {
            conversation: existing.id(),
            source_digest: existing.source_digest(),
        }))
    }

    async fn finish_projection(
        &self,
        projection: StoredConversationProjection,
    ) -> Result<ImportedConversation, ImportedConversationRepositoryError> {
        finish_projection(self.blob_storage.as_ref(), projection).await
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
    source_session_id: Option<Vec<u8>>,
    display_title: Option<String>,
    raws: Vec<EncodedRawRecord>,
    entries: Vec<EncodedEntry>,
}

impl EncodedConversation {
    fn from_domain(
        conversation: &ImportedConversation,
    ) -> Result<Self, ImportedConversationRepositoryError> {
        let (format, converter_version) = encode_format(conversation.format());
        let source_session_id = consistent_source_session_id(conversation).map(<[u8]>::to_vec);
        let display_title = ImportedConversationDisplayTitle::derive(conversation)
            .map(ImportedConversationDisplayTitle::into_string);
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
                    conversion_digest: raw.conversion_digest(),
                    bytes: Arc::from(raw.bytes()),
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
            source_session_id,
            display_title,
            raws,
            entries,
        })
    }

    fn raw_source_bytes(&self) -> Result<u64, ImportedConversationRepositoryError> {
        encoded_byte_total(
            self.raws.iter().map(|raw| raw.bytes.len()),
            "raw source bytes",
        )
    }

    fn normalized_source_record_bytes(&self) -> Result<u64, ImportedConversationRepositoryError> {
        encoded_byte_total(
            self.raws.iter().map(|raw| raw.normalized.len()),
            "normalized source-record bytes",
        )
    }

    fn normalized_entry_bytes(&self) -> Result<u64, ImportedConversationRepositoryError> {
        encoded_byte_total(
            self.entries
                .iter()
                .flat_map(|entry| [entry.content.len(), entry.source.len()]),
            "normalized entry bytes",
        )
    }
}

fn encoded_byte_total(
    mut lengths: impl Iterator<Item = usize>,
    field: &'static str,
) -> Result<u64, ImportedConversationRepositoryError> {
    lengths.try_fold(0_u64, |total, length| {
        total
            .checked_add(usize_to_u64(length, field)?)
            .ok_or_else(|| invalid_ordinal(field))
    })
}

/// Maps a derived-or-absent display title to its closed resolved state.
///
/// Insertion always writes one of the final resolved title states.
fn resolved_display_title_state(display_title: Option<&str>) -> &'static str {
    if display_title.is_some() {
        DISPLAY_TITLE_STATE_DERIVED
    } else {
        DISPLAY_TITLE_STATE_UNDERIVABLE
    }
}

fn consistent_source_session_id(conversation: &ImportedConversation) -> Option<&[u8]> {
    let mut consistent = None;
    for entry in conversation.entries() {
        if let ImportedSourceAttestation::Attested(source_session_id) =
            entry.source().source_session_id()
        {
            let source_session_id = source_session_id.as_str().as_bytes();
            match consistent {
                None => consistent = Some(source_session_id),
                Some(existing) if existing == source_session_id => {}
                Some(_) => return None,
            }
        }
    }
    consistent
}

struct EncodedRawRecord {
    content_hash: ImportedRawRecordHash,
    conversion_digest: ImportedRawRecordConversionDigest,
    bytes: Arc<[u8]>,
    normalized: Vec<u8>,
    declared_entry_count: u64,
}

impl EncodedRawRecord {
    fn expected(&self) -> Result<ExpectedBlob, ImportedConversationRepositoryError> {
        ExpectedBlob::try_new(
            BlobDigest::from_bytes(*self.content_hash.as_bytes()),
            u64::try_from(self.bytes.len())
                .map_err(|_| invalid_ordinal("raw-record byte length"))?,
        )
        .map_err(|_| invalid_ordinal("raw-record byte length"))
    }
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

async fn publish_raw_blobs(
    storage: &dyn ImportedRawBlobStorage,
    raws: &[EncodedRawRecord],
) -> Result<Box<[ImportedRawBlobPublication]>, ImportedConversationRepositoryError> {
    let mut blobs = raw_blobs_in_key_order(raws);
    if blobs
        .windows(2)
        .any(|pair| pair[0].content_hash == pair[1].content_hash && pair[0].bytes != pair[1].bytes)
    {
        return Err(ImportedConversationCorruption::RawRecordHashCollision.into());
    }
    blobs.dedup_by_key(|raw| raw.content_hash);
    let blobs = blobs
        .into_iter()
        .map(|raw| {
            Ok(ImportedRawBlobInput::new(
                raw.expected()?,
                raw.bytes.clone(),
            ))
        })
        .collect::<Result<Vec<_>, ImportedConversationRepositoryError>>()?;
    storage
        .publish(blobs.into_boxed_slice())
        .await
        .map_err(Into::into)
}

async fn register_raw_blobs(
    transaction: &mut Transaction<'_, Postgres>,
    publications: &[ImportedRawBlobPublication],
) -> Result<(), ImportedConversationRepositoryError> {
    for publication in publications {
        let binding =
            BlobStoreBindingRecord::new(publication.store.clone(), publication.namespace_id);
        let replica =
            BlobReplicaRecord::new(publication.store.clone(), publication.object_key.clone());
        register_verified_replica_in_transaction(
            transaction,
            publication.expected,
            &binding,
            &replica,
        )
        .await?;
    }
    Ok(())
}

async fn insert_raw_blob_references(
    connection: &mut PgConnection,
    raws: &[EncodedRawRecord],
) -> Result<(), ImportedConversationRepositoryError> {
    for raw in raw_blobs_in_key_order(raws) {
        sqlx::query(
            "INSERT INTO imported_raw_source_record (content_hash)
             VALUES ($1)
             ON CONFLICT DO NOTHING",
        )
        .bind(raw.content_hash.as_bytes().as_slice())
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

fn raw_blobs_in_key_order(raws: &[EncodedRawRecord]) -> Vec<&EncodedRawRecord> {
    let mut blobs = raws.iter().collect::<Vec<_>>();
    blobs.sort_unstable_by_key(|raw| raw.content_hash);
    blobs
}

async fn insert_raw_occurrences(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
    raws: &[EncodedRawRecord],
) -> Result<(), ImportedConversationRepositoryError> {
    for (index, raw) in raws.iter().enumerate() {
        let hash = raw.content_hash.as_bytes().as_slice();
        sqlx::query(
            "INSERT INTO imported_conversation_raw_record
                (imported_conversation_id, raw_record_position, content_hash,
                 conversion_digest, normalized_value_encoding,
                 declared_entry_count)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(conversation.into_uuid())
        .bind(Decimal::from(ordinal(index)?))
        .bind(hash)
        .bind(raw.conversion_digest.as_bytes().as_slice())
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
    for entry in entries_in_key_order(entries) {
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

async fn insert_size_totals(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
    raw_source_bytes: u64,
    normalized_source_record_bytes: u64,
    normalized_entry_bytes: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO imported_conversation_size_totals
            (imported_conversation_id, raw_source_bytes,
             normalized_source_record_bytes, normalized_entry_bytes)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(conversation.into_uuid())
    .bind(Decimal::from(raw_source_bytes))
    .bind(Decimal::from(normalized_source_record_bytes))
    .bind(Decimal::from(normalized_entry_bytes))
    .execute(connection)
    .await?;
    Ok(())
}

fn entries_in_key_order(entries: &[EncodedEntry]) -> Vec<&EncodedEntry> {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.identity);
    entries
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

pub(crate) async fn load_projection_from_connection(
    connection: &mut PgConnection,
    requested: ImportedConversationId,
) -> Result<Option<StoredConversationProjection>, ImportedConversationRepositoryError> {
    let header = sqlx::query(
        "SELECT imported_conversation_id, storage_version, source_format,
                converter_version, source_digest, source_session_id,
                declared_raw_record_count, declared_entry_count,
                display_title, display_title_state
           FROM imported_conversation
          WHERE imported_conversation_id = $1",
    )
    .bind(requested.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(header) = header else {
        return Ok(None);
    };
    decode_projection(connection, requested, header)
        .await
        .map(Some)
}

/// Loads the normalized runtime prefix without touching raw audit bytes.
pub(crate) async fn load_normalized_prefix_from_connection(
    connection: &mut PgConnection,
    frontier: ImportedTranscriptFrontier,
) -> Result<Option<Vec<ImportedTranscriptEntryInput>>, ImportedConversationRepositoryError> {
    load_normalized_entries_from_connection(
        connection,
        frontier.conversation(),
        Some(frontier.through_position().as_u64()),
    )
    .await
}

/// Loads one conversation's normalized runtime entries without audit bytes.
pub async fn load_normalized_entries(
    pool: &PgPool,
    conversation: ImportedConversationId,
) -> Result<Option<Box<[ImportedTranscriptEntryInput]>>, ImportedConversationRepositoryError> {
    let mut connection = pool.acquire().await?;
    load_normalized_entries_from_connection(&mut connection, conversation, None)
        .await
        .map(|entries| entries.map(Vec::into_boxed_slice))
}

async fn load_normalized_entries_from_connection(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
    through_position: Option<u64>,
) -> Result<Option<Vec<ImportedTranscriptEntryInput>>, ImportedConversationRepositoryError> {
    let header = sqlx::query(
        "SELECT conversation.storage_version,
                conversation.declared_entry_count,
                inventory.actual_entry_count,
                inventory.inventory_is_complete
           FROM imported_conversation AS conversation
           CROSS JOIN LATERAL (
               SELECT COUNT(*)::numeric AS actual_entry_count,
                      COUNT(*)::numeric = conversation.declared_entry_count
                      AND MAX(imported_entry_position) =
                          conversation.declared_entry_count
                          AS inventory_is_complete
                 FROM imported_transcript_entry
                WHERE imported_conversation_id =
                      conversation.imported_conversation_id
           ) AS inventory
          WHERE conversation.imported_conversation_id = $1",
    )
    .bind(conversation.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(header) = header else {
        return Ok(None);
    };
    require_i16(&header, "storage_version", STORAGE_VERSION)?;
    let declared_entry_count = positive_u64(header.try_get("declared_entry_count")?)
        .map_err(|reason| invalid_ordinal_with_reason("declared entry count", reason))?;
    let actual_entry_count: Decimal = header.try_get("actual_entry_count")?;
    let actual_entry_count =
        u64::try_from(actual_entry_count).map_err(|_| invalid_ordinal("actual entry count"))?;
    if !header.try_get::<bool, _>("inventory_is_complete")? {
        return Err(ImportedConversationCorruption::Domain(
            ImportedConversationReconstitutionFailure::DeclaredEntryCountMismatch {
                declared: declared_entry_count,
                actual: usize::try_from(actual_entry_count)
                    .map_err(|_| invalid_ordinal("actual entry count"))?,
            },
        )
        .into());
    }
    let rows = sqlx::query(
        "SELECT imported_entry_position, imported_transcript_entry_id,
                raw_record_position, record_entry_position,
                source_speaker_kind, content_encoding,
                source_metadata_encoding
          FROM imported_transcript_entry
          WHERE imported_conversation_id = $1
            AND ($2::numeric IS NULL OR imported_entry_position <= $2)
          ORDER BY imported_entry_position",
    )
    .bind(conversation.into_uuid())
    .bind(through_position.map(Decimal::from))
    .fetch_all(&mut *connection)
    .await?;
    let mut entries = Vec::with_capacity(rows.len());
    let mut expected_position = ImportedTranscriptPosition::first();
    let mut identities = BTreeSet::new();
    for row in rows {
        let position = decode_entry_position(row.try_get("imported_entry_position")?)?;
        let identity =
            ImportedTranscriptEntryId::from_uuid(row.try_get("imported_transcript_entry_id")?);
        if position != expected_position {
            return Err(ImportedConversationCorruption::Domain(
                ImportedConversationReconstitutionFailure::EntryPositionMismatch {
                    entry: identity,
                    expected: expected_position,
                    actual: position,
                },
            )
            .into());
        }
        expected_position = expected_position
            .checked_next()
            .ok_or_else(|| invalid_ordinal("normalized entry position"))?;
        if !identities.insert(identity) {
            return Err(ImportedConversationCorruption::Domain(
                ImportedConversationReconstitutionFailure::DuplicateEntry { entry: identity },
            )
            .into());
        }
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
            conversation,
            position,
            raw_position,
            within_position,
            source_speaker,
            content,
            source,
        ));
    }
    Ok(Some(entries))
}

pub(crate) struct StoredConversationProjection {
    requested: ImportedConversationId,
    stored: ImportedConversationId,
    format: ImportedConversationFormat,
    source_digest: ImportedConversationSourceDigest,
    source_session_id: Option<Vec<u8>>,
    display_title: Option<String>,
    display_title_state: String,
    declared_raw_record_count: u64,
    raws: Vec<StoredRawProjection>,
    declared_entry_count: u64,
    entries: Vec<ImportedTranscriptEntryInput>,
}

struct StoredRawProjection {
    position: ImportedRawRecordPosition,
    hash: ImportedRawRecordHash,
    conversion_digest: ImportedRawRecordConversionDigest,
    expected: ExpectedBlob,
    normalized: signalbox_domain::ImportedStructuredValue,
}

async fn decode_projection(
    connection: &mut PgConnection,
    requested: ImportedConversationId,
    header: PgRow,
) -> Result<StoredConversationProjection, ImportedConversationRepositoryError> {
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
    let source_session_id: Option<Vec<u8>> = header.try_get("source_session_id")?;
    let display_title: Option<String> = header.try_get("display_title")?;
    let display_title_state: String = header.try_get("display_title_state")?;
    let declared_raw_record_count = positive_u64(header.try_get("declared_raw_record_count")?)
        .map_err(|reason| invalid_ordinal_with_reason("declared raw-record count", reason))?;
    let declared_entry_count = positive_u64(header.try_get("declared_entry_count")?)
        .map_err(|reason| invalid_ordinal_with_reason("declared entry count", reason))?;

    let raw_rows = sqlx::query(
        "SELECT occurrence.raw_record_position, occurrence.content_hash,
                occurrence.conversion_digest, occurrence.normalized_value_encoding,
                occurrence.declared_entry_count, blob.byte_length
           FROM imported_conversation_raw_record AS occurrence
           LEFT JOIN imported_raw_source_record AS raw
             ON raw.content_hash = occurrence.content_hash
           LEFT JOIN blob
             ON blob.digest = raw.content_hash
          WHERE occurrence.imported_conversation_id = $1
          ORDER BY occurrence.raw_record_position",
    )
    .bind(stored.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut raws = Vec::with_capacity(raw_rows.len());
    let mut declared_entries_by_raw = BTreeMap::new();
    for row in raw_rows {
        let position = decode_raw_position(row.try_get("raw_record_position")?)?;
        let declared_entry_count =
            positive_u64(row.try_get("declared_entry_count")?).map_err(|reason| {
                invalid_ordinal_with_reason("raw-record declared entry count", reason)
            })?;
        let hash = digest_from_bytes(
            row.try_get("content_hash")?,
            "raw-record hash",
            ImportedRawRecordHash::from_bytes,
        )?;
        let conversion_digest = digest_from_bytes(
            row.try_get("conversion_digest")?,
            "raw-record conversion digest",
            ImportedRawRecordConversionDigest::from_bytes,
        )?;
        let byte_length: Option<Decimal> = row.try_get("byte_length")?;
        let byte_length = byte_length.ok_or(ImportedConversationCorruption::Missing(
            "raw blob reference",
        ))?;
        let byte_length = positive_u64(byte_length)
            .map_err(|reason| invalid_ordinal_with_reason("raw-record byte length", reason))?;
        let expected = ExpectedBlob::try_new(BlobDigest::from_bytes(*hash.as_bytes()), byte_length)
            .map_err(|_| invalid_ordinal("raw-record byte length"))?;
        let normalized_encoding: Vec<u8> = row.try_get("normalized_value_encoding")?;
        let normalized = decode_structured(&normalized_encoding)
            .map_err(|failure| encoding_corruption("normalized value", failure))?;
        raws.push(StoredRawProjection {
            position,
            hash,
            conversion_digest,
            expected,
            normalized,
        });
        declared_entries_by_raw.insert(position, declared_entry_count);
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
    let mut actual_entries_by_raw = BTreeMap::<ImportedRawRecordPosition, u64>::new();
    for row in entry_rows {
        let position = decode_entry_position(row.try_get("imported_entry_position")?)?;
        let identity =
            ImportedTranscriptEntryId::from_uuid(row.try_get("imported_transcript_entry_id")?);
        let raw_position = decode_raw_position(row.try_get("raw_record_position")?)?;
        let actual_count = actual_entries_by_raw.entry(raw_position).or_default();
        *actual_count = actual_count
            .checked_add(1)
            .ok_or_else(|| invalid_ordinal("raw-record actual entry count"))?;
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
    for (position, declared) in declared_entries_by_raw {
        let actual = actual_entries_by_raw.get(&position).copied().unwrap_or(0);
        if actual != declared {
            return Err(
                ImportedConversationCorruption::RawRecordDeclaredEntryCountMismatch {
                    position,
                    declared,
                    actual,
                }
                .into(),
            );
        }
    }

    Ok(StoredConversationProjection {
        requested,
        stored,
        format,
        source_digest,
        source_session_id,
        display_title,
        display_title_state,
        declared_raw_record_count,
        raws,
        declared_entry_count,
        entries,
    })
}

fn total_expected_bytes(
    blobs: impl IntoIterator<Item = ExpectedBlob>,
) -> Result<u64, ImportedRawBlobStorageError> {
    blobs.into_iter().try_fold(0_u64, |total, blob| {
        total
            .checked_add(blob.byte_length())
            .ok_or(ImportedRawBlobStorageError::Integrity)
    })
}

fn distinct_expected_blobs(
    blobs: impl IntoIterator<Item = ExpectedBlob>,
) -> Result<BTreeMap<BlobDigest, ExpectedBlob>, ImportedConversationRepositoryError> {
    let mut expected_by_digest = BTreeMap::new();
    for expected in blobs {
        match expected_by_digest.entry(expected.digest()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(expected);
            }
            std::collections::btree_map::Entry::Occupied(entry) if *entry.get() == expected => {}
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(ImportedConversationCorruption::RawRecordHashCollision.into());
            }
        }
    }
    Ok(expected_by_digest)
}

pub(crate) async fn finish_projection(
    storage: &dyn ImportedRawBlobStorage,
    projection: StoredConversationProjection,
) -> Result<ImportedConversation, ImportedConversationRepositoryError> {
    let total_source_bytes = total_expected_bytes(projection.raws.iter().map(|raw| raw.expected))?;
    let expected_by_digest =
        distinct_expected_blobs(projection.raws.iter().map(|raw| raw.expected))?;
    let expected = expected_by_digest.values().copied().collect::<Box<[_]>>();
    let distinct_bytes = storage.read(expected, total_source_bytes).await?;
    if distinct_bytes.len() != expected_by_digest.len() {
        return Err(ImportedConversationCorruption::Missing("raw blob bytes").into());
    }
    let bytes_by_digest = expected_by_digest
        .into_keys()
        .zip(distinct_bytes)
        .collect::<BTreeMap<_, _>>();
    let raws = projection
        .raws
        .into_iter()
        .map(|raw| {
            let stored_bytes = bytes_by_digest
                .get(&raw.expected.digest())
                .ok_or(ImportedConversationCorruption::Missing("raw blob bytes"))?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(stored_bytes.len())
                .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
            bytes.extend_from_slice(stored_bytes);
            Ok(ImportedRawSourceRecordReconstitutionInput::new(
                raw.position,
                raw.hash,
                raw.conversion_digest,
                bytes,
                raw.normalized,
            ))
        })
        .collect::<Result<Vec<_>, ImportedConversationRepositoryError>>()?;
    let conversation = ImportedConversationReconstitutionInput::new(
        projection.requested,
        projection.stored,
        projection.format,
        projection.source_digest,
        projection.declared_raw_record_count,
        raws,
        projection.declared_entry_count,
        projection.entries,
    )
    .reconstitute()
    .map_err(|error| ImportedConversationCorruption::Domain(error.failure()))?;
    if let Some(source_session_id) = projection.source_session_id
        && Some(source_session_id.as_slice()) != consistent_source_session_id(&conversation)
    {
        return Err(ImportedConversationCorruption::SourceSessionLineageMismatch.into());
    }
    validate_display_title(
        &conversation,
        projection.display_title,
        &projection.display_title_state,
    )?;
    Ok(conversation)
}

/// Requires a resolved stored display title to agree exactly with pure
/// re-derivation from the reconstituted records.
///
fn validate_display_title(
    conversation: &ImportedConversation,
    display_title: Option<String>,
    display_title_state: &str,
) -> Result<(), ImportedConversationRepositoryError> {
    let derived = ImportedConversationDisplayTitle::derive(conversation);
    match display_title_state {
        DISPLAY_TITLE_STATE_DERIVED => {
            let stored = display_title
                .ok_or(ImportedConversationCorruption::Missing("display title"))
                .and_then(|stored| {
                    ImportedConversationDisplayTitle::try_new(stored)
                        .map_err(|_| ImportedConversationCorruption::DisplayTitleMismatch)
                })?;
            if Some(stored) != derived {
                return Err(ImportedConversationCorruption::DisplayTitleMismatch.into());
            }
            Ok(())
        }
        DISPLAY_TITLE_STATE_UNDERIVABLE => {
            if derived.is_some() || display_title.is_some() {
                return Err(ImportedConversationCorruption::DisplayTitleMismatch.into());
            }
            Ok(())
        }
        other => Err(ImportedConversationCorruption::Unsupported {
            field: "display_title_state",
            value: String::from(other),
        }
        .into()),
    }
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

pub(crate) fn encode_format(format: ImportedConversationFormat) -> (&'static str, i16) {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_ONE)
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
            (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_TWO)
        }
        ImportedConversationFormat::CodexRolloutJsonlV1 => (CODEX_FORMAT, CODEX_VERSION_ONE),
    }
}

pub(crate) fn decode_format(
    format: &str,
    converter_version: i16,
) -> Result<ImportedConversationFormat, ImportedConversationRepositoryError> {
    match (format, converter_version) {
        (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_ONE) => {
            Ok(ImportedConversationFormat::ClaudeCodeSessionJsonlV1)
        }
        (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_TWO) => {
            Ok(ImportedConversationFormat::ClaudeCodeSessionJsonlV2)
        }
        (CODEX_FORMAT, CODEX_VERSION_ONE) => Ok(ImportedConversationFormat::CodexRolloutJsonlV1),
        (_, version) if format == CLAUDE_CODE_FORMAT => {
            Err(ImportedConversationCorruption::Unsupported {
                field: "converter version",
                value: version.to_string(),
            }
            .into())
        }
        (_, version) if format == CODEX_FORMAT => {
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

pub(crate) fn decode_source_speaker(
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

pub(crate) fn positive_u64(value: Decimal) -> Result<u64, PositiveOrdinalMappingError> {
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
            CodecFailure::UnexpectedPayloadKind { expected, actual } => {
                Self::UnexpectedPayloadKind { expected, actual }
            }
            CodecFailure::UnsupportedTag { kind, value } => Self::UnsupportedTag { kind, value },
            CodecFailure::InvalidUtf8(kind) => Self::InvalidUtf8(kind),
            CodecFailure::InvalidJsonNumber => Self::InvalidJsonNumber,
            CodecFailure::ContainerDepthExceeded => Self::ContainerDepthExceeded,
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::types::Uuid;

    use super::{
        BlobDigest, CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_ONE, CLAUDE_CODE_VERSION_TWO,
        CODEX_FORMAT, CODEX_VERSION_ONE, EncodedEntry, EncodedRawRecord, ExpectedBlob,
        ImportedConversationFormat, ImportedRawRecordConversionDigest, ImportedRawRecordHash,
        ImportedRawRecordPosition, ImportedRecordEntryPosition, ImportedTranscriptEntryId,
        ImportedTranscriptPosition, decode_format, distinct_expected_blobs, encode_format,
        entries_in_key_order, raw_blobs_in_key_order,
    };

    fn encoded_raw(key: u8) -> EncodedRawRecord {
        EncodedRawRecord {
            content_hash: ImportedRawRecordHash::from_bytes([key; 32]),
            conversion_digest: ImportedRawRecordConversionDigest::from_bytes([0; 32]),
            bytes: std::sync::Arc::from([key]),
            normalized: vec![key],
            declared_entry_count: 1,
        }
    }

    fn encoded_entry(key: u128) -> EncodedEntry {
        EncodedEntry {
            identity: ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(key)),
            position: ImportedTranscriptPosition::first(),
            raw_position: ImportedRawRecordPosition::first(),
            within_position: ImportedRecordEntryPosition::first(),
            source_speaker: "not_attested",
            content: vec![0],
            source: vec![0],
        }
    }

    #[test]
    fn s28_claude_code_converter_versions_have_distinct_storage_mappings() {
        assert_eq!(
            encode_format(ImportedConversationFormat::ClaudeCodeSessionJsonlV1),
            (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_ONE)
        );
        assert_eq!(
            encode_format(ImportedConversationFormat::ClaudeCodeSessionJsonlV2),
            (CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_TWO)
        );
        assert_eq!(
            decode_format(CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_ONE)
                .expect("version one remains readable"),
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1
        );
        assert_eq!(
            decode_format(CLAUDE_CODE_FORMAT, CLAUDE_CODE_VERSION_TWO)
                .expect("version two remains readable"),
            ImportedConversationFormat::ClaudeCodeSessionJsonlV2
        );
    }

    #[test]
    fn s28_codex_rollout_converter_has_distinct_storage_mapping() {
        assert_eq!(
            encode_format(ImportedConversationFormat::CodexRolloutJsonlV1),
            (CODEX_FORMAT, CODEX_VERSION_ONE)
        );
        assert_eq!(
            decode_format(CODEX_FORMAT, CODEX_VERSION_ONE)
                .expect("Codex rollout version one remains readable"),
            ImportedConversationFormat::CodexRolloutJsonlV1
        );
    }

    /// S28: shared raw-blob keys are emitted in one deterministic
    /// acquisition order independent of physical transcript order.
    #[test]
    fn s28_raw_blob_acquisition_is_content_hash_ordered() {
        let larger = encoded_raw(2);
        let smaller = encoded_raw(1);
        let raws = [larger, smaller];

        let ordered = raw_blobs_in_key_order(&raws);

        assert_eq!(
            ordered[0].content_hash,
            ImportedRawRecordHash::from_bytes([1; 32])
        );
        assert_eq!(
            ordered[1].content_hash,
            ImportedRawRecordHash::from_bytes([2; 32])
        );
    }

    #[test]
    fn imported_blob_read_plan_deduplicates_equal_occurrences() {
        let expected = ExpectedBlob::try_new(BlobDigest::from_bytes([1; 32]), 1)
            .expect("fixture length is positive");

        let distinct = distinct_expected_blobs([expected, expected])
            .expect("equal immutable identities agree");

        assert_eq!(distinct.len(), 1);
        assert_eq!(distinct.get(&expected.digest()), Some(&expected));
    }

    #[test]
    fn imported_blob_source_size_counts_equal_occurrences() {
        let expected = ExpectedBlob::try_new(BlobDigest::from_bytes([1; 32]), 3)
            .expect("fixture length is positive");

        assert_eq!(super::total_expected_bytes([expected, expected]), Ok(6));
    }

    /// S28: globally unique entry keys are emitted in one
    /// deterministic acquisition order independent of transcript order.
    #[test]
    fn s28_entry_acquisition_is_identity_ordered() {
        let larger = encoded_entry(2);
        let smaller = encoded_entry(1);
        let entries = [larger, smaller];

        let ordered = entries_in_key_order(&entries);

        assert_eq!(
            ordered[0].identity,
            ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(1))
        );
        assert_eq!(
            ordered[1].identity,
            ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(2))
        );
    }
}
