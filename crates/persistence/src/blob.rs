//! PostgreSQL catalog for immutable blob identities and verified replicas.
//!
//! Store I/O is deliberately absent from this adapter. Callers publish and
//! verify bytes first, then register the durable facts in one transaction.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_blob_store::{BlobObjectKey, BlobStoreName, ExpectedBlob, MAX_BLOB_STORES};
use signalbox_domain::BlobDigest;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

use crate::{commit_failure_is_ambiguous, mapping::positive_u64_from_numeric};

const LOAD_ENTRY: &str = r#"SELECT blob.digest, blob.byte_length,
            blob_replica.store_name, blob_replica.object_key
       FROM blob
       LEFT JOIN blob_replica
         ON blob_replica.digest = blob.digest
      WHERE blob.digest = $1
      ORDER BY blob_replica.store_name COLLATE "C",
               blob_replica.object_key COLLATE "C""#;

/// One durable deployment store name and backend namespace identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobStoreBindingRecord {
    store: BlobStoreName,
    namespace_id: Uuid,
}

impl BlobStoreBindingRecord {
    /// Constructs one configured deployment identity.
    pub const fn new(store: BlobStoreName, namespace_id: Uuid) -> Self {
        Self {
            store,
            namespace_id,
        }
    }

    /// Returns the durable deployment store name.
    pub const fn store(&self) -> &BlobStoreName {
        &self.store
    }

    /// Returns the deployment-supplied backend namespace identity.
    pub const fn namespace_id(&self) -> Uuid {
        self.namespace_id
    }
}

/// One verified placement in a durable blob catalog entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobReplicaRecord {
    store: BlobStoreName,
    object_key: BlobObjectKey,
}

impl BlobReplicaRecord {
    /// Constructs one already-verified durable placement fact.
    pub const fn new(store: BlobStoreName, object_key: BlobObjectKey) -> Self {
        Self { store, object_key }
    }

    /// Returns the durable deployment store identity.
    pub const fn store(&self) -> &BlobStoreName {
        &self.store
    }

    /// Returns the exact recorded object key.
    pub const fn object_key(&self) -> &BlobObjectKey {
        &self.object_key
    }
}

/// One immutable blob identity and all currently recorded verified replicas.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobCatalogEntry {
    expected: ExpectedBlob,
    replicas: Box<[BlobReplicaRecord]>,
}

impl BlobCatalogEntry {
    /// Returns the immutable digest and positive byte length.
    pub const fn expected(&self) -> ExpectedBlob {
        self.expected
    }

    /// Returns verified replicas in durable store-name and object-key order.
    pub fn replicas(&self) -> &[BlobReplicaRecord] {
        &self.replicas
    }

    /// Finds the verified replica in one routed store, if present.
    pub fn replica_in_store(&self, store: &BlobStoreName) -> Option<&BlobReplicaRecord> {
        self.replicas
            .iter()
            .find(|replica| replica.store() == store)
    }
}

/// Durable catalog facts disagreed or could not reconstruct typed values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobCatalogCorruption {
    /// A stored digest was not exactly one SHA-256 value.
    InvalidDigest,
    /// A stored byte length was not a positive integral u64.
    InvalidByteLength,
    /// A stored deployment store name violated its type boundary.
    InvalidStoreName,
    /// A store name was already bound to another namespace UUID.
    StoreNamespaceMismatch,
    /// A namespace UUID was already bound to another store name.
    NamespaceStoreMismatch,
    /// A supplied replica named a store other than its supplied binding.
    ReplicaBindingMismatch,
    /// A stored object key violated its type boundary.
    InvalidObjectKey,
    /// One blob identity was paired with two positive byte lengths.
    BlobLengthMismatch,
    /// One digest/store replica slot was paired with two object keys.
    ReplicaKeyMismatch,
    /// One store/object key was paired with two blob identities.
    ObjectKeyCollision,
    /// A committed blob row had no verified replica.
    BlobWithoutReplica,
    /// A left-joined replica row had only some nullable fields present.
    PartialReplica,
    /// Registration returned without the replica it was required to record.
    MissingRegisteredReplica,
    /// Durable rows exceeded the version-one deployment store bound.
    StoreLimitExceeded,
}

