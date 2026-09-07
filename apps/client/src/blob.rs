use super::*;

pub(crate) struct PreparedBlobSource {
    pub(crate) path: PathBuf,
    pub(crate) file: tokio::fs::File,
}

enum BlobUploadResponse {
    Begun {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    AlreadyPresent {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    Appended(CanonicalU64),
    Committed {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

fn classify_blob_upload_response(message: ServerMessage) -> BlobUploadResponse {
    match message {
        ServerMessage::BlobUploadBegun {
            expected_digest,
            expected_length_bytes,
        } => BlobUploadResponse::Begun {
            digest: expected_digest,
            byte_length: expected_length_bytes,
        },
        ServerMessage::BlobUploadAlreadyPresent {
            digest,
            byte_length,
        } => BlobUploadResponse::AlreadyPresent {
            digest,
            byte_length,
        },
        ServerMessage::BlobUploadAppended {
            assembled_length_bytes,
        } => BlobUploadResponse::Appended(assembled_length_bytes),
        ServerMessage::BlobUploadCommitted {
            digest,
            byte_length,
        } => BlobUploadResponse::Committed {
            digest,
            byte_length,
        },
        ServerMessage::Error {
            code,
            message,
            detail,
        } => BlobUploadResponse::Error {
            code,
            message,
            detail,
        },
        ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionCommissioned { .. }
        | ServerMessage::SessionLifecycleCommandApplied { .. }
        | ServerMessage::SessionSpawned { .. }
        | ServerMessage::SessionAwaitRegistered { .. }
        | ServerMessage::ChildResult { .. }
        | ServerMessage::SessionMessageSent { .. }
        | ServerMessage::SessionPlacementUpdated { .. }
        | ServerMessage::InputSubmitted { .. }
        | ServerMessage::SteeringSubmitted { .. }
        | ServerMessage::GoalTransitionApplied { .. }
        | ServerMessage::GoalHistoryStart { .. }
        | ServerMessage::GoalHistoryState { .. }
        | ServerMessage::GoalHistoryItem { .. }
        | ServerMessage::GoalHistoryEnd { .. }
        | ServerMessage::SessionsStart {}
        | ServerMessage::SessionSummary { .. }
        | ServerMessage::SessionsEnd { .. }
        | ServerMessage::OperatorStatus(..)
        | ServerMessage::TemplatesStart {}
        | ServerMessage::TemplateSummary { .. }
        | ServerMessage::TemplatesEnd { .. }
        | ServerMessage::SessionMetadataPageStart {}
        | ServerMessage::SessionMetadataSummary { .. }
        | ServerMessage::SessionMetadataPageEnd { .. }
        | ServerMessage::ConversationPageStart {}
        | ServerMessage::ConversationSummary { .. }
        | ServerMessage::ConversationPageEnd { .. }
        | ServerMessage::ModelAliasesStart {}
        | ServerMessage::ModelAliasSummary { .. }
        | ServerMessage::ModelAliasesEnd { .. }
        | ServerMessage::ModelCapabilitiesStart {}
        | ServerMessage::ModelCapabilityItem { .. }
        | ServerMessage::ModelCapabilitiesEnd { .. }
        | ServerMessage::SessionMetadata { .. }
        | ServerMessage::SessionMetadataReplaced { .. }
        | ServerMessage::SessionDefaultsReplaced { .. }
        | ServerMessage::SessionDefaults { .. }
        | ServerMessage::ToolRequestDecided { .. }
        | ServerMessage::ToolDenialOverridden { .. }
        | ServerMessage::SessionCompacted { .. }
        | ServerMessage::ConversationImportBegun { .. }
        | ServerMessage::ConversationImportAppended { .. }
        | ServerMessage::ConversationImportInserted { .. }
        | ServerMessage::ConversationImportAlreadyImported { .. }
        | ServerMessage::ConversationImportAborted {}
        | ServerMessage::BlobUploadAborted {}
        | ServerMessage::BlobMetadata { .. }
        | ServerMessage::BlobChunkRead { .. }
        | ServerMessage::ImportedConversationStart { .. }
        | ServerMessage::ImportedConversationEntry { .. }
        | ServerMessage::ImportedConversationEnd { .. }
        | ServerMessage::TranscriptSnapshotStart { .. }
        | ServerMessage::TranscriptTurn { .. }
        | ServerMessage::TranscriptModelCallUsage { .. }
        | ServerMessage::TranscriptModelCallsEnd { .. }
        | ServerMessage::TranscriptEntry { .. }
        | ServerMessage::TranscriptUserEntry { .. }
        | ServerMessage::TranscriptTextEntry { .. }
        | ServerMessage::TranscriptContent { .. }
        | ServerMessage::TranscriptSnapshotEnd { .. }
        | ServerMessage::SessionEvent { .. }
        | ServerMessage::ProviderTextDelta { .. }
        | ServerMessage::ReviewTargetCreated { .. }
        | ServerMessage::ReviewRunStarted { .. }
        | ServerMessage::ReviewPassActivated { .. }
        | ServerMessage::ReviewPassCompleted { .. }
        | ServerMessage::ReviewFindingsRecorded { .. }
        | ServerMessage::ReviewFindingEventRecorded { .. }
        | ServerMessage::ReviewExternalLinkReserved { .. }
        | ServerMessage::ReviewExternalLinkAttached { .. }
        | ServerMessage::ReviewTarget { .. }
        | ServerMessage::ReviewRun { .. }
        | ServerMessage::ReviewFinding { .. }
        | ServerMessage::ReviewFindingsStart { .. }
        | ServerMessage::ReviewFindingItem { .. }
        | ServerMessage::ReviewFindingsEnd { .. }
        | ServerMessage::ReviewOrchestrationStarted { .. }
        | ServerMessage::ReviewOrchestrationAdvanced { .. }
        | ServerMessage::ReviewOrchestration { .. }
        | ServerMessage::DeploymentLimits { .. }
        | ServerMessage::RunnerReplacementReceipt { .. }
        | ServerMessage::RunnerAbandonmentReceipt { .. }
        | ServerMessage::RunnerPromotionReceipt { .. }
        | ServerMessage::OauthCredentialAuthorization { .. }
        | ServerMessage::OauthCredentialReceipt { .. } => BlobUploadResponse::Unexpected,
    }
}

pub(crate) fn open_blob_source(path: &Path) -> Result<PreparedBlobSource, ClientError> {
    let descriptor = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ClientError::blob_source_file(path, source))?;
    let status = fstat(&descriptor)
        .map_err(std::io::Error::from)
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(ClientError::blob_source_file(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "blob upload source is not a regular file",
            ),
        ));
    }
    Ok(PreparedBlobSource {
        path: path.to_path_buf(),
        file: tokio::fs::File::from_std(File::from(descriptor)),
    })
}

pub(crate) async fn upload_blob(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    source: PreparedBlobSource,
) -> Result<(), ClientError> {
    let PreparedBlobSource { path, mut file } = source;
    let (expected_digest, expected_length_bytes) = hash_blob_source(&mut file, &path).await?;
    if expected_length_bytes.value() == 0 {
        return Err(ClientError::Input("blob source must be nonempty"));
    }
    let first = upload_blob_once(
        client,
        output,
        &mut file,
        &path,
        expected_digest,
        expected_length_bytes,
    )
    .await;
    match first {
        Err(error) if error.is_ambiguous_mutation() => {
            verify_blob_source_unchanged(&mut file, &path, expected_digest, expected_length_bytes)
                .await?;
            upload_blob_once(
                client,
                output,
                &mut file,
                &path,
                expected_digest,
                expected_length_bytes,
            )
            .await
        }
        result => result,
    }
}

async fn upload_blob_once(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    file: &mut tokio::fs::File,
    path: &Path,
    expected_digest: CanonicalBlobDigest,
    expected_length_bytes: CanonicalU64,
) -> Result<(), ClientError> {
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    let mut connection = client
        .setup_request(ClientRequest::BeginBlobUpload {
            expected_digest,
            expected_length_bytes,
        })
        .await?;
    match classify_blob_upload_response(connection.message().await?) {
        BlobUploadResponse::AlreadyPresent {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {
            verify_blob_source_unchanged(file, path, expected_digest, expected_length_bytes)
                .await?;
            output.blob_uploaded(
                digest,
                byte_length.value(),
                BlobUploadPresentation::AlreadyPresent,
            )?;
            return Ok(());
        }
        BlobUploadResponse::Begun {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {}
        BlobUploadResponse::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        BlobUploadResponse::AlreadyPresent { .. }
        | BlobUploadResponse::Begun { .. }
        | BlobUploadResponse::Appended(_)
        | BlobUploadResponse::Committed { .. }
        | BlobUploadResponse::Unexpected => {
            return Err(ClientError::Protocol(
                "blob upload begin returned an unexpected response",
            ));
        }
    }

    let mut assembled_length = 0_u64;
    loop {
        let mut chunk = Vec::with_capacity(MAX_BLOB_CHUNK_BYTES);
        (&mut *file)
            .take(u64::try_from(MAX_BLOB_CHUNK_BYTES).unwrap_or(u64::MAX))
            .read_to_end(&mut chunk)
            .await
            .map_err(|source| ClientError::blob_source_file(path, source))?;
        if chunk.is_empty() {
            break;
        }
        assembled_length = assembled_length
            .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                ClientError::Protocol("blob upload chunk length is not representable")
            })?)
            .ok_or(ClientError::Protocol("blob upload length overflowed"))?;
        client
            .continue_setup_request(
                &mut connection,
                ClientRequest::AppendBlobUpload {
                    chunk: BlobChunk::new(chunk),
                },
            )
            .await?;
        match classify_blob_upload_response(connection.message().await?) {
            BlobUploadResponse::Appended(admitted) if admitted.value() == assembled_length => {}
            BlobUploadResponse::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            BlobUploadResponse::Begun { .. }
            | BlobUploadResponse::AlreadyPresent { .. }
            | BlobUploadResponse::Appended(_)
            | BlobUploadResponse::Committed { .. }
            | BlobUploadResponse::Unexpected => {
                return Err(ClientError::Protocol(
                    "blob upload append returned an unexpected response",
                ));
            }
        }
    }

