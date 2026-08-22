//! PostgreSQL corpus storage governed by the evaluation-system specification.

use std::path::Path;

use sha2::{Digest, Sha256};
use signalbox_persistence::mapping::{
    EvaluationCorpusSourceStorageKind, evaluation_corpus_source_from_str,
    evaluation_corpus_source_to_str,
};
use sqlx::{PgPool, Row};

use crate::{
    ApprovalJudgeCorpus, CORPUS_FORMAT_VERSION,
    manifest::{ManifestError, corpus_digest, load_manifest_corpus},
    store::{
        CorpusKey, CorpusRegistration, CorpusSourceDescriptor, CorpusStore, CorpusStoreCorruption,
        CorpusStoreError, CorpusStoreFuture, Sha256Digest,
    },
};

/// PostgreSQL-backed corpus registrations and ordered evaluation cases.
#[derive(Clone, Debug)]
pub struct DatabaseCorpusStore {
    pool: PgPool,
}

impl DatabaseCorpusStore {
    /// Binds the store to one Signalbox instance database.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports and verifies a repository or database-native portable manifest.
    pub async fn import_manifest(
        &self,
        manifest_path: impl AsRef<Path>,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        let loaded = load_manifest_corpus(manifest_path).map_err(CorpusStoreError::Manifest)?;
        let source_sha256 = loaded.manifest.integrity().source_sha256;
        self.put_verified(loaded.registration, &loaded.corpus, source_sha256)
            .await
    }

    /// Stores database-native cases and their registration metadata atomically.
    pub async fn put(
        &self,
        registration: CorpusRegistration,
        corpus: &ApprovalJudgeCorpus,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        match registration.source() {
            CorpusSourceDescriptor::DatabaseNative => {}
            CorpusSourceDescriptor::Repository { .. } => {
                return Err(CorpusStoreError::RepositorySourceRequiresManifestImport);
            }
            CorpusSourceDescriptor::BlobReference { .. } => {
                return Err(CorpusStoreError::BlobBackendUnavailable);
            }
        }
        self.put_verified(registration, corpus, None).await
    }

