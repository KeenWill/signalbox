//! PostgreSQL adapter for immutable blob derivation provenance.

use std::{error::Error, fmt};

use signalbox_application::{BlobDerivationRecordOutcome, BlobDerivationStore};
use signalbox_domain::{
    BlobDerivation, BlobDerivationId, BlobDerivationProducer, BlobDigest, BlobTransformation,
    BlobTransformationName, DeterministicBlobDerivationKey, ModelCallId,
};
use sqlx::{
    PgPool, Postgres, Row, Transaction,
    postgres::{PgConnection, PgRow},
};

use crate::commit_failure_is_ambiguous;

const LOAD_ROOT_BY_KEY: &str = r#"SELECT derivation_id, deterministic_key,
           transformation_name, transformation_version, parameters_json, parameters_canonical,
           producer_class, implementation_digest, execution_id, model_call_id,
           input_count, output_count
      FROM blob_derivation
     WHERE deterministic_key = $1"#;

/// Durable derivation facts disagreed with the domain algebra.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobDerivationCorruption {
    InvalidIdentity,
    InvalidDigest,
    InvalidTransformation,
    InvalidProducer,
    InvalidCardinality,
    InvalidOrdinal,
    DeterministicKeyMismatch,
    IdentityCollision,
}

impl fmt::Display for BlobDerivationCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "stored blob derivation identity is invalid",
            Self::InvalidDigest => "stored blob derivation digest is invalid",
            Self::InvalidTransformation => "stored blob transformation is invalid",
            Self::InvalidProducer => "stored blob derivation producer is invalid",
            Self::InvalidCardinality => "stored blob derivation cardinality is invalid",
            Self::InvalidOrdinal => "stored blob derivation ordinal is invalid",
            Self::DeterministicKeyMismatch => "stored blob derivation cache key is invalid",
            Self::IdentityCollision => "blob derivation identity already exists",
        })
    }
}

impl Error for BlobDerivationCorruption {}

/// PostgreSQL unavailability, commit ambiguity, or durable corruption.
#[derive(Debug)]
pub enum BlobDerivationRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(BlobDerivationCorruption),
}

impl fmt::Display for BlobDerivationRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "blob derivation database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(formatter, "blob derivation commit is ambiguous: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for BlobDerivationRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for BlobDerivationRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<BlobDerivationCorruption> for BlobDerivationRepositoryError {
    fn from(error: BlobDerivationCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// Append-only PostgreSQL derivation store.
#[derive(Clone, Debug)]
pub struct BlobDerivationRepository {
    pool: PgPool,
}

impl BlobDerivationRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn find_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
    ) -> Result<Option<BlobDerivation>, BlobDerivationRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        let root = sqlx::query(LOAD_ROOT_BY_KEY)
            .bind(key.digest().as_bytes().as_slice())
            .fetch_optional(&mut *connection)
            .await?;
        let Some(root) = root else {
            return Ok(None);
        };
        load_satellites(&mut connection, root).await.map(Some)
    }

    /// Appends any admitted producer class, returning a deterministic race winner.
    pub async fn record(
        &self,
        derivation: BlobDerivation,
    ) -> Result<BlobDerivationRecordOutcome, BlobDerivationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let deterministic_key = derivation.deterministic_key();
        let (producer_class, implementation, execution_id, model_call_id) =
            encode_producer(derivation.producer());
        let input_count = i16::try_from(derivation.inputs().len())
            .map_err(|_| BlobDerivationCorruption::InvalidCardinality)?;
        let output_count = i16::try_from(derivation.outputs().len())
            .map_err(|_| BlobDerivationCorruption::InvalidCardinality)?;
        let inserted = sqlx::query(
            r#"INSERT INTO blob_derivation (
                   derivation_id, deterministic_key, transformation_name,
                   transformation_version, parameters_json, parameters_canonical, producer_class,
                   implementation_digest, execution_id, model_call_id,
                   input_count, output_count
               ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(derivation.id().into_uuid())
        .bind(deterministic_key.map(|key| key.digest().as_bytes().to_vec()))
        .bind(derivation.transformation().name().as_str())
        .bind(i64::from(derivation.transformation().version().get()))
        .bind(
            serde_json::from_str::<serde_json::Value>(
                derivation.transformation().parameters_json(),
            )
            .map_err(|_| BlobDerivationCorruption::InvalidTransformation)?,
        )
        .bind(derivation.transformation().parameters_json())
        .bind(producer_class)
        .bind(implementation.map(|digest| digest.as_bytes().to_vec()))
        .bind(execution_id)
        .bind(model_call_id)
        .bind(input_count)
        .bind(output_count)
        .execute(&mut *transaction)
        .await?
        .rows_affected();

        if inserted == 0 {
            let key = deterministic_key.ok_or(BlobDerivationCorruption::IdentityCollision)?;
            let existing = load_by_key_in_transaction(&mut transaction, key)
                .await?
                .ok_or(BlobDerivationCorruption::DeterministicKeyMismatch)?;
            transaction.rollback().await?;
            return Ok(BlobDerivationRecordOutcome::Existing(existing));
        }

        insert_digests(
            &mut transaction,
            "INSERT INTO blob_derivation_input (derivation_id, input_ordinal, digest) VALUES ($1, $2, $3)",
            derivation.id(),
            derivation.inputs(),
        )
        .await?;
        insert_digests(
            &mut transaction,
            "INSERT INTO blob_derivation_output (derivation_id, output_ordinal, digest) VALUES ($1, $2, $3)",
            derivation.id(),
            derivation.outputs(),
        )
        .await?;
        match transaction.commit().await {
            Ok(()) => Ok(BlobDerivationRecordOutcome::Recorded(derivation)),
            Err(error) if commit_failure_is_ambiguous(&error) => {
                Err(BlobDerivationRepositoryError::CommitAmbiguous(error))
            }
            Err(error) => Err(BlobDerivationRepositoryError::Database(error)),
        }
    }
}

