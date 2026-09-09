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
            blob_length_bytes: fixture.expected_length,
            digest: fixture.wire_digest,
            offset_bytes,
            bytes: BlobChunk::new(expected_bytes.to_vec()),
        }
    );

    fixture.stop().await
}

/// A range beyond EOF returns empty bytes and the catalog length.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_range_beyond_eof_is_empty() -> Result<(), Box<dyn Error>> {
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
        &ServerMessage::BlobChunkRead {
            digest: fixture.wire_digest,
            offset_bytes,
            blob_length_bytes: fixture.expected_length,
            bytes: BlobChunk::new(Vec::new()),
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

/// A multi-gigabyte attached file contributes metadata without any body read.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_four_gib_attachment_preparation_reads_no_body() -> Result<(), Box<dyn Error>> {
    use signalbox_blob_store::{BlobStore, ExpectedBlob};
    use signalbox_blob_store_filesystem::FilesystemBlobStore;
    use signalbox_persistence::blob::{
        BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord,
    };
    use std::os::unix::fs::DirBuilderExt;

    const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
    // SHA-256 of FOUR_GIB zero bytes, computed with a bounded streaming buffer.
    let digest: BlobDigest =
        "sha256:8479e43911dc45e89f934fe48d01297e16f51d17aa561d4d1c216b1ae0fcddca".parse()?;
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let root = runtime.blob_storage_root.as_ref().expect("blob fixture");
    let key = BlobObjectKey::for_digest(digest);
    let path = root.store.join(key.as_str());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path.parent().expect("object parent"))?;
    let file = fs::File::create(&path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.set_len(FOUR_GIB)?;
    let measured = Arc::new(FilesystemBlobStore::try_new_for_conformance(
        root.store.clone(),
    )?);
    let configuration = support::parse_model_configuration(&root.model_configuration())?;
    let mut registry = BlobStoreRegistry::initialize_for_conformance(
        configuration.blob_storage(),
        runtime.pool.clone(),
    )
    .await?
    .expect("configured registry");
    let (name, _) = registry.routed_store(BlobStorageClass::UserAttachment);
    let name = name.clone();
    let expected = ExpectedBlob::try_new(digest, FOUR_GIB)?;
    BlobCatalogRepository::new(runtime.pool.clone())
        .register_verified_replica(
            expected,
            BlobStoreBindingRecord::new(name.clone(), registry.namespace_id(&name)),
            BlobReplicaRecord::new(name.clone(), key.clone()),
        )
        .await?;
    assert!(registry.replace_store_for_conformance(&name, measured.clone()));
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    connection
        .request(
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: CanonicalBlobDigest::from_digest(digest),
                    kind: UserAttachmentKind::File,
                    media_type: "application/octet-stream".into(),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    accepted_successor_turn(&mut connection, session_id, 1).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("sparse-blob-fixture"),
    );
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let mut prepared = None;
    for _ in 0..2 {
        prepared = Some(
            calls
                .prepare_initial_call(
                    session,
                    call,
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    ),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    |_| panic!("no steering"),
                )
                .await?,
        );
    }
    let Some(PrepareInitialModelCallOutcome::Ready {
        request,
        credential_reference,
        system_prompt,
        tool_entries,
        reasoning_provenance,
        ..
    }) = prepared
    else {
        panic!("prepared attachment call is ready");
    };
    let operation = PreparedModelOperation::render(
        *request,
        credential_reference,
        system_prompt,
        Box::new([]),
        &tool_entries,
        &reasoning_provenance,
    )?;
    let interactions = Arc::new(AtomicUsize::new(0));
    let counter = AttachmentPreparingModelCallProvider::new(
        super::compaction::CountingProbe {
            interactions: interactions.clone(),
            outcome: ModelCallInputTokenCount::Counted(1),
        },
        runtime.pool.clone(),
        Some(Arc::new(registry)),
    );
    assert_eq!(
        counter
            .count_input_tokens(operation, std::future::pending())
            .await?,
        ModelCallInputTokenCount::Counted(1)
    );
    assert_eq!(interactions.load(Ordering::SeqCst), 1);
    assert_eq!(measured.read_bytes_for_test(), 0);
    let page = measured
        .open_range(
            expected,
            &key,
            FOUR_GIB / 2,
            std::num::NonZeroU64::new(524_288).expect("page length"),
        )
        .await?;
    assert_eq!(page.byte_length(), 524_288);
    assert_eq!(measured.read_bytes_for_test(), 524_288);
    drop(connection);
    runtime.stop().await
}