    async fn put_verified(
        &self,
        registration: CorpusRegistration,
        corpus: &ApprovalJudgeCorpus,
        source_sha256: Option<Sha256Digest>,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        validate_registration(&registration, corpus)?;
        let replay_sha256 = replay_digest(corpus)?;
        let case_count = i64::try_from(registration.case_count()).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CaseCountOutOfRange)
        })?;
        let format_version = i32::try_from(registration.format_version()).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::FormatVersionOutOfRange)
        })?;
        let source = encode_source(registration.source());
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO evaluation_corpus (
                corpus_name, corpus_version, format_version, corpus_digest, replay_digest,
                case_count, source_kind, source_repository, source_path, source_sha256,
                source_blob_store, source_blob_digest, source_blob_byte_length
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::numeric)
             ON CONFLICT (corpus_name, corpus_version) DO NOTHING",
        )
        .bind(&registration.key().name)
        .bind(&registration.key().version)
        .bind(format_version)
        .bind(registration.corpus_sha256().as_bytes().as_slice())
        .bind(replay_sha256.as_bytes().as_slice())
        .bind(case_count)
        .bind(source.kind)
        .bind(source.repository)
        .bind(source.path)
        .bind(source_sha256.map(|digest| digest.as_bytes().to_vec()))
        .bind(source.blob_store)
        .bind(source.blob_digest)
        .bind(source.blob_byte_length)
        .execute(&mut *transaction)
        .await?
        .rows_affected();

        if inserted == 0 {
            transaction.rollback().await?;
            if self.source_sha256(registration.key()).await? != source_sha256 {
                return Err(CorpusStoreError::CorruptRegistration(
                    CorpusStoreCorruption::RegistrationConflict,
                ));
            }
            let existing = self.registration(registration.key()).await?;
            if existing == registration {
                let loaded = self.load_owned(registration.key()).await?;
                if loaded == *corpus {
                    return Ok(existing);
                }
            }
            return Err(CorpusStoreError::CorruptRegistration(
                CorpusStoreCorruption::RegistrationConflict,
            ));
        }

        for (position, case) in corpus.cases.iter().enumerate() {
            let position = i64::try_from(position).map_err(|_| {
                CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CasePositionOutOfRange)
            })?;
            let case_json =
                serde_json::to_string(case).map_err(CorpusStoreError::StoredCaseJson)?;
            sqlx::query(
                "INSERT INTO evaluation_corpus_case (
                    corpus_name, corpus_version, case_id, replay_position, case_json
                 ) VALUES ($1, $2, $3, $4, $5::jsonb)",
            )
            .bind(&registration.key().name)
            .bind(&registration.key().version)
            .bind(&case.id)
            .bind(position)
            .bind(case_json)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(registration)
    }

    async fn source_sha256(
        &self,
        key: &CorpusKey,
    ) -> Result<Option<Sha256Digest>, CorpusStoreError> {
        let bytes: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT source_sha256
               FROM evaluation_corpus
              WHERE corpus_name = $1 AND corpus_version = $2",
        )
        .bind(&key.name)
        .bind(&key.version)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        bytes.as_deref().map(decode_digest).transpose()
    }

    /// Returns one registration or a typed not-found result.
    pub async fn registration(
        &self,
        key: &CorpusKey,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        let row = sqlx::query(
            "SELECT corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
                    source_kind, source_repository, source_path, source_blob_store,
                    source_blob_digest, source_blob_byte_length::text AS source_blob_byte_length
               FROM evaluation_corpus
              WHERE corpus_name = $1 AND corpus_version = $2",
        )
        .bind(&key.name)
        .bind(&key.version)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| CorpusStoreError::NotFound(key.clone()))?;
        let stored = decode_registration(&row)?;
        registration_metadata_from_stored(stored)
    }

    async fn enumerate_owned(&self) -> Result<Vec<CorpusRegistration>, CorpusStoreError> {
        let rows = sqlx::query(
            "SELECT corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
                    source_kind, source_repository, source_path, source_blob_store,
                    source_blob_digest, source_blob_byte_length::text AS source_blob_byte_length
               FROM evaluation_corpus
              ORDER BY corpus_name COLLATE \"C\", corpus_version COLLATE \"C\"",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut registrations = Vec::with_capacity(rows.len());
        for row in &rows {
            let stored = decode_registration(row)?;
            registrations.push(registration_metadata_from_stored(stored)?);
        }
        Ok(registrations)
    }

    async fn load_owned(&self, key: &CorpusKey) -> Result<ApprovalJudgeCorpus, CorpusStoreError> {
        let row = sqlx::query(
            "SELECT corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
                    source_kind, source_repository, source_path, source_blob_store,
                    source_blob_digest, source_blob_byte_length::text AS source_blob_byte_length
               FROM evaluation_corpus
              WHERE corpus_name = $1 AND corpus_version = $2",
        )
        .bind(&key.name)
        .bind(&key.version)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| CorpusStoreError::NotFound(key.clone()))?;
        let stored = decode_registration(&row)?;
        let corpus = self.load_stored_corpus(&stored).await?;
        registration_from_stored(stored, &corpus)?;
        Ok(corpus)
    }

    async fn load_stored_corpus(
        &self,
        registration: &StoredRegistration,
    ) -> Result<ApprovalJudgeCorpus, CorpusStoreError> {
        if matches!(
            &registration.source,
            CorpusSourceDescriptor::BlobReference { .. }
        ) {
            return Err(CorpusStoreError::BlobBackendUnavailable);
        }
        let rows = sqlx::query(
            "SELECT case_id, case_json::text AS case_json
               FROM evaluation_corpus_case
              WHERE corpus_name = $1 AND corpus_version = $2
              ORDER BY replay_position",
        )
        .bind(&registration.key.name)
        .bind(&registration.key.version)
        .fetch_all(&self.pool)
        .await?;
        let mut cases = Vec::with_capacity(rows.len());
        for row in rows {
            let stored_id: String = row.try_get("case_id")?;
            let json: String = row.try_get("case_json")?;
            let case: crate::ApprovalJudgeCase =
                serde_json::from_str(&json).map_err(CorpusStoreError::StoredCaseJson)?;
            if case.id != stored_id {
                return Err(CorpusStoreError::CorruptRegistration(
                    CorpusStoreCorruption::CaseIdMismatch,
                ));
            }
            cases.push(case);
        }
        Ok(ApprovalJudgeCorpus {
            format_version: registration.format_version,
            cases,
        })
    }
}