impl fmt::Display for BlobCatalogCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidDigest => "stored blob digest is invalid",
            Self::InvalidByteLength => "stored blob byte length is invalid",
            Self::InvalidStoreName => "stored blob replica store name is invalid",
            Self::StoreNamespaceMismatch => "blob store name is bound to another namespace",
            Self::NamespaceStoreMismatch => "blob namespace is bound to another store name",
            Self::ReplicaBindingMismatch => "blob replica disagrees with its store binding",
            Self::InvalidObjectKey => "stored blob replica object key is invalid",
            Self::BlobLengthMismatch => "blob identity has conflicting byte lengths",
            Self::ReplicaKeyMismatch => "blob replica slot has conflicting object keys",
            Self::ObjectKeyCollision => "blob object key names conflicting identities",
            Self::BlobWithoutReplica => "blob identity has no verified replica",
            Self::PartialReplica => "blob replica row is incomplete",
            Self::MissingRegisteredReplica => "registered blob replica could not be reloaded",
            Self::StoreLimitExceeded => "blob store identities exceed the deployment bound",
        };
        formatter.write_str(message)
    }
}

impl Error for BlobCatalogCorruption {}

/// PostgreSQL unavailability, commit ambiguity, or durable catalog corruption.
#[derive(Debug)]
pub enum BlobCatalogRepositoryError {
    /// PostgreSQL could not complete a statement.
    Database(sqlx::Error),
    /// PostgreSQL did not reveal whether the registration commit took effect.
    CommitAmbiguous(sqlx::Error),
    /// Durable rows or competing registration facts disagreed.
    Corruption(BlobCatalogCorruption),
}

impl BlobCatalogRepositoryError {
    /// Returns the closed corruption class when durable facts disagreed.
    pub const fn corruption(&self) -> Option<BlobCatalogCorruption> {
        match self {
            Self::Corruption(corruption) => Some(*corruption),
            Self::Database(_) | Self::CommitAmbiguous(_) => None,
        }
    }
}

impl fmt::Display for BlobCatalogRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "blob catalog database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "blob catalog commit outcome is ambiguous: {error}"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for BlobCatalogRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for BlobCatalogRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<BlobCatalogCorruption> for BlobCatalogRepositoryError {
    fn from(error: BlobCatalogCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of the append-only verified-replica catalog.
#[derive(Clone, Debug)]
pub struct BlobCatalogRepository {
    pool: PgPool,
}

/// Test-only ownership token for a deliberately unavailable blob catalog.
#[cfg(feature = "test-support")]
pub struct BlobCatalogRegistrationFault {
    pool: PgPool,
}

#[cfg(feature = "test-support")]
impl BlobCatalogRegistrationFault {
    /// Restores the catalog after the registration-failure assertion.
    pub async fn restore(self) -> Result<(), BlobCatalogRepositoryError> {
        sqlx::query("ALTER TABLE blob_registration_unavailable RENAME TO blob")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl BlobCatalogRepository {
    /// Uses the supplied pool for independent registration and read operations.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Makes registration unavailable for a composed integration test.
    #[cfg(feature = "test-support")]
    pub async fn inject_registration_fault(
        &self,
    ) -> Result<BlobCatalogRegistrationFault, BlobCatalogRepositoryError> {
        sqlx::query("ALTER TABLE blob RENAME TO blob_registration_unavailable")
            .execute(&self.pool)
            .await?;
        Ok(BlobCatalogRegistrationFault {
            pool: self.pool.clone(),
        })
    }

    /// Reports whether no namespace binding or blob identity has been recorded.
    pub async fn is_empty(&self) -> Result<bool, BlobCatalogRepositoryError> {
        let empty: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM blob_store_binding)
                    AND NOT EXISTS (SELECT 1 FROM blob)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(empty)
    }

    /// Loads every durable store binding in bytewise store-name order.
    pub async fn recorded_store_bindings(
        &self,
    ) -> Result<Box<[BlobStoreBindingRecord]>, BlobCatalogRepositoryError> {
        let rows = sqlx::query(
            r#"SELECT store_name, namespace_id
                 FROM blob_store_binding
                ORDER BY store_name COLLATE "C""#,
        )
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > MAX_BLOB_STORES {
            return Err(BlobCatalogCorruption::StoreLimitExceeded.into());
        }
        rows.into_iter()
            .map(|row| {
                let store = BlobStoreName::try_new(row.try_get::<String, _>("store_name")?)
                    .map_err(|_| BlobCatalogCorruption::InvalidStoreName)?;
                Ok(BlobStoreBindingRecord::new(
                    store,
                    row.try_get("namespace_id")?,
                ))
            })
            .collect::<Result<Vec<_>, BlobCatalogRepositoryError>>()
            .map(Vec::into_boxed_slice)
    }

    /// Idempotently records one deployment store name and namespace UUID.
    pub async fn register_store_binding(
        &self,
        binding: BlobStoreBindingRecord,
    ) -> Result<BlobStoreBindingRecord, BlobCatalogRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        ensure_store_binding(&mut transaction, &binding).await?;
        match transaction.commit().await {
            Ok(()) => Ok(binding),
            Err(error) if commit_failure_is_ambiguous(&error) => {
                Err(BlobCatalogRepositoryError::CommitAmbiguous(error))
            }
            Err(error) => Err(BlobCatalogRepositoryError::Database(error)),
        }
    }

    /// Loads one catalogued identity and its bounded-by-configuration replicas.
    pub async fn find(
        &self,
        digest: BlobDigest,
    ) -> Result<Option<BlobCatalogEntry>, BlobCatalogRepositoryError> {
        let rows = sqlx::query(LOAD_ENTRY)
            .bind(digest.as_bytes().as_slice())
            .fetch_all(&self.pool)
            .await?;
        decode_entry(&rows)
    }

    /// Idempotently registers one replica only after its store verified bytes.
    ///
    /// Matching concurrent registration reloads the winner. Any disagreement
    /// becomes typed corruption rather than exposing a uniqueness error.
    pub async fn register_verified_replica(
        &self,
        expected: ExpectedBlob,
        binding: BlobStoreBindingRecord,
        replica: BlobReplicaRecord,
    ) -> Result<BlobCatalogEntry, BlobCatalogRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let entry = register_verified_replica_in_transaction(
            &mut transaction,
            expected,
            &binding,
            &replica,
        )
        .await?;

        match transaction.commit().await {
            Ok(()) => Ok(entry),
            Err(error) if commit_failure_is_ambiguous(&error) => {
                Err(BlobCatalogRepositoryError::CommitAmbiguous(error))
            }
            Err(error) => Err(BlobCatalogRepositoryError::Database(error)),
        }
    }
}