    client
        .continue_mutation_request(&mut connection, ClientRequest::CommitBlobUpload {})
        .await?;
    match classify_blob_upload_response(connection.message().await.map_err(ClientError::mutation)?)
    {
        BlobUploadResponse::Committed {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {
            output.blob_uploaded(
                digest,
                byte_length.value(),
                BlobUploadPresentation::Committed,
            )?;
            Ok(())
        }
        BlobUploadResponse::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        BlobUploadResponse::Begun { .. }
        | BlobUploadResponse::AlreadyPresent { .. }
        | BlobUploadResponse::Appended(_)
        | BlobUploadResponse::Committed { .. }
        | BlobUploadResponse::Unexpected => Err(ClientError::Protocol(
            "blob upload commit returned an unexpected response",
        )
        .mutation()),
    }
}

pub(crate) async fn read_blob_metadata(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    digest: CanonicalBlobDigest,
) -> Result<(), ClientError> {
    let mut connection = client
        .setup_request(ClientRequest::ReadBlobMetadata { digest })
        .await?;
    match connection.message().await? {
        ServerMessage::BlobMetadata {
            digest: returned_digest,
            byte_length,
            replica_count,
        } if returned_digest == digest => output
            .blob_metadata(digest, byte_length.value(), replica_count.value())
            .map_err(ClientError::from),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "blob metadata returned an unexpected response",
        )),
    }
}