impl CorpusStore for DatabaseCorpusStore {
    fn enumerate(&self) -> CorpusStoreFuture<'_, Vec<CorpusRegistration>> {
        Box::pin(self.enumerate_owned())
    }

    fn load<'a>(&'a self, key: &'a CorpusKey) -> CorpusStoreFuture<'a, ApprovalJudgeCorpus> {
        Box::pin(self.load_owned(key))
    }
}

fn validate_registration(
    registration: &CorpusRegistration,
    corpus: &ApprovalJudgeCorpus,
) -> Result<(), CorpusStoreError> {
    let count = u64::try_from(corpus.cases.len()).map_err(|_| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CaseCountOutOfRange)
    })?;
    if registration.format_version() != corpus.format_version {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::FormatVersionMismatch,
        ));
    }
    if registration.format_version() != CORPUS_FORMAT_VERSION {
        return Err(CorpusStoreError::Manifest(
            ManifestError::UnsupportedCorpusVersion {
                observed: registration.format_version(),
            },
        ));
    }
    if registration.case_count() != count {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CaseCountMismatch,
        ));
    }
    let observed = corpus_digest(corpus).map_err(CorpusStoreError::Manifest)?;
    if registration.corpus_sha256() != observed {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CorpusDigestMismatch,
        ));
    }
    Ok(())
}

fn replay_digest(corpus: &ApprovalJudgeCorpus) -> Result<Sha256Digest, CorpusStoreError> {
    const REPLAY_DIGEST_DOMAIN: &[u8] = b"signalbox-eval-corpus-replay-v1\0";
    let count = u64::try_from(corpus.cases.len()).map_err(|_| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CaseCountOutOfRange)
    })?;
    let mut hasher = Sha256::new();
    hasher.update(REPLAY_DIGEST_DOMAIN);
    hasher.update(count.to_be_bytes());
    for case in &corpus.cases {
        let length = u64::try_from(case.id.len()).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CaseCountOutOfRange)
        })?;
        hasher.update(length.to_be_bytes());
        hasher.update(case.id.as_bytes());
    }
    Ok(Sha256Digest::from_bytes(hasher.finalize().into()))
}

struct EncodedSource<'a> {
    kind: &'static str,
    repository: Option<&'a str>,
    path: Option<&'a str>,
    blob_store: Option<&'a str>,
    blob_digest: Option<&'a [u8]>,
    blob_byte_length: Option<String>,
}

fn encode_source(source: &CorpusSourceDescriptor) -> EncodedSource<'_> {
    match source {
        CorpusSourceDescriptor::Repository { repository, path } => EncodedSource {
            kind: evaluation_corpus_source_to_str(EvaluationCorpusSourceStorageKind::Repository),
            repository: Some(repository),
            path: Some(path),
            blob_store: None,
            blob_digest: None,
            blob_byte_length: None,
        },
        CorpusSourceDescriptor::DatabaseNative => EncodedSource {
            kind: evaluation_corpus_source_to_str(
                EvaluationCorpusSourceStorageKind::DatabaseNative,
            ),
            repository: None,
            path: None,
            blob_store: None,
            blob_digest: None,
            blob_byte_length: None,
        },
        CorpusSourceDescriptor::BlobReference {
            store,
            digest,
            byte_length,
        } => EncodedSource {
            kind: evaluation_corpus_source_to_str(EvaluationCorpusSourceStorageKind::BlobReference),
            repository: None,
            path: None,
            blob_store: store.as_deref(),
            blob_digest: Some(digest.as_bytes()),
            blob_byte_length: Some(byte_length.to_string()),
        },
    }
}

