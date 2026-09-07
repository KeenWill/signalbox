use super::*;

pub(crate) enum PreparedImport {
    File(tokio::fs::File),
    Scan(PreparedImportScan),
}

pub(crate) struct PreparedImportScan {
    pub(crate) root: OwnedFd,
    pub(crate) paths: Vec<ScannedImportPath>,
}

pub(crate) struct ScannedImportPath {
    pub(crate) relative: PathBuf,
    display: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationImportOutcome {
    Inserted(CanonicalUuid),
    AlreadyImported(CanonicalUuid),
}

enum ConversationImportResponse {
    Begun(CanonicalU64),
    Appended(CanonicalU64),
    Inserted(CanonicalUuid),
    AlreadyImported(CanonicalUuid),
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

fn classify_conversation_import_response(message: ServerMessage) -> ConversationImportResponse {
    match message {
        ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        } => ConversationImportResponse::Begun(declared_size_bytes),
        ServerMessage::ConversationImportAppended {
            assembled_size_bytes,
        } => ConversationImportResponse::Appended(assembled_size_bytes),
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => ConversationImportResponse::Inserted(imported_conversation_id),
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id,
        } => ConversationImportResponse::AlreadyImported(imported_conversation_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => ConversationImportResponse::Error {
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
        | ServerMessage::ConversationImportAborted {}
        | ServerMessage::BlobUploadBegun { .. }
        | ServerMessage::BlobUploadAlreadyPresent { .. }
        | ServerMessage::BlobUploadAppended { .. }
        | ServerMessage::BlobUploadCommitted { .. }
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
        | ServerMessage::WorkspaceRegistered { .. }
        | ServerMessage::GitRemoteMinted { .. }
        | ServerMessage::GitRemoteWithdrawn { .. }
        | ServerMessage::OauthCredentialReceipt { .. } => ConversationImportResponse::Unexpected,
    }
}

#[derive(Default)]
pub(crate) struct ImportScanSummary {
    pub(crate) imported: usize,
    pub(crate) already_imported: usize,
    pub(crate) skipped: usize,
}

pub(crate) async fn open_import_source(path: &Path) -> Result<tokio::fs::File, ClientError> {
    tokio::fs::File::open(path)
        .await
        .map_err(ClientError::source_file)
}

pub(crate) async fn read_import_file(file: tokio::fs::File) -> Result<Vec<u8>, ClientError> {
    let read_limit = u64::try_from(MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol(
            "conversation import read bound overflow",
        ))?;
    let mut source = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut source)
        .await
        .map_err(ClientError::source_file)?;
    if source.len() > MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES {
        return Err(ClientError::SourceExceedsFrame);
    }
    Ok(source)
}

pub(crate) fn collect_import_paths(root: &Path) -> Result<PreparedImportScan, ClientError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(ClientError::scan_directory)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ClientError::Input("--scan requires a directory"));
    }

    let root_fd = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(ClientError::scan_directory)?;
    let root_directory = Dir::read_from(&root_fd)
        .map_err(std::io::Error::from)
        .map_err(ClientError::scan_directory)?;
    let mut pending = vec![(PathBuf::new(), root_directory)];
    let mut paths = Vec::new();
    while let Some((relative_directory, directory)) = pending.last_mut() {
        let Some(entry) = directory.read() else {
            pending.pop();
            continue;
        };
        let entry = entry
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name_bytes);
        let relative = relative_directory.join(name);
        let descriptor = directory
            .fd()
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let status = statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        match FileType::from_raw_mode(status.st_mode) {
            FileType::Directory => {
                let child = openat(
                    descriptor,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)
                .map_err(ClientError::scan_directory)?;
                let child = Dir::new(child)
                    .map_err(std::io::Error::from)
                    .map_err(ClientError::scan_directory)?;
                pending.push((relative, child));
            }
            FileType::RegularFile if relative.extension() == Some(OsStr::new("jsonl")) => {
                paths.push(ScannedImportPath {
                    display: root.join(&relative),
                    relative,
                });
            }
            FileType::RegularFile
            | FileType::Symlink
            | FileType::Fifo
            | FileType::Socket
            | FileType::CharacterDevice
            | FileType::BlockDevice
            | FileType::Unknown => {}
        }
    }
    paths.sort_by(|left, right| left.display.cmp(&right.display));
    Ok(PreparedImportScan {
        root: root_fd,
        paths,
    })
}

