//! PostgreSQL integration coverage for the immutable blob catalog.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, sync::Arc};

use signalbox_application::BlobDerivationRecordOutcome;
use signalbox_blob_store::{BlobObjectKey, BlobStoreName, ExpectedBlob, MAX_BLOB_STORES};
use signalbox_domain::{
    BlobDerivation, BlobDerivationId, BlobDerivationProducer, BlobDigest, BlobTransformation,
    BlobTransformationName,
};
use signalbox_persistence::{
    blob::{
        BlobCatalogCorruption, BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord,
    },
    blob_derivation::BlobDerivationRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_blob";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const CONTENT: &[u8] = b"catalogued immutable blob";
const OTHER_CONTENT: &[u8] = b"another immutable blob";
const PRIMARY_STORE: &str = "primary";
const PRIMARY_NAMESPACE: u128 = 0x5a10_0001;
const SECONDARY_NAMESPACE: u128 = 0x5a10_0002;
const BOUNDARY_NAMESPACE_A: u128 = 0x5a10_0031;
const BOUNDARY_NAMESPACE_B: u128 = 0x5a10_0032;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs())
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn expected_blob(bytes: &[u8]) -> ExpectedBlob {
    ExpectedBlob::try_new(
        BlobDigest::digest(bytes),
        u64::try_from(bytes.len()).expect("the bounded fixture length fits u64"),
    )
    .expect("the fixture is nonempty")
}

fn store(name: &str) -> BlobStoreName {
    BlobStoreName::try_new(name).expect("the fixture store name is valid")
}

fn replica(expected: ExpectedBlob, store: &str) -> BlobReplicaRecord {
    BlobReplicaRecord::new(
        self::store(store),
        BlobObjectKey::for_digest(expected.digest()),
    )
}

fn binding(name: &str, namespace: u128) -> BlobStoreBindingRecord {
    BlobStoreBindingRecord::new(store(name), Uuid::from_u128(namespace))
}