struct StoredRegistration {
    key: CorpusKey,
    format_version: u32,
    corpus_sha256: Sha256Digest,
    replay_sha256: Sha256Digest,
    case_count: u64,
    source: CorpusSourceDescriptor,
}

fn decode_registration(
    row: &sqlx::postgres::PgRow,
) -> Result<StoredRegistration, CorpusStoreError> {
    let name: String = row.try_get("corpus_name")?;
    let version: String = row.try_get("corpus_version")?;
    let format_version: i32 = row.try_get("format_version")?;
    let corpus_digest: Vec<u8> = row.try_get("corpus_digest")?;
    let replay_digest: Vec<u8> = row.try_get("replay_digest")?;
    let case_count: i64 = row.try_get("case_count")?;
    let source_kind: String = row.try_get("source_kind")?;
    let source = match evaluation_corpus_source_from_str(&source_kind) {
        Some(EvaluationCorpusSourceStorageKind::Repository) => CorpusSourceDescriptor::Repository {
            repository: required_column(row, "source_repository")?,
            path: required_column(row, "source_path")?,
        },
        Some(EvaluationCorpusSourceStorageKind::DatabaseNative) => {
            CorpusSourceDescriptor::DatabaseNative
        }
        Some(EvaluationCorpusSourceStorageKind::BlobReference) => {
            let digest: Vec<u8> = required_column(row, "source_blob_digest")?;
            CorpusSourceDescriptor::BlobReference {
                store: row.try_get("source_blob_store")?,
                digest: decode_digest(&digest)?,
                byte_length: required_column::<String>(row, "source_blob_byte_length")?
                    .parse()
                    .map_err(|_| {
                        CorpusStoreError::CorruptRegistration(
                            CorpusStoreCorruption::InvalidBlobByteLength,
                        )
                    })?,
            }
        }
        None => {
            return Err(CorpusStoreError::CorruptRegistration(
                CorpusStoreCorruption::UnknownSourceKind(source_kind),
            ));
        }
    };
    let format_version = u32::try_from(format_version).map_err(|_| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::NegativeFormatVersion)
    })?;
    let case_count = u64::try_from(case_count).map_err(|_| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::NegativeCaseCount)
    })?;
    Ok(StoredRegistration {
        key: CorpusKey { name, version },
        format_version,
        corpus_sha256: decode_digest(&corpus_digest)?,
        replay_sha256: decode_digest(&replay_digest)?,
        case_count,
        source,
    })
}

fn registration_from_stored(
    stored: StoredRegistration,
    corpus: &ApprovalJudgeCorpus,
) -> Result<CorpusRegistration, CorpusStoreError> {
    if stored.format_version != corpus.format_version {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::FormatVersionMismatch,
        ));
    }
    let registration = CorpusRegistration::new(stored.key, stored.source, corpus)
        .map_err(CorpusStoreError::CorruptStoredAdmission)?;
    if registration.case_count() != stored.case_count {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CaseCountMismatch,
        ));
    }
    if registration.corpus_sha256() != stored.corpus_sha256 {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CorpusDigestMismatch,
        ));
    }
    if replay_digest(corpus)? != stored.replay_sha256 {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::ReplayDigestMismatch,
        ));
    }
    Ok(registration)
}

fn registration_metadata_from_stored(
    stored: StoredRegistration,
) -> Result<CorpusRegistration, CorpusStoreError> {
    CorpusRegistration::from_stored_metadata(
        stored.key,
        stored.format_version,
        stored.corpus_sha256,
        stored.case_count,
        stored.source,
    )
    .map_err(CorpusStoreError::CorruptStoredAdmission)
}

fn required_column<Value>(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
) -> Result<Value, CorpusStoreError>
where
    for<'row> Value: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<Value>, _>(column)?.ok_or_else(|| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::MissingSourceField(column))
    })
}

fn decode_digest(bytes: &[u8]) -> Result<Sha256Digest, CorpusStoreError> {
    let array: [u8; 32] = bytes.try_into().map_err(|_| {
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::InvalidDigestLength)
    })?;
    Ok(Sha256Digest::from_bytes(array))
}