pub(crate) fn open_scanned_import_source(
    root: &OwnedFd,
    relative: &Path,
) -> Result<tokio::fs::File, ClientError> {
    let mut components = relative.components().peekable();
    let mut current = None;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(ClientError::Protocol(
                "scan produced a non-relative candidate path",
            ));
        };
        let parent = current.as_ref().unwrap_or(root);
        let flags = if components.peek().is_some() {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        current = Some(
            openat(parent, name, flags, Mode::empty())
                .map_err(std::io::Error::from)
                .map_err(ClientError::source_file)?,
        );
    }
    let descriptor = current.ok_or(ClientError::Protocol(
        "scan produced an empty candidate path",
    ))?;
    let status = fstat(&descriptor)
        .map_err(std::io::Error::from)
        .map_err(ClientError::source_file)?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(ClientError::source_file(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scan candidate is no longer a regular file",
        )));
    }
    Ok(tokio::fs::File::from_std(File::from(descriptor)))
}

pub(crate) fn source_fits_single_shot_import(
    format: ConversationImportFormat,
    source: &[u8],
    request_id: RequestId,
) -> Result<bool, ClientError> {
    if source.len() > MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES {
        return Ok(false);
    }
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ClientRequest::ImportConversation {
            format,
            source: ConversationImportSource::new(source.to_vec()),
        },
    )
    .map_err(FrameEncodeError::Validation)?;
    match encode_client_line(&frame) {
        Ok(_) => Ok(true),
        Err(FrameEncodeError::OversizedFrame) => Ok(false),
        Err(FrameEncodeError::Validation(error)) => {
            Err(ClientError::Encode(FrameEncodeError::Validation(error)))
        }
        Err(FrameEncodeError::Json(error)) => {
            Err(ClientError::Encode(FrameEncodeError::Json(error)))
        }
    }
}

pub(crate) async fn import_conversation_file(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    file: tokio::fs::File,
) -> Result<ConversationImportOutcome, ClientError> {
    let declared_size_bytes = file
        .metadata()
        .await
        .map_err(ClientError::source_file)?
        .len();
    if declared_size_bytes
        <= u64::try_from(MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES).unwrap_or(u64::MAX)
    {
        let source = read_import_file(file).await?;
        import_conversation_source(client, format, source).await
    } else {
        import_conversation_chunked(client, format, CanonicalU64::new(declared_size_bytes), file)
            .await
    }
}

async fn import_conversation_source(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    source: Vec<u8>,
) -> Result<ConversationImportOutcome, ClientError> {
    if source_fits_single_shot_import(format, &source, client.pending_request_id()?)? {
        import_conversation(client, format, source).await
    } else {
        let declared_size_bytes = u64::try_from(source.len())
            .map(CanonicalU64::new)
            .map_err(|_| ClientError::Protocol("import source size is not representable"))?;
        import_conversation_chunked(client, format, declared_size_bytes, source.as_slice()).await
    }
}