fn thumbnail_derivation(input: BlobDigest, output: BlobDigest) -> BlobDerivation {
    BlobDerivation::try_new(
        BlobDerivationId::from_uuid(Uuid::from_u128(0x5a10_0700)),
        [input],
        BlobTransformation::try_new(
            BlobTransformationName::try_new("image.thumbnail")
                .expect("the fixture transformation name is valid"),
            1,
            &serde_json::json!({"edge_px": 256, "format": "image/png"}),
        )
        .expect("the fixture transformation is valid"),
        BlobDerivationProducer::Deterministic {
            implementation: BlobDigest::digest(b"thumbnail-worker-v1"),
        },
        [output],
    )
    .expect("the fixture derivation is valid")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blob_catalog_is_empty_until_its_first_binding() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());

    let initially_empty = repository.is_empty().await?;
    repository
        .register_store_binding(binding(PRIMARY_STORE, PRIMARY_NAMESPACE))
        .await?;
    let empty_after_binding = repository.is_empty().await?;

    assert!(initially_empty);
    assert!(!empty_after_binding);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: matching deployment store binding registration is idempotent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_matching_blob_store_binding_registration_is_idempotent()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let primary = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);

    let first = repository.register_store_binding(primary.clone()).await?;
    let repeated = repository.register_store_binding(primary.clone()).await?;

    assert_eq!(first, primary);
    assert_eq!(repeated, primary);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: concurrent new store names cannot exceed the durable store bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_concurrent_store_binding_admission_preserves_the_catalog_bound()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let seed_count =
        i64::try_from(MAX_BLOB_STORES - 1).expect("the bounded deployment store count fits i64");
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         SELECT 'seed-' || ordinal,
                ('5a100001-0000-0000-0000-' || lpad(ordinal::text, 12, '0'))::uuid
           FROM generate_series(1, $1) AS ordinal",
    )
    .bind(seed_count)
    .execute(&pool)
    .await?;
    let first_repository = repository.clone();
    let second_repository = repository.clone();

    let (first, second) = tokio::join!(
        first_repository.register_store_binding(binding("boundary-a", BOUNDARY_NAMESPACE_A,)),
        second_repository.register_store_binding(binding("boundary-b", BOUNDARY_NAMESPACE_B,)),
    );
    let first_reached_limit = first.as_ref().err().and_then(|error| error.corruption())
        == Some(BlobCatalogCorruption::StoreLimitExceeded);
    let second_reached_limit = second.as_ref().err().and_then(|error| error.corruption())
        == Some(BlobCatalogCorruption::StoreLimitExceeded);
    let binding_count: i64 = sqlx::query_scalar("SELECT count(*) FROM blob_store_binding")
        .fetch_one(&pool)
        .await?;
    let maximum_store_count =
        i64::try_from(MAX_BLOB_STORES).expect("the bounded deployment store count fits i64");

    assert_eq!(u8::from(first.is_ok()) + u8::from(second.is_ok()), 1);
    assert_eq!(
        u8::from(first_reached_limit) + u8::from(second_reached_limit),
        1
    );
    assert_eq!(binding_count, maximum_store_count);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: one deployment store name cannot acquire another namespace UUID.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_blob_store_name_rejects_another_namespace() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    repository
        .register_store_binding(binding(PRIMARY_STORE, PRIMARY_NAMESPACE))
        .await?;

    let error = repository
        .register_store_binding(binding(PRIMARY_STORE, SECONDARY_NAMESPACE))
        .await
        .expect_err("one store name cannot acquire another namespace");

    assert_eq!(
        error.corruption(),
        Some(BlobCatalogCorruption::StoreNamespaceMismatch)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: one namespace UUID cannot acquire another deployment store name.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_blob_namespace_rejects_another_store_name() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    repository
        .register_store_binding(binding(PRIMARY_STORE, PRIMARY_NAMESPACE))
        .await?;

    let error = repository
        .register_store_binding(binding("secondary", PRIMARY_NAMESPACE))
        .await
        .expect_err("one namespace cannot acquire another store name");

    assert_eq!(
        error.corruption(),
        Some(BlobCatalogCorruption::NamespaceStoreMismatch)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: one replica can be registered only in the supplied durable store
/// binding, and a rejected disagreement records no catalog fact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_replica_registration_requires_its_matching_binding() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let expected = expected_blob(CONTENT);

    let error = repository
        .register_verified_replica(
            expected,
            binding(PRIMARY_STORE, PRIMARY_NAMESPACE),
            replica(expected, "secondary"),
        )
        .await
        .expect_err("a replica cannot name another store than its binding");

    assert_eq!(
        error.corruption(),
        Some(BlobCatalogCorruption::ReplicaBindingMismatch)
    );
    assert!(repository.is_empty().await?);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: equal registration and replay produce one catalog identity and one
/// verified replica.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_blob_registration_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let expected = expected_blob(CONTENT);
    let replica = replica(expected, PRIMARY_STORE);
    let store_binding = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);

    let first = repository
        .register_verified_replica(expected, store_binding.clone(), replica.clone())
        .await?;
    let repeated = repository
        .register_verified_replica(expected, store_binding, replica.clone())
        .await?;
    let loaded = repository
        .find(expected.digest())
        .await?
        .expect("the registered identity is loadable");

    assert_eq!(first, repeated);
    assert_eq!(loaded, first);
    assert_eq!(loaded.expected(), expected);
    assert_eq!(loaded.replicas(), &[replica]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: concurrent equal registration reloads the winning catalog state
/// instead of surfacing a uniqueness failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_concurrent_registration_reuses_the_winner() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = Arc::new(BlobCatalogRepository::new(pool.clone()));
    let expected = expected_blob(CONTENT);
    let replica = replica(expected, PRIMARY_STORE);
    let first_repository = Arc::clone(&repository);
    let second_repository = Arc::clone(&repository);
    let first_replica = replica.clone();
    let second_replica = replica.clone();
    let first_binding = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);
    let second_binding = first_binding.clone();

    let (first, second) = tokio::join!(
        first_repository.register_verified_replica(expected, first_binding, first_replica),
        second_repository.register_verified_replica(expected, second_binding, second_replica),
    );
    let first = first?;
    let second = second?;
    let blob_count: i64 = sqlx::query_scalar("SELECT count(*) FROM blob")
        .fetch_one(&pool)
        .await?;
    let replica_count: i64 = sqlx::query_scalar("SELECT count(*) FROM blob_replica")
        .fetch_one(&pool)
        .await?;

    assert_eq!(first, second);
    assert_eq!(first.replicas(), &[replica]);
    assert_eq!(blob_count, 1);
    assert_eq!(replica_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: one digest cannot acquire a conflicting positive byte length.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_registration_rejects_length_disagreement() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let expected = expected_blob(CONTENT);
    let replica = replica(expected, PRIMARY_STORE);
    let store_binding = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);
    repository
        .register_verified_replica(expected, store_binding.clone(), replica.clone())
        .await?;
    let conflicting = ExpectedBlob::try_new(expected.digest(), expected.byte_length() + 1)
        .expect("the conflicting fixture length remains positive");

    let error = repository
        .register_verified_replica(conflicting, store_binding, replica)
        .await
        .expect_err("one digest cannot be registered with another length");

    assert_eq!(
        error.corruption(),
        Some(BlobCatalogCorruption::BlobLengthMismatch)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: replica-slot and store/object-key uniqueness disagreements are
/// closed corruption rather than raw database errors.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_registration_maps_replica_uniqueness_disagreement() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let expected = expected_blob(CONTENT);
    let replica = replica(expected, PRIMARY_STORE);
    let store_binding = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);
    repository
        .register_verified_replica(expected, store_binding.clone(), replica.clone())
        .await?;
    let different_key = BlobReplicaRecord::new(
        store(PRIMARY_STORE),
        BlobObjectKey::try_from_recorded("sha256/custom/different-key")?,
    );

    let slot_error = repository
        .register_verified_replica(expected, store_binding.clone(), different_key)
        .await
        .expect_err("one replica slot cannot name another key");
    let other = expected_blob(OTHER_CONTENT);
    let key_error = repository
        .register_verified_replica(other, store_binding, replica)
        .await
        .expect_err("one store key cannot name another digest");

    assert_eq!(
        slot_error.corruption(),
        Some(BlobCatalogCorruption::ReplicaKeyMismatch)
    );
    assert_eq!(
        key_error.corruption(),
        Some(BlobCatalogCorruption::ObjectKeyCollision)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: a committed blob identity always has a verified replica.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_blob_identity_cannot_commit_without_a_replica() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let expected = expected_blob(CONTENT);
    let mut incomplete = pool.begin().await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, $2)")
        .bind(expected.digest().as_bytes().as_slice())
        .bind(rust_decimal::Decimal::from(expected.byte_length()))
        .execute(&mut *incomplete)
        .await?;
    let replica_less_commit = incomplete.commit().await;

    assert!(replica_less_commit.is_err());

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-057: blob catalog facts cannot be updated, deleted, or truncated.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv057_blob_catalog_facts_are_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let expected = expected_blob(CONTENT);
    let repository = BlobCatalogRepository::new(pool.clone());
    repository
        .register_verified_replica(
            expected,
            binding(PRIMARY_STORE, PRIMARY_NAMESPACE),
            replica(expected, PRIMARY_STORE),
        )
        .await?;

    let update = sqlx::query("UPDATE blob SET byte_length = byte_length + 1")
        .execute(&pool)
        .await;
    let binding_update = sqlx::query("UPDATE blob_store_binding SET store_name = 'renamed'")
        .execute(&pool)
        .await;
    let delete = sqlx::query("DELETE FROM blob_replica").execute(&pool).await;
    let binding_delete = sqlx::query("DELETE FROM blob_store_binding")
        .execute(&pool)
        .await;
    let truncate = sqlx::query("TRUNCATE blob, blob_replica, blob_store_binding")
        .execute(&pool)
        .await;

    assert!(update.is_err());
    assert!(binding_update.is_err());
    assert!(delete.is_err());
    assert!(binding_delete.is_err());
    assert!(truncate.is_err());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn missing_blob_is_not_catalogued() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());

    let loaded = repository.find(BlobDigest::digest(b"missing")).await?;

    assert_eq!(loaded, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recorded_store_bindings_use_bytewise_name_order() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = BlobCatalogRepository::new(pool.clone());
    let first = expected_blob(CONTENT);
    let second = expected_blob(OTHER_CONTENT);
    let punctuated = binding("a_", PRIMARY_NAMESPACE);
    let letters = binding("aa", SECONDARY_NAMESPACE);
    repository
        .register_verified_replica(
            first,
            letters.clone(),
            replica(first, letters.store().as_str()),
        )
        .await?;
    repository
        .register_verified_replica(
            second,
            punctuated.clone(),
            replica(second, punctuated.store().as_str()),
        )
        .await?;

    let bindings = repository.recorded_store_bindings().await?;

    assert_eq!(bindings.as_ref(), &[punctuated, letters]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-071: one deterministic key has one immutable, complete derivation record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv071_deterministic_blob_derivation_is_cached_and_immutable() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let catalog = BlobCatalogRepository::new(pool.clone());
    let repository = BlobDerivationRepository::new(pool.clone());
    let input = expected_blob(CONTENT);
    let output = expected_blob(OTHER_CONTENT);
    let store_binding = binding(PRIMARY_STORE, PRIMARY_NAMESPACE);
    catalog
        .register_verified_replica(input, store_binding.clone(), replica(input, PRIMARY_STORE))
        .await?;
    catalog
        .register_verified_replica(output, store_binding, replica(output, PRIMARY_STORE))
        .await?;
    let derivation = thumbnail_derivation(input.digest(), output.digest());
    let key = derivation
        .deterministic_key()
        .expect("the fixture producer is deterministic");

    let recorded = repository.record(derivation.clone()).await?;
    let replay = repository.record(derivation.clone()).await?;
    let loaded = repository.find_deterministic(key).await?;
    let boundary_transformation = BlobTransformation::try_new(
        BlobTransformationName::try_new("image.boundary")
            .expect("the boundary transformation name is valid"),
        1,
        &serde_json::json!({"payload": "x".repeat(4082)}),
    )
    .expect("the canonical boundary parameters are valid");
    assert_eq!(boundary_transformation.parameters_json().len(), 4096);
    let boundary_derivation = BlobDerivation::try_new(
        BlobDerivationId::from_uuid(Uuid::from_u128(0x5a10_0710)),
        [input.digest()],
        boundary_transformation,
        BlobDerivationProducer::Deterministic {
            implementation: BlobDigest::digest(b"boundary-worker-v1"),
        },
        [output.digest()],
    )
    .expect("the boundary derivation is valid");
    let boundary_key = boundary_derivation
        .deterministic_key()
        .expect("the boundary producer is deterministic");
    repository.record(boundary_derivation.clone()).await?;
    let loaded_boundary = repository.find_deterministic(boundary_key).await?;
    let mutation = sqlx::query("UPDATE blob_derivation SET transformation_version = 2")
        .execute(&pool)
        .await;
    let extra_output = sqlx::query(
        "INSERT INTO blob_derivation_output (derivation_id, output_ordinal, digest)
         VALUES ($1, 1, $2)",
    )
    .bind(derivation.id().into_uuid())
    .bind(output.digest().as_bytes().as_slice())
    .execute(&pool)
    .await;
    let truncation = sqlx::query("TRUNCATE blob_derivation CASCADE")
        .execute(&pool)
        .await;
    let null_implementation = sqlx::query(
        "INSERT INTO blob_derivation (
             derivation_id, deterministic_key, transformation_name, transformation_version,
             parameters_json, parameters_canonical, producer_class, implementation_digest,
             execution_id, model_call_id, input_count, output_count
         ) VALUES ($1, $2, 'image.thumbnail', 1, '{}'::jsonb, '{}',
                   'deterministic', NULL, NULL, NULL, 1, 1)",
    )
    .bind(Uuid::from_u128(0x5a10_0711))
    .bind(
        BlobDigest::digest(b"null implementation key")
            .as_bytes()
            .as_slice(),
    )
    .execute(&pool)
    .await;
    let mut malformed_ordinals = pool.begin().await?;
    sqlx::query(
        "INSERT INTO blob_derivation (
             derivation_id, deterministic_key, transformation_name, transformation_version,
             parameters_json, parameters_canonical, producer_class, implementation_digest,
             execution_id, model_call_id, input_count, output_count
         ) VALUES ($1, $2, 'image.thumbnail', 1, '{}'::jsonb, '{}',
                   'deterministic', $3, NULL, NULL, 1, 1)",
    )
    .bind(Uuid::from_u128(0x5a10_0712))
    .bind(
        BlobDigest::digest(b"malformed ordinal key")
            .as_bytes()
            .as_slice(),
    )
    .bind(BlobDigest::digest(b"implementation").as_bytes().as_slice())
    .execute(&mut *malformed_ordinals)
    .await?;
    sqlx::query(
        "INSERT INTO blob_derivation_input (derivation_id, input_ordinal, digest)
         VALUES ($1, 15, $2)",
    )
    .bind(Uuid::from_u128(0x5a10_0712))
    .bind(input.digest().as_bytes().as_slice())
    .execute(&mut *malformed_ordinals)
    .await?;
    sqlx::query(
        "INSERT INTO blob_derivation_output (derivation_id, output_ordinal, digest)
         VALUES ($1, 0, $2)",
    )
    .bind(Uuid::from_u128(0x5a10_0712))
    .bind(output.digest().as_bytes().as_slice())
    .execute(&mut *malformed_ordinals)
    .await?;
    let malformed_ordinals = malformed_ordinals.commit().await;

    assert_eq!(
        recorded,
        BlobDerivationRecordOutcome::Recorded(derivation.clone())
    );
    assert_eq!(
        replay,
        BlobDerivationRecordOutcome::Existing(derivation.clone())
    );
    assert_eq!(loaded, Some(derivation));
    assert_eq!(loaded_boundary, Some(boundary_derivation));
    let mutation = mutation.expect_err("the immutability trigger rejects the update");
    let extra_output = extra_output.expect_err("the completeness trigger rejects the extra output");
    let truncation = truncation.expect_err("the truncate trigger rejects the statement");
    let null_implementation =
        null_implementation.expect_err("producer provenance rejects a null implementation");
    let malformed_ordinals =
        malformed_ordinals.expect_err("completeness rejects non-contiguous ordinals");
    assert!(
        mutation
            .to_string()
            .contains("blob derivation records are immutable")
    );
    assert!(
        extra_output
            .to_string()
            .contains("blob derivation record is incomplete")
    );
    assert!(
        truncation
            .to_string()
            .contains("blob derivation records are immutable")
    );
    assert!(
        null_implementation
            .to_string()
            .contains("blob_derivation_producer_shape")
    );
    assert!(
        malformed_ordinals
            .to_string()
            .contains("blob derivation record is incomplete")
    );

    pool.close().await;
    drop(container);
    Ok(())
}
