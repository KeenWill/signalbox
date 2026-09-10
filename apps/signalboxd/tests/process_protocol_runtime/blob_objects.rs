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

/// A sparse attachment stays bounded through preparation and 200 reads in one turn.
#[tokio::test]
async fn blob_ten_gib_preparation_and_two_hundred_reads_stay_bounded() -> Result<(), Box<dyn Error>>
{
    use signalbox_blob_store::{BlobStore, ExpectedBlob};
    use signalbox_blob_store_filesystem::FilesystemBlobStore;
    use signalbox_persistence::blob::{
        BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord,
    };
    use std::os::unix::fs::DirBuilderExt;

    const TEN_GIB: u64 = 10 * 1024 * 1024 * 1024;
    // SHA-256 of TEN_GIB zero bytes, computed with a bounded streaming buffer.
    let digest: BlobDigest =
        "sha256:732377e7f4a2abdc13ddfa1eb4c9c497fd2a2b294674d056cf51581b47dd586d".parse()?;
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
    file.set_len(TEN_GIB)?;
    let measured = Arc::new(FilesystemBlobStore::try_new(root.store.clone())?);
    let configuration = support::parse_model_configuration(&root.model_configuration())?;
    let mut registry =
        BlobStoreRegistry::initialize(configuration.blob_storage(), runtime.pool.clone())
            .await?
            .expect("configured registry");
    let (name, _) = registry.routed_store(BlobStorageClass::UserAttachment);
    let name = name.clone();
    let expected = ExpectedBlob::try_new(digest, TEN_GIB)?;
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
    let turn = accepted_successor_turn(&mut connection, session_id, 1).await?;
    let turn = TurnId::from_uuid(turn.into_uuid());
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("sparse-blob-fixture"),
    )
    .with_continuation_usage_limits(configuration.tool_continuation_usage_limits());
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
    let registry = Arc::new(registry);
    let interactions = Arc::new(AtomicUsize::new(0));
    let counter = AttachmentPreparingModelCallProvider::new(
        super::compaction::CountingProbe {
            interactions: interactions.clone(),
            outcome: ModelCallInputTokenCount::Counted(1),
        },
        runtime.pool.clone(),
        Some(registry.clone()),
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
            TEN_GIB / 2,
            std::num::NonZeroU64::new(524_288).expect("page length"),
        )
        .await?;
    assert_eq!(page.byte_length(), 524_288);
    assert_eq!(measured.read_bytes_for_test(), 524_288);
    let before_turn_reads = measured.read_bytes_for_test();
    execute_sparse_blob_turn(&calls, &runtime, session, turn, call, registry, expected).await?;
    // Five full pages, 194 one-byte pages, and one three-byte short tail.
    // Healthy, truncated, and deleted EOF candidates read no body bytes.
    assert_eq!(
        measured.read_bytes_for_test() - before_turn_reads,
        2_621_637
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_operator_range_crossing_eof_returns_the_short_tail() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"short tail").await?;
    let offset_bytes = CanonicalU64::new(6);
    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes,
                length_bytes: CanonicalU64::new(524_288),
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::BlobChunkRead {
            digest: fixture.wire_digest,
            blob_length_bytes: fixture.expected_length,
            offset_bytes,
            bytes: BlobChunk::new(b"tail".to_vec()),
        }
    );
    fixture.stop().await
}