/// Registers verified placement facts inside the transaction that first
/// references them from an aggregate.
pub(crate) async fn register_verified_replica_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    expected: ExpectedBlob,
    binding: &BlobStoreBindingRecord,
    replica: &BlobReplicaRecord,
) -> Result<BlobCatalogEntry, BlobCatalogRepositoryError> {
    if binding.store() != replica.store() {
        return Err(BlobCatalogCorruption::ReplicaBindingMismatch.into());
    }
    ensure_store_binding(transaction, binding).await?;
    sqlx::query(
        "INSERT INTO blob (digest, byte_length)
         VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(expected.digest().as_bytes().as_slice())
    .bind(Decimal::from(expected.byte_length()))
    .execute(&mut **transaction)
    .await?;

    let recorded_length: Decimal =
        sqlx::query_scalar("SELECT byte_length FROM blob WHERE digest = $1")
            .bind(expected.digest().as_bytes().as_slice())
            .fetch_one(&mut **transaction)
            .await?;
    let recorded_length = positive_u64_from_numeric(recorded_length)
        .map_err(|_| BlobCatalogCorruption::InvalidByteLength)?;
    if recorded_length != expected.byte_length() {
        return Err(BlobCatalogCorruption::BlobLengthMismatch.into());
    }

    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(expected.digest().as_bytes().as_slice())
    .bind(replica.store().as_str())
    .bind(replica.object_key().as_str())
    .execute(&mut **transaction)
    .await?;

    validate_registered_replica(transaction, expected.digest(), replica).await?;
    load_in_transaction(transaction, expected.digest())
        .await?
        .ok_or(BlobCatalogRepositoryError::Corruption(
            BlobCatalogCorruption::MissingRegisteredReplica,
        ))
}

async fn ensure_store_binding(
    transaction: &mut Transaction<'_, Postgres>,
    supplied: &BlobStoreBindingRecord,
) -> Result<(), BlobCatalogRepositoryError> {
    let namespace_for_name: Option<Uuid> = sqlx::query_scalar(
        "SELECT namespace_id
           FROM blob_store_binding
          WHERE store_name = $1",
    )
    .bind(supplied.store().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(namespace_id) = namespace_for_name {
        if namespace_id == supplied.namespace_id() {
            return Ok(());
        }
        return Err(BlobCatalogCorruption::StoreNamespaceMismatch.into());
    }

    // Store bindings are deployment-time facts. Serialize their admission so
    // concurrent new names cannot both observe the final free catalog slot.
    // The ordinary established-binding path above remains lock-free.
    sqlx::query("LOCK TABLE blob_store_binding IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut **transaction)
        .await?;

    let namespace_for_name: Option<Uuid> = sqlx::query_scalar(
        "SELECT namespace_id
           FROM blob_store_binding
          WHERE store_name = $1",
    )
    .bind(supplied.store().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(namespace_id) = namespace_for_name {
        if namespace_id == supplied.namespace_id() {
            return Ok(());
        }
        return Err(BlobCatalogCorruption::StoreNamespaceMismatch.into());
    }

    let store_for_namespace: Option<String> = sqlx::query_scalar(
        "SELECT store_name
           FROM blob_store_binding
          WHERE namespace_id = $1",
    )
    .bind(supplied.namespace_id())
    .fetch_optional(&mut **transaction)
    .await?;
    if store_for_namespace.is_some() {
        return Err(BlobCatalogCorruption::NamespaceStoreMismatch.into());
    }

    let store_count: i64 = sqlx::query_scalar("SELECT count(*) FROM blob_store_binding")
        .fetch_one(&mut **transaction)
        .await?;
    let maximum_store_count =
        i64::try_from(MAX_BLOB_STORES).map_err(|_| BlobCatalogCorruption::StoreLimitExceeded)?;
    if store_count >= maximum_store_count {
        return Err(BlobCatalogCorruption::StoreLimitExceeded.into());
    }

    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ($1, $2)",
    )
    .bind(supplied.store().as_str())
    .bind(supplied.namespace_id())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn load_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    digest: BlobDigest,
) -> Result<Option<BlobCatalogEntry>, BlobCatalogRepositoryError> {
    let rows = sqlx::query(LOAD_ENTRY)
        .bind(digest.as_bytes().as_slice())
        .fetch_all(&mut **transaction)
        .await?;
    decode_entry(&rows)
}

async fn validate_registered_replica(
    transaction: &mut Transaction<'_, Postgres>,
    digest: BlobDigest,
    supplied: &BlobReplicaRecord,
) -> Result<(), BlobCatalogRepositoryError> {
    let key_in_slot: Option<String> = sqlx::query_scalar(
        "SELECT object_key
           FROM blob_replica
          WHERE digest = $1 AND store_name = $2",
    )
    .bind(digest.as_bytes().as_slice())
    .bind(supplied.store().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(recorded_key) = key_in_slot {
        let recorded_key = BlobObjectKey::try_from_recorded(recorded_key)
            .map_err(|_| BlobCatalogCorruption::InvalidObjectKey)?;
        if &recorded_key == supplied.object_key() {
            return Ok(());
        }
        return Err(BlobCatalogCorruption::ReplicaKeyMismatch.into());
    }

    let digest_at_key: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT digest
           FROM blob_replica
          WHERE store_name = $1 AND object_key = $2",
    )
    .bind(supplied.store().as_str())
    .bind(supplied.object_key().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(recorded_digest) = digest_at_key {
        let recorded_digest = decode_digest(recorded_digest)?;
        if recorded_digest != digest {
            return Err(BlobCatalogCorruption::ObjectKeyCollision.into());
        }
    }
    Err(BlobCatalogCorruption::MissingRegisteredReplica.into())
}

fn decode_entry(rows: &[PgRow]) -> Result<Option<BlobCatalogEntry>, BlobCatalogRepositoryError> {
    if rows.len() > MAX_BLOB_STORES {
        return Err(BlobCatalogCorruption::StoreLimitExceeded.into());
    }
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let digest = decode_digest(first.try_get("digest")?)?;
    let byte_length = positive_u64_from_numeric(first.try_get("byte_length")?)
        .map_err(|_| BlobCatalogCorruption::InvalidByteLength)?;
    let expected = ExpectedBlob::try_new(digest, byte_length)
        .map_err(|_| BlobCatalogCorruption::InvalidByteLength)?;
    let mut replicas = Vec::with_capacity(rows.len());
    for row in rows {
        let row_digest = decode_digest(row.try_get("digest")?)?;
        let row_length = positive_u64_from_numeric(row.try_get("byte_length")?)
            .map_err(|_| BlobCatalogCorruption::InvalidByteLength)?;
        if row_digest != digest || row_length != byte_length {
            return Err(BlobCatalogCorruption::BlobLengthMismatch.into());
        }
        let store: Option<String> = row.try_get("store_name")?;
        let object_key: Option<String> = row.try_get("object_key")?;
        match (store, object_key) {
            (Some(store), Some(object_key)) => replicas.push(BlobReplicaRecord::new(
                BlobStoreName::try_new(store)
                    .map_err(|_| BlobCatalogCorruption::InvalidStoreName)?,
                BlobObjectKey::try_from_recorded(object_key)
                    .map_err(|_| BlobCatalogCorruption::InvalidObjectKey)?,
            )),
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                return Err(BlobCatalogCorruption::PartialReplica.into());
            }
        }
    }
    if replicas.is_empty() {
        return Err(BlobCatalogCorruption::BlobWithoutReplica.into());
    }
    Ok(Some(BlobCatalogEntry {
        expected,
        replicas: replicas.into_boxed_slice(),
    }))
}

fn decode_digest(bytes: Vec<u8>) -> Result<BlobDigest, BlobCatalogRepositoryError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| BlobCatalogCorruption::InvalidDigest)?;
    Ok(BlobDigest::from_bytes(bytes))
}