pub(crate) async fn read_blob_chunk(
    client: &mut ProcessClient,
    digest: CanonicalBlobDigest,
    offset_bytes: CanonicalU64,
    length_bytes: CanonicalU64,
) -> Result<Vec<u8>, ClientError> {
    if !(1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value()) {
        return Err(ClientError::BlobReadLengthOutOfRange);
    }
    let mut connection = client
        .setup_request(ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes,
            length_bytes,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::BlobChunkRead {
            digest: returned_digest,
            offset_bytes: returned_offset,
            bytes,
        } if returned_digest == digest
            && returned_offset == offset_bytes
            && u64::try_from(bytes.as_bytes().len()) == Ok(length_bytes.value()) =>
        {
            Ok(bytes.into_bytes())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "blob range returned an unexpected response",
        )),
    }
}

pub(crate) async fn write_blob_output(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    fchmod(temporary.as_file(), Mode::RUSR | Mode::WUSR)
        .map_err(std::io::Error::from)
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    let mut file = tokio::fs::File::from_std(
        temporary
            .reopen()
            .map_err(|source| ClientError::blob_output_file(path, source))?,
    );
    file.write_all(bytes)
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    file.sync_all()
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    drop(file);
    temporary
        .persist_noclobber(path)
        .map_err(|error| ClientError::blob_output_file(path, error.error))?;
    tokio::fs::File::open(parent)
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?
        .sync_all()
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    Ok(())
}

pub(crate) async fn hash_blob_source(
    file: &mut tokio::fs::File,
    path: &Path,
) -> Result<(CanonicalBlobDigest, CanonicalU64), ClientError> {
    let mut digest = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; BLOB_HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| ClientError::blob_source_file(path, source))?;
        if read == 0 {
            break;
        }
        observed_length = observed_length
            .checked_add(u64::try_from(read).map_err(|_| {
                ClientError::Protocol("blob source read length is not representable")
            })?)
            .ok_or(ClientError::Protocol("blob source length overflowed"))?;
        digest.update(&buffer[..read]);
    }
    Ok((
        CanonicalBlobDigest::from_bytes(digest.finalize().into()),
        CanonicalU64::new(observed_length),
    ))
}

async fn verify_blob_source_unchanged(
    file: &mut tokio::fs::File,
    path: &Path,
    expected_digest: CanonicalBlobDigest,
    expected_length: CanonicalU64,
) -> Result<(), ClientError> {
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    let (actual_digest, actual_length) = hash_blob_source(file, path).await?;
    if actual_digest != expected_digest || actual_length != expected_length {
        return Err(ClientError::Input(
            "blob source changed after it was hashed",
        ));
    }
    Ok(())
}