async fn execute_sparse_blob_turn(
    calls: &PostgresModelCallRepository,
    runtime: &RunningRuntime,
    session: SessionId,
    turn: TurnId,
    mut call: ModelCallId,
    registry: Arc<BlobStoreRegistry>,
    expected: ExpectedBlob,
) -> Result<(), Box<dyn Error>> {
    use signalbox_application::{
        ToolExecutionService, ToolExecutionServiceOutcome, UuidV7ToolLoopIdGenerator,
    };
    use signalbox_domain::{ToolAttemptEnd, ToolResultContent, TurnAttemptId};
    let object_path = runtime
        .blob_storage_root
        .as_ref()
        .expect("blob fixture")
        .store
        .join(BlobObjectKey::for_digest(expected.digest()).as_str());
    let (catalog, executor) = signalboxd::BlobTools::try_new(
        signalbox_persistence::blob::BlobCatalogRepository::new(runtime.pool.clone()),
        Some(registry),
    )?
    .into_parts();
    let mut tools = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        calls.tool_loop_repository(),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    for round in 0..28 {
        let AuthorizeModelCallOutcome::Authorized(authorized) =
            calls.authorize_send(session, call).await?
        else {
            panic!("sparse blob model call authorizes");
        };
        let response = ToolUsingAssistantResponse::try_from_parts((0..8).map(|page| {
            let index = round * 8 + page;
            let offset = if round >= 25 { if page % 2 == 0 { expected.byte_length() } else { u64::MAX } } else if index == 199 { expected.byte_length() - 3 } else { expected.byte_length() / 2 + index };
            let length = if index < 5 || index == 199 { 524_288 } else { 1 };
            AssistantResponsePart::ToolCall(ToolCallProposal::new(
                ToolName::try_new("blob_read".into()).expect("tool name"),
                NormalizedToolArguments::try_from_provider_text(format!(r#"{{"digest":"{}","offset_bytes":"{offset}","length_bytes":"{length}"}}"#, expected.digest())).expect("range arguments"),
            ))
        }).collect()).expect("eight tool requests");
        let observation = authorized
            .observation_correlation()
            .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                response,
                retained_input_tokens: None,
                retained_output_tokens: None,
            });
        calls
            .apply_terminal_observation(
                session,
                observation,
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    (0..8)
                        .map(|_| {
                            ToolResponsePartIdentity::tool_call(
                                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                                ToolRequestId::from_uuid(Uuid::now_v7()),
                                InitialToolApproval::PolicyAuto,
                            )
                        })
                        .collect(),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    Some(TurnAttemptId::from_uuid(Uuid::now_v7())),
                )),
                |_| panic!("no pending steering"),
            )
            .await?;
        // Mutate after admission so failures come from the model read itself.
        if round == 26 {
            fs::OpenOptions::new()
                .write(true)
                .open(&object_path)?
                .set_len(expected.byte_length() - 1)?;
        } else if round == 27 {
            fs::remove_file(&object_path)?;
        }
        for page in 0..8 {
            assert!(matches!(
                tools.execute(session, turn).await?,
                ToolExecutionServiceOutcome::AttemptCheckpointed(_)
            ));
            let ToolExecutionServiceOutcome::ObservationCommitted(ended) =
                tools.execute(session, turn).await?
            else {
                panic!("range request completes");
            };
            if round >= 26 {
                let ToolAttemptEnd::KnownFailed { error } = ended.end() else {
                    panic!("EOF read detects the damaged replica: {:?}", ended.end());
                };
                assert_eq!(
                    error.detail().expect("blob failure detail").as_str(),
                    if round == 26 {
                        "blob_corrupt"
                    } else {
                        "blob_missing"
                    }
                );
                continue;
            }
            let ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(text),
            } = ended.end()
            else {
                panic!("page read succeeds: {:?}", ended.end());
            };
            let result: serde_json::Value = serde_json::from_str(text.as_str())?;
            assert_eq!(result["blob_length_bytes"], "10737418240");
            if round == 25 {
                assert_eq!(result["bytes_base64"], "", "healthy EOF read is empty");
            }
            if round == 24 && page == 7 {
                assert_eq!(
                    result["bytes_base64"], "AAAA",
                    "end-crossing page returns the three-byte tail"
                );
            }
        }
        let ToolExecutionServiceOutcome::ContinuationCheckpointed(next) =
            tools.execute(session, turn).await?
        else {
            panic!("completed pages continue the same turn");
        };
        call = next;
    }
    Ok(())
}

struct FailedVerifiedDeliveryStore {
    inner: Arc<dyn BlobStore>,
    fail_after: AtomicUsize,
    verified_opens: AtomicUsize,
}

impl BlobStore for FailedVerifiedDeliveryStore {
    fn put<'a>(
        &'a self,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome> {
        self.inner.put(expected, source)
    }

    fn open<'a>(&'a self, key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
        self.inner.open(key)
    }

    fn open_verified<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move {
            let opened = self.inner.open_verified(expected, key).await?;
            self.verified_opens.fetch_add(1, Ordering::SeqCst);
            let prefix = opened
                .into_reader()
                .take(self.fail_after.load(Ordering::SeqCst) as u64);
            Ok(OpenedBlob::new(
                expected.byte_length(),
                Box::new(prefix.chain(FailedDeliveryReader)),
            ))
        })
    }

    fn open_range<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        self.inner.open_range(expected, key, offset, byte_length)
    }
}