async fn import_conversation_chunked<Source>(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    declared_size_bytes: CanonicalU64,
    mut source: Source,
) -> Result<ConversationImportOutcome, ClientError>
where
    Source: tokio::io::AsyncRead + Unpin,
{
    let mut connection = client
        .setup_request(ClientRequest::BeginConversationImport {
            format,
            declared_size_bytes,
        })
        .await?;
    match classify_conversation_import_response(connection.message().await?) {
        ConversationImportResponse::Begun(admitted) if admitted == declared_size_bytes => {}
        ConversationImportResponse::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        ConversationImportResponse::Begun(_)
        | ConversationImportResponse::Appended(_)
        | ConversationImportResponse::Inserted(_)
        | ConversationImportResponse::AlreadyImported(_)
        | ConversationImportResponse::Unexpected => {
            return Err(ClientError::Protocol(
                "conversation import begin returned an unexpected response",
            ));
        }
    }

    let mut assembled_size_bytes = 0_u64;
    loop {
        let read_limit =
            conversation_import_chunk_read_limit(declared_size_bytes, assembled_size_bytes);
        if read_limit == 0 {
            break;
        }
        let mut chunk = Vec::with_capacity(MAX_CONVERSATION_IMPORT_CHUNK_BYTES);
        (&mut source)
            .take(read_limit)
            .read_to_end(&mut chunk)
            .await
            .map_err(ClientError::source_file)?;
        let chunk_size = chunk.len();
        if chunk_size == 0 {
            break;
        }
        assembled_size_bytes = assembled_size_bytes
            .checked_add(u64::try_from(chunk_size).map_err(|_| {
                ClientError::Protocol("conversation import chunk size is not representable")
            })?)
            .ok_or(ClientError::Protocol(
                "conversation import assembled size overflowed",
            ))?;
        client
            .continue_setup_request(
                &mut connection,
                ClientRequest::AppendConversationImport {
                    chunk: ConversationImportSource::new(chunk),
                },
            )
            .await?;
        match classify_conversation_import_response(connection.message().await?) {
            ConversationImportResponse::Appended(admitted)
                if admitted.value() == assembled_size_bytes => {}
            ConversationImportResponse::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            ConversationImportResponse::Begun(_)
            | ConversationImportResponse::Appended(_)
            | ConversationImportResponse::Inserted(_)
            | ConversationImportResponse::AlreadyImported(_)
            | ConversationImportResponse::Unexpected => {
                return Err(ClientError::Protocol(
                    "conversation import append returned an unexpected response",
                ));
            }
        }
    }

    client
        .continue_mutation_request(&mut connection, ClientRequest::CommitConversationImport {})
        .await?;
    match classify_conversation_import_response(
        connection.message().await.map_err(ClientError::mutation)?,
    ) {
        ConversationImportResponse::Inserted(imported_conversation_id) => Ok(
            ConversationImportOutcome::Inserted(imported_conversation_id),
        ),
        ConversationImportResponse::AlreadyImported(imported_conversation_id) => Ok(
            ConversationImportOutcome::AlreadyImported(imported_conversation_id),
        ),
        ConversationImportResponse::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        ConversationImportResponse::Begun(_)
        | ConversationImportResponse::Appended(_)
        | ConversationImportResponse::Unexpected => Err(ClientError::Protocol(
            "conversation import commit returned an unexpected response",
        )
        .mutation()),
    }
}

pub(crate) fn conversation_import_chunk_read_limit(
    declared_size_bytes: CanonicalU64,
    assembled_size_bytes: u64,
) -> u64 {
    declared_size_bytes
        .value()
        .saturating_add(1)
        .saturating_sub(assembled_size_bytes)
        .min(u64::try_from(MAX_CONVERSATION_IMPORT_CHUNK_BYTES).unwrap_or(u64::MAX))
}

async fn import_conversation(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    source: Vec<u8>,
) -> Result<ConversationImportOutcome, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ImportConversation {
            format,
            source: ConversationImportSource::new(source),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::Inserted(
            imported_conversation_id,
        )),
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::AlreadyImported(
            imported_conversation_id,
        )),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("import returned an unexpected response").mutation()),
    }
}