impl BlobDerivationStore for BlobDerivationRepository {
    type Error = BlobDerivationRepositoryError;

    async fn find_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
    ) -> Result<Option<BlobDerivation>, Self::Error> {
        Self::find_deterministic(self, key).await
    }

    async fn record_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
        derivation: BlobDerivation,
    ) -> Result<BlobDerivationRecordOutcome, Self::Error> {
        if derivation.deterministic_key() != Some(key) {
            return Err(BlobDerivationCorruption::DeterministicKeyMismatch.into());
        }
        self.record(derivation).await
    }
}

fn encode_producer(
    producer: BlobDerivationProducer,
) -> (
    &'static str,
    Option<BlobDigest>,
    Option<uuid::Uuid>,
    Option<uuid::Uuid>,
) {
    match producer {
        BlobDerivationProducer::Deterministic { implementation } => {
            ("deterministic", Some(implementation), None, None)
        }
        BlobDerivationProducer::Executed {
            execution_id,
            implementation,
        } => ("executed", Some(implementation), Some(execution_id), None),
        BlobDerivationProducer::ModelDerived { model_call } => {
            ("model_derived", None, None, Some(model_call.into_uuid()))
        }
    }
}

async fn insert_digests(
    transaction: &mut Transaction<'_, Postgres>,
    statement: &'static str,
    id: BlobDerivationId,
    digests: &[BlobDigest],
) -> Result<(), BlobDerivationRepositoryError> {
    for (ordinal, digest) in digests.iter().enumerate() {
        let ordinal =
            i16::try_from(ordinal).map_err(|_| BlobDerivationCorruption::InvalidOrdinal)?;
        sqlx::query(statement)
            .bind(id.into_uuid())
            .bind(ordinal)
            .bind(digest.as_bytes().as_slice())
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn load_by_key_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    key: DeterministicBlobDerivationKey,
) -> Result<Option<BlobDerivation>, BlobDerivationRepositoryError> {
    let root = sqlx::query(LOAD_ROOT_BY_KEY)
        .bind(key.digest().as_bytes().as_slice())
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(root) = root else {
        return Ok(None);
    };
    load_satellites(transaction, root).await.map(Some)
}

async fn load_satellites(
    connection: &mut PgConnection,
    root: PgRow,
) -> Result<BlobDerivation, BlobDerivationRepositoryError> {
    let id = BlobDerivationId::from_uuid(root.try_get("derivation_id")?);
    let inputs = load_digests(
        connection,
        "SELECT input_ordinal, digest FROM blob_derivation_input WHERE derivation_id = $1 ORDER BY input_ordinal",
        "input_ordinal",
        id,
    )
    .await?;
    let outputs = load_digests(
        connection,
        "SELECT output_ordinal, digest FROM blob_derivation_output WHERE derivation_id = $1 ORDER BY output_ordinal",
        "output_ordinal",
        id,
    )
    .await?;
    let expected_inputs: i16 = root.try_get("input_count")?;
    let expected_outputs: i16 = root.try_get("output_count")?;
    if usize::try_from(expected_inputs).ok() != Some(inputs.len())
        || usize::try_from(expected_outputs).ok() != Some(outputs.len())
    {
        return Err(BlobDerivationCorruption::InvalidCardinality.into());
    }
    let version = u32::try_from(root.try_get::<i64, _>("transformation_version")?)
        .map_err(|_| BlobDerivationCorruption::InvalidTransformation)?;
    let parameters_canonical: String = root.try_get("parameters_canonical")?;
    let parameters: serde_json::Value = serde_json::from_str(&parameters_canonical)
        .map_err(|_| BlobDerivationCorruption::InvalidTransformation)?;
    let transformation = BlobTransformation::try_new(
        BlobTransformationName::try_new(root.try_get::<String, _>("transformation_name")?)
            .map_err(|_| BlobDerivationCorruption::InvalidTransformation)?,
        version,
        &parameters,
    )
    .map_err(|_| BlobDerivationCorruption::InvalidTransformation)?;
    if transformation.parameters_json() != parameters_canonical {
        return Err(BlobDerivationCorruption::InvalidTransformation.into());
    }
    let producer = decode_producer(&root)?;
    let derivation = BlobDerivation::try_new(id, inputs, transformation, producer, outputs)
        .map_err(|_| BlobDerivationCorruption::InvalidCardinality)?;
    let stored_key_digest = root
        .try_get::<Option<Vec<u8>>, _>("deterministic_key")?
        .map(decode_digest)
        .transpose()?;
    if stored_key_digest != derivation.deterministic_key().map(|key| key.digest()) {
        return Err(BlobDerivationCorruption::DeterministicKeyMismatch.into());
    }
    Ok(derivation)
}

async fn load_digests(
    connection: &mut PgConnection,
    statement: &'static str,
    ordinal_column: &'static str,
    id: BlobDerivationId,
) -> Result<Box<[BlobDigest]>, BlobDerivationRepositoryError> {
    let rows = sqlx::query(statement)
        .bind(id.into_uuid())
        .fetch_all(&mut *connection)
        .await?;
    let mut digests = Vec::with_capacity(rows.len());
    for (expected, row) in rows.into_iter().enumerate() {
        let ordinal: i16 = row.try_get(ordinal_column)?;
        if usize::try_from(ordinal).ok() != Some(expected) {
            return Err(BlobDerivationCorruption::InvalidOrdinal.into());
        }
        digests.push(decode_digest(row.try_get("digest")?)?);
    }
    Ok(digests.into_boxed_slice())
}

fn decode_producer(root: &PgRow) -> Result<BlobDerivationProducer, BlobDerivationRepositoryError> {
    let implementation = root
        .try_get::<Option<Vec<u8>>, _>("implementation_digest")?
        .map(decode_digest)
        .transpose()?;
    let execution_id: Option<uuid::Uuid> = root.try_get("execution_id")?;
    let model_call_id: Option<uuid::Uuid> = root.try_get("model_call_id")?;
    match root.try_get::<&str, _>("producer_class")? {
        "deterministic" => Ok(BlobDerivationProducer::Deterministic {
            implementation: implementation.ok_or(BlobDerivationCorruption::InvalidProducer)?,
        }),
        "executed" => Ok(BlobDerivationProducer::Executed {
            execution_id: execution_id.ok_or(BlobDerivationCorruption::InvalidProducer)?,
            implementation: implementation.ok_or(BlobDerivationCorruption::InvalidProducer)?,
        }),
        "model_derived" => Ok(BlobDerivationProducer::ModelDerived {
            model_call: ModelCallId::from_uuid(
                model_call_id.ok_or(BlobDerivationCorruption::InvalidProducer)?,
            ),
        }),
        _ => Err(BlobDerivationCorruption::InvalidProducer.into()),
    }
}

fn decode_digest(bytes: Vec<u8>) -> Result<BlobDigest, BlobDerivationRepositoryError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| BlobDerivationCorruption::InvalidDigest)?;
    Ok(BlobDigest::from_bytes(bytes))
}