struct FailedDeliveryReader;

impl tokio::io::AsyncRead for FailedDeliveryReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Err(io::Error::other("fixture verified delivery failure")))
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_verified_delivery_retries_after_skip_and_page_failures() -> Result<(), Box<dyn Error>>
{
    use signalbox_blob_store::BlobStoreName;
    use signalbox_persistence::blob::{
        BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord,
    };

    let mut fixture = CommittedBlobReadFixture::start(b"verified replica fallback").await?;
    let secondary_root = tempfile::TempDir::new()?;
    fs::set_permissions(secondary_root.path(), fs::Permissions::from_mode(0o700))?;
    let root = fixture
        .runtime
        .blob_storage_root
        .as_ref()
        .expect("blob fixture");
    let configuration_text = format!(
        "{}\n[[blob_storage.stores]]\nname = \"secondary\"\nnamespace_id = \"5a100001-0000-4000-8000-000000000002\"\nkind = \"filesystem\"\nroot_directory = \"{}\"\n",
        root.model_configuration(),
        secondary_root.path().display(),
    );
    let configuration = support::parse_model_configuration(&configuration_text)?;
    let mut registry =
        BlobStoreRegistry::initialize(configuration.blob_storage(), fixture.runtime.pool.clone())
            .await?
            .expect("configured registry");
    let primary_name = BlobStoreName::try_new("primary")?;
    let primary = Arc::new(FailedVerifiedDeliveryStore {
        inner: registry
            .recorded_store(&primary_name)
            .expect("primary store"),
        fail_after: AtomicUsize::new(1),
        verified_opens: AtomicUsize::new(0),
    });
    assert!(registry.replace_store_for_conformance(&primary_name, primary.clone()));
    let secondary_name = BlobStoreName::try_new("secondary")?;
    let secondary = registry
        .recorded_store(&secondary_name)
        .expect("secondary store");
    let expected = ExpectedBlob::try_new(fixture.digest, fixture.expected_length.value())?;
    let published = secondary
        .put(expected, Box::new(io::Cursor::new(fixture.bytes)))
        .await?;
    BlobCatalogRepository::new(fixture.runtime.pool.clone())
        .register_verified_replica(
            expected,
            BlobStoreBindingRecord::new(
                secondary_name.clone(),
                registry.namespace_id(&secondary_name),
            ),
            BlobReplicaRecord::new(secondary_name, published.key().clone()),
        )
        .await?;
    fixture.runtime.blob_store_registry = Some(Arc::new(registry));
    fixture
        .runtime
        .restart_with_model_configuration(&configuration_text)
        .await?;
    fixture.connection = Connection::connect(fixture.runtime.socket()).await?;

    for fail_after in [1, 5] {
        primary.fail_after.store(fail_after, Ordering::SeqCst);
        fixture
            .connection
            .request(
                4,
                ClientRequest::ReadBlobChunk {
                    digest: fixture.wire_digest,
                    offset_bytes: CanonicalU64::new(4),
                    length_bytes: CanonicalU64::new(4),
                },
            )
            .await?;
        assert_eq!(
            fixture.connection.response().await?.message(),
            &ServerMessage::BlobChunkRead {
                blob_length_bytes: fixture.expected_length,
                digest: fixture.wire_digest,
                offset_bytes: CanonicalU64::new(4),
                bytes: BlobChunk::new(fixture.bytes[4..8].to_vec()),
            },
        );
    }
    assert_eq!(primary.verified_opens.load(Ordering::SeqCst), 2);
    fixture.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn blob_attachment_without_storage_is_unavailable_without_claiming_command()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let command_id = command()?;
    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: CanonicalBlobDigest::from_digest(BlobDigest::digest(b"attachment")),
                    kind: UserAttachmentKind::File,
                    media_type: String::from("application/octet-stream"),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::Unavailable,
            message: String::from("the requested operation is unavailable"),
            detail: ErrorDetail::none(),
        },
    );
    let claimed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(command_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(claimed, 0);

    connection
        .request(
            3,
            ClientRequest::SubmitInput {
                command_id,
                session_id,
                content: UserInputContent::text(String::from("text remains available")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert_eq!(
        submitted_session(connection.response().await?.message()),
        session_id
    );
    drop(connection);
    runtime.stop().await
}
