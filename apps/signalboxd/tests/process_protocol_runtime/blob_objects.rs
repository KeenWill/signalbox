//! Blob objects coverage.

use super::*;

pub(crate) async fn append_blob_upload(
    connection: &mut Connection,
    request_id: u64,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    connection
        .request(
            request_id,
            ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(bytes.to_vec()),
            },
        )
        .await?;
    let response = connection.response().await?;
    assert_eq!(
        response.message(),
        &ServerMessage::BlobUploadAppended {
            assembled_length_bytes: CanonicalU64::new(u64::try_from(bytes.len())?),
        }
    );
    Ok(())
}

pub(crate) async fn commit_blob_upload(
    connection: &mut Connection,
    wire_digest: CanonicalBlobDigest,
    expected_length: CanonicalU64,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadBegun {
            expected_digest: wire_digest,
            expected_length_bytes: expected_length,
        }
    );
    append_blob_upload(connection, 2, bytes).await?;
    connection
        .request(3, ClientRequest::CommitBlobUpload {})
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadCommitted {
            digest: wire_digest,
            byte_length: expected_length,
        }
    );
    Ok(())
}

/// the daemon streams exact bytes through one upload lifecycle and
/// registers one immutable identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_upload_round_trips_exact_bytes() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"exact immutable upload bytes";
    let digest = BlobDigest::digest(bytes);
    let wire_digest = CanonicalBlobDigest::from_digest(digest);
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;

    commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;

    let catalog = BlobCatalogRepository::new(runtime.pool.clone())
        .find(digest)
        .await?
        .expect("the committed upload is catalogued");
    assert_eq!(catalog.expected().byte_length(), expected_length.value());
    assert_eq!(catalog.replicas().len(), 1);
    let registry = runtime.blob_store_registry();
    let (store_name, store) = registry.routed_store(BlobStorageClass::UserAttachment);
    assert_eq!(catalog.replicas()[0].store(), store_name);
    let opened = store.open(catalog.replicas()[0].object_key()).await?;
    let mut observed = Vec::new();
    opened.into_reader().read_to_end(&mut observed).await?;
    assert_eq!(observed, bytes);

    drop(connection);
    runtime.stop().await
}

/// an exact retry against the routed store short-circuits as already
/// present without accepting another upload body.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_upload_exact_retry_is_already_present() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"exact immutable upload retry bytes";
    let wire_digest = CanonicalBlobDigest::from_digest(BlobDigest::digest(bytes));
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;
    commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;

    connection
        .request(
            4,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;

    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadAlreadyPresent {
            digest: wire_digest,
            byte_length: expected_length,
        }
    );
    drop(connection);
    runtime.stop().await
}

/// failure to register after verified publication leaves an orphan
/// object and never a dangling catalog reference.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn registration_failure_after_publication_leaves_only_an_orphan() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"published before unavailable catalog";
    let digest = BlobDigest::digest(bytes);
    let wire_digest = CanonicalBlobDigest::from_digest(digest);
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadBegun {
            expected_digest: wire_digest,
            expected_length_bytes: expected_length,
        }
    );
    append_blob_upload(&mut connection, 2, bytes).await?;
    let catalog = BlobCatalogRepository::new(runtime.pool.clone());
    let catalog_fault = catalog.inject_registration_fault().await?;
    connection
        .request(3, ClientRequest::CommitBlobUpload {})
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::Unavailable,
            message: String::from("the requested operation is unavailable"),
            detail: ErrorDetail::none(),
        }
    );
    catalog_fault.restore().await?;

    assert!(catalog.find(digest).await?.is_none());
    let registry = runtime.blob_store_registry();
    let (_store_name, store) = registry.routed_store(BlobStorageClass::UserAttachment);
    let orphan = store.open(&BlobObjectKey::for_digest(digest)).await?;
    let mut observed = Vec::new();
    orphan.into_reader().read_to_end(&mut observed).await?;
    assert_eq!(observed, bytes);
    drop(connection);
    runtime.stop().await
}

/// metadata reports the catalog's exact bounded identity, length, and
/// replica count.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_metadata_reports_exact_catalog_facts() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"metadata blob fixture").await?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobMetadata {
                digest: fixture.wire_digest,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::BlobMetadata {
            digest: fixture.wire_digest,
            byte_length: fixture.expected_length,
            replica_count: fixture.expected_replica_count(),
        }
    );

    fixture.stop().await
}

/// a direct range returns the exact requested bytes only after the
/// recorded replica verifies.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_range_returns_exact_verified_bytes() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"verified direct blob range").await?;
    let offset_bytes = CanonicalU64::new(9);
    let length_bytes = CanonicalU64::new(6);
    let expected_bytes = fixture.expected_range(offset_bytes, length_bytes);

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes,
                length_bytes,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::BlobChunkRead {
            digest: fixture.wire_digest,
            offset_bytes,
            bytes: BlobChunk::new(expected_bytes.to_vec()),
        }
    );

    fixture.stop().await
}

/// an exact range outside the catalog length is rejected before store
/// access with the typed range facts.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_range_out_of_bounds_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"out of bounds blob fixture").await?;
    let offset_bytes = CanonicalU64::new(u64::MAX);
    let length_bytes = CanonicalU64::new(1);

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes,
                length_bytes,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob read was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobReadRangeOutOfBounds {
                offset_bytes,
                length_bytes,
                blob_length_bytes: fixture.expected_length,
            }),
        }
    );

    fixture.stop().await
}

/// an absent recorded object returns the content-silent missing code.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_read_missing_replica_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"missing replica blob fixture").await?;
    fs::remove_file(fixture.object_path())?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes: CanonicalU64::new(0),
                length_bytes: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::BlobMissing,
            message: String::from("all recorded blob replicas are missing"),
            detail: ErrorDetail::none(),
        }
    );

    fixture.stop().await
}

/// a recorded object whose bytes no longer match the catalog returns
/// the content-silent corruption code.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_read_corrupt_replica_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"corrupt replica blob fixture").await?;
    fs::write(fixture.object_path(), b"corrupt")?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes: CanonicalU64::new(0),
                length_bytes: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::BlobCorrupt,
            message: String::from("all usable blob replicas are corrupt"),
            detail: ErrorDetail::none(),
        }
    );

    fixture.stop().await
}

/// a digest absent from the catalog returns the content-silent
/// not-found code without store access.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_metadata_absent_catalog_entry_is_not_found() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let absent_digest = CanonicalBlobDigest::from_bytes([0xcd; 32]);
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::ReadBlobMetadata {
                digest: absent_digest,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::NotFound,
            message: String::from("the requested blob was not found"),
            detail: ErrorDetail::none(),
        }
    );

    drop(connection);
    runtime.stop().await
}