pub(crate) fn write_single_import_outcome(
    output: &mut Output<'_>,
    outcome: ConversationImportOutcome,
) -> Result<(), ClientError> {
    match outcome {
        ConversationImportOutcome::Inserted(imported_conversation_id) => {
            output.conversation_import_inserted(imported_conversation_id)?;
        }
        ConversationImportOutcome::AlreadyImported(imported_conversation_id) => {
            output.conversation_import_already_imported(imported_conversation_id)?;
        }
    }
    Ok(())
}

pub(crate) async fn scan_conversations(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    format: ConversationImportFormat,
    scan: PreparedImportScan,
) -> Result<(), ClientError> {
    let mut summary = ImportScanSummary::default();
    for path in scan.paths {
        let outcome = match open_scanned_import_source(&scan.root, &path.relative) {
            Ok(file) => import_conversation_file(client, format, file).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(ConversationImportOutcome::Inserted(imported_conversation_id)) => {
                summary.imported += 1;
                output
                    .conversation_import_scan_inserted(&path.display, imported_conversation_id)?;
            }
            Ok(ConversationImportOutcome::AlreadyImported(imported_conversation_id)) => {
                summary.already_imported += 1;
                output.conversation_import_scan_already_imported(
                    &path.display,
                    imported_conversation_id,
                )?;
            }
            Err(error) => {
                summary.skipped += 1;
                output.conversation_import_scan_skipped(&path.display, &error)?;
            }
        }
    }
    output.conversation_import_scan_summary(&summary)?;
    if summary.skipped == 0 {
        Ok(())
    } else {
        Err(ClientError::ScanIncomplete {
            skipped_files: summary.skipped,
        })
    }
}

/// Prints one imported conversation's selectable positions and its total.
///
/// The complete sequence is spooled and validated before presentation, so the
/// wire's intentionally unbounded entry sequence never becomes unbounded client
/// memory.
pub(crate) async fn imported(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    imported_conversation_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let entry_count = read_imported_conversation(client, imported_conversation_id, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::ImportedConversationEntry {
                position,
                imported_entry_id,
                source_speaker,
                content_kind,
                text_preview,
            } => output.imported_conversation_entry(&ImportedEntryRow {
                position: position.value(),
                imported_entry_id: *imported_entry_id,
                source_speaker: *source_speaker,
                content_kind: *content_kind,
                text_preview: text_preview.as_ref(),
            })?,
            _ => {
                return Err(ClientError::Protocol(
                    "imported-entry spool contained a non-entry frame",
                ));
            }
        }
        line.clear();
    }
    output.imported_conversation_entry_count(entry_count)?;
    Ok(())
}

/// Reads one imported conversation's complete entry sequence, returning its
/// validated entry count, which is also its greatest selectable position.
pub(crate) async fn read_imported_conversation(
    client: &mut ProcessClient,
    imported_conversation_id: CanonicalUuid,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<u64, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadImportedConversation {
            imported_conversation_id,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::ImportedConversationStart {
            imported_conversation_id: started,
        } if started == imported_conversation_id => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "imported conversation did not begin with its start frame",
            ));
        }
    }
    let mut entry_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::ImportedConversationEntry { position, .. } => {
                let expected = entry_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("imported entry count overflowed"))?;
                if position.value() != expected {
                    return Err(ClientError::Protocol(
                        "imported entry positions were not the contiguous sequence from one",
                    ));
                }
                consume(&frame)?;
                entry_count = expected;
            }
            ServerMessage::ImportedConversationEnd {
                imported_conversation_id: ended,
                entry_count: declared,
            } if *ended == imported_conversation_id && declared.value() == entry_count => {
                // An imported conversation's normalized entry sequence is
                // nonempty, so an empty inventory is never a valid read of one.
                if entry_count == 0 {
                    return Err(ClientError::Protocol(
                        "imported conversation reported no entries",
                    ));
                }
                return Ok(entry_count);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "imported conversation sequence or count was invalid",
                ));
            }
        }
    }
}
