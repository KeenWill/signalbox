use std::path::Path;

use sqlx::{PgPool, Row};

use crate::{
    ApprovalJudgeCorpus, CORPUS_FORMAT_VERSION,
    manifest::{ManifestError, corpus_digest, load_manifest_corpus},
    store::{
        CorpusKey, CorpusRegistration, CorpusSourceDescriptor, CorpusStore, CorpusStoreCorruption,
        CorpusStoreError, CorpusStoreFuture, Sha256Digest,
    },
};

const SOURCE_REPOSITORY: &str = "repository";
const SOURCE_DATABASE_NATIVE: &str = "database_native";
const SOURCE_BLOB_REFERENCE: &str = "blob_reference";

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
        self.put(loaded.registration, &loaded.corpus).await
    }

    /// Stores verified cases and their registration metadata atomically.
    pub async fn put(
        &self,
        registration: CorpusRegistration,
        corpus: &ApprovalJudgeCorpus,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        validate_registration(&registration, corpus)?;
        let case_count = i64::try_from(registration.case_count).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::CaseCountOutOfRange)
        })?;
        let format_version = i32::try_from(registration.format_version).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::FormatVersionOutOfRange)
        })?;
        let (source_kind, repository, path, blob_store, blob_digest, blob_byte_length) =
            encode_source(&registration.source);
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO evaluation_corpus (
                corpus_name, corpus_version, format_version, corpus_digest, case_count,
                source_kind, source_repository, source_path, source_blob_store,
                source_blob_digest, source_blob_byte_length
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::numeric)
             ON CONFLICT (corpus_name, corpus_version) DO NOTHING",
        )
        .bind(&registration.key.name)
        .bind(&registration.key.version)
        .bind(format_version)
        .bind(registration.corpus_sha256.as_bytes().as_slice())
        .bind(case_count)
        .bind(source_kind)
        .bind(repository)
        .bind(path)
        .bind(blob_store)
        .bind(blob_digest)
        .bind(blob_byte_length)
        .execute(&mut *transaction)
        .await?
        .rows_affected();

        if inserted == 0 {
            transaction.rollback().await?;
            let existing = self.registration(&registration.key).await?;
            if existing == registration {
                let loaded = self.load_owned(&registration.key).await?;
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
            let case_json = serde_json::to_string(case).map_err(|_| {
                CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::StoredCaseJson)
            })?;
            sqlx::query(
                "INSERT INTO evaluation_corpus_case (
                    corpus_name, corpus_version, case_id, replay_position, case_json
                 ) VALUES ($1, $2, $3, $4, $5::jsonb)",
            )
            .bind(&registration.key.name)
            .bind(&registration.key.version)
            .bind(&case.id)
            .bind(position)
            .bind(case_json)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(registration)
    }

    /// Returns one registration or a typed not-found result.
    pub async fn registration(
        &self,
        key: &CorpusKey,
    ) -> Result<CorpusRegistration, CorpusStoreError> {
        let row = sqlx::query(
            "SELECT corpus_name, corpus_version, format_version, corpus_digest, case_count,
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
        decode_registration(&row)
    }

    async fn enumerate_owned(&self) -> Result<Vec<CorpusRegistration>, CorpusStoreError> {
        let rows = sqlx::query(
            "SELECT corpus_name, corpus_version, format_version, corpus_digest, case_count,
                    source_kind, source_repository, source_path, source_blob_store,
                    source_blob_digest, source_blob_byte_length::text AS source_blob_byte_length
               FROM evaluation_corpus
              ORDER BY corpus_name COLLATE \"C\", corpus_version COLLATE \"C\"",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_registration).collect()
    }

    async fn load_owned(&self, key: &CorpusKey) -> Result<ApprovalJudgeCorpus, CorpusStoreError> {
        let registration = self.registration(key).await?;
        let rows = sqlx::query(
            "SELECT case_json::text AS case_json
               FROM evaluation_corpus_case
              WHERE corpus_name = $1 AND corpus_version = $2
              ORDER BY replay_position",
        )
        .bind(&key.name)
        .bind(&key.version)
        .fetch_all(&self.pool)
        .await?;
        let mut cases = Vec::with_capacity(rows.len());
        for row in rows {
            let json: String = row.try_get("case_json")?;
            let case = serde_json::from_str(&json).map_err(|_| {
                CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::StoredCaseJson)
            })?;
            cases.push(case);
        }
        let corpus = ApprovalJudgeCorpus {
            format_version: registration.format_version,
            cases,
        };
        validate_registration(&registration, &corpus)?;
        Ok(corpus)
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
    if registration.format_version != corpus.format_version {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::FormatVersionMismatch,
        ));
    }
    if registration.format_version != CORPUS_FORMAT_VERSION {
        return Err(CorpusStoreError::Manifest(
            ManifestError::UnsupportedCorpusVersion {
                observed: registration.format_version,
            },
        ));
    }
    if registration.case_count != count {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CaseCountMismatch,
        ));
    }
    let observed = corpus_digest(corpus).map_err(CorpusStoreError::Manifest)?;
    if registration.corpus_sha256 != observed {
        return Err(CorpusStoreError::CorruptRegistration(
            CorpusStoreCorruption::CorpusDigestMismatch,
        ));
    }
    Ok(())
}

type EncodedSource<'a> = (
    &'static str,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a [u8]>,
    Option<String>,
);

fn encode_source(source: &CorpusSourceDescriptor) -> EncodedSource<'_> {
    match source {
        CorpusSourceDescriptor::Repository { repository, path } => (
            SOURCE_REPOSITORY,
            Some(repository),
            Some(path),
            None,
            None,
            None,
        ),
        CorpusSourceDescriptor::DatabaseNative => {
            (SOURCE_DATABASE_NATIVE, None, None, None, None, None)
        }
        CorpusSourceDescriptor::BlobReference {
            store,
            digest,
            byte_length,
        } => (
            SOURCE_BLOB_REFERENCE,
            None,
            None,
            store.as_deref(),
            Some(digest.as_bytes()),
            Some(byte_length.to_string()),
        ),
    }
}

fn decode_registration(
    row: &sqlx::postgres::PgRow,
) -> Result<CorpusRegistration, CorpusStoreError> {
    let name: String = row.try_get("corpus_name")?;
    let version: String = row.try_get("corpus_version")?;
    let format_version: i32 = row.try_get("format_version")?;
    let corpus_digest: Vec<u8> = row.try_get("corpus_digest")?;
    let case_count: i64 = row.try_get("case_count")?;
    let source_kind: String = row.try_get("source_kind")?;
    let source = match source_kind.as_str() {
        SOURCE_REPOSITORY => CorpusSourceDescriptor::Repository {
            repository: required_column(row, "source_repository")?,
            path: required_column(row, "source_path")?,
        },
        SOURCE_DATABASE_NATIVE => CorpusSourceDescriptor::DatabaseNative,
        SOURCE_BLOB_REFERENCE => {
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
        other => {
            return Err(CorpusStoreError::CorruptRegistration(
                CorpusStoreCorruption::UnknownSourceKind(other.to_owned()),
            ));
        }
    };
    Ok(CorpusRegistration {
        key: CorpusKey { name, version },
        format_version: u32::try_from(format_version).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::NegativeFormatVersion)
        })?,
        corpus_sha256: decode_digest(&corpus_digest)?,
        case_count: u64::try_from(case_count).map_err(|_| {
            CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::NegativeCaseCount)
        })?,
        source,
    })
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
