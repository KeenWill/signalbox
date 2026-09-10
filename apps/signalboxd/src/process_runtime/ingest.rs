use super::*;

pub(super) async fn write_import_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    detail: RejectionDetail,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_import(detail),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_begin_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    format: ConversationImportFormat,
    declared_size_bytes: CanonicalU64,
    import_permit: Option<OwnedSemaphorePermit>,
    acquired_bulk_ingest_at: Option<Instant>,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.is_some() {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportAlreadyInProgress {},
        )
        .await;
    }
    let import_permit = import_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let started_at = acquired_bulk_ingest_at.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let source = match tempfile::tempfile() {
        Ok(source) => tokio::fs::File::from_std(source),
        Err(_) => {
            drop(import_permit);
            return write_error(
                writer,
                version,
                request_id,
                unavailable_protocol_error(InternalDiagnostic::ConversationImportSpoolUnavailable),
            )
            .await;
        }
    };
    *pending = Some(PendingConversationImport {
        format,
        declared_size_bytes: declared_size_bytes.value(),
        actual_size_bytes: 0,
        source,
        import_permit,
        started_at,
        idle_since: started_at,
    });
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        },
    )
    .await?;
    if let Some(active_import) = pending.as_mut() {
        active_import.idle_since = Instant::now();
    }
    Ok(())
}

pub(super) async fn handle_append_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    chunk: Vec<u8>,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(active_import) = pending.as_mut() else {
        drop(chunk);
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    };
    let chunk_size =
        u64::try_from(chunk.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    active_import.actual_size_bytes = active_import
        .actual_size_bytes
        .checked_add(chunk_size)
        .ok_or(ProcessConnectionError::EncodeInvariant)?;
    if active_import.source.write_all(&chunk).await.is_err() {
        drop(chunk);
        drop(pending.take());
        return write_error(
            writer,
            version,
            request_id,
            unavailable_protocol_error(InternalDiagnostic::ConversationImportSpoolUnavailable),
        )
        .await;
    }
    drop(chunk);
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportAppended {
            assembled_size_bytes: CanonicalU64::new(active_import.actual_size_bytes),
        },
    )
    .await?;
    active_import.idle_since = Instant::now();
    Ok(())
}

pub(super) async fn handle_commit_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    repository: ImportedConversationRepository,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(pending) = pending.take() else {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    };
    let declared_size_bytes = CanonicalU64::new(pending.declared_size_bytes);
    let actual_size_bytes = CanonicalU64::new(pending.actual_size_bytes);
    if pending.actual_size_bytes != pending.declared_size_bytes {
        let detail = RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        };
        drop(pending);
        return write_import_rejection(writer, version, request_id, detail).await;
    }
    let PendingConversationImport {
        format,
        declared_size_bytes: _,
        actual_size_bytes,
        mut source,
        import_permit,
        started_at: _,
        idle_since: _,
    } = pending;
    if source.flush().await.is_err() {
        return write_error(
            writer,
            version,
            request_id,
            unavailable_protocol_error(InternalDiagnostic::ConversationImportSpoolUnavailable),
        )
        .await;
    }
    let observed_source_size = match source.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(_) => {
            return write_error(
                writer,
                version,
                request_id,
                unavailable_protocol_error(InternalDiagnostic::ConversationImportSpoolUnavailable),
            )
            .await;
        }
    };
    if observed_source_size != actual_size_bytes {
        return write_error(
            writer,
            version,
            request_id,
            internal_protocol_error(None, InternalDiagnostic::ConversationImportContractDefect),
        )
        .await;
    }
    if source.seek(SeekFrom::Start(0)).await.is_err() {
        return write_error(
            writer,
            version,
            request_id,
            unavailable_protocol_error(InternalDiagnostic::ConversationImportSpoolUnavailable),
        )
        .await;
    }
    let source = source.into_std().await;
    handle_import_conversation(
        writer,
        version,
        request_id,
        format,
        ConversationImportSource::Spooled(source),
        repository,
        import_permit,
    )
    .await
}

pub(super) async fn handle_abort_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.take().is_none() {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportAborted {},
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the lifecycle boundary keeps request correlation, resource ownership, and state explicit"
)]
pub(super) async fn handle_begin_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    expected_digest: CanonicalBlobDigest,
    expected_length_bytes: CanonicalU64,
    bulk_permit: Option<OwnedSemaphorePermit>,
    acquired_bulk_ingest_at: Option<Instant>,
    services: &ConnectionServices,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(registry) = services.blob_store_registry.as_deref() else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    if let Some(detail) = blob_upload_begin_preflight(
        pending.is_some(),
        expected_length_bytes,
        registry.max_blob_bytes(),
    ) {
        return write_blob_rejection(writer, version, request_id, detail).await;
    }
    let expected =
        ExpectedBlob::try_new(expected_digest.into_digest(), expected_length_bytes.value())
            .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    let bulk_permit = bulk_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let started_at = acquired_bulk_ingest_at.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let repository = BlobCatalogRepository::new(services.pool.clone());
    match begin_blob_upload(registry, &repository, expected, bulk_permit, started_at).await {
        Ok(BeginBlobUploadOutcome::Begun(upload)) => {
            *pending = Some(*upload);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadBegun {
                    expected_digest,
                    expected_length_bytes,
                },
            )
            .await?;
            if let Some(upload) = pending.as_mut() {
                upload.mark_activity_complete();
            }
            Ok(())
        }
        Ok(BeginBlobUploadOutcome::AlreadyPresent(expected)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadAlreadyPresent {
                    digest: CanonicalBlobDigest::from_digest(expected.digest()),
                    byte_length: CanonicalU64::new(expected.byte_length()),
                },
            )
            .await
        }
        Err(error) => write_blob_upload_error(writer, version, request_id, expected, error).await,
    }
}

pub(super) fn blob_upload_begin_preflight(
    upload_is_active: bool,
    expected_length_bytes: CanonicalU64,
    max_blob_bytes: u64,
) -> Option<RejectionDetail> {
    if upload_is_active {
        Some(RejectionDetail::BlobUploadAlreadyInProgress {})
    } else if !(1..=max_blob_bytes).contains(&expected_length_bytes.value()) {
        Some(RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes: CanonicalU64::new(1),
            max_length_bytes: CanonicalU64::new(max_blob_bytes),
            declared_length_bytes: expected_length_bytes,
        })
    } else {
        None
    }
}

pub(super) async fn handle_append_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    chunk: Vec<u8>,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(mut upload) = pending.take() else {
        drop(chunk);
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    };
    let expected = upload.expected();
    match upload.append(&chunk).await {
        Ok(assembled_length_bytes) => {
            *pending = Some(upload);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadAppended {
                    assembled_length_bytes: CanonicalU64::new(assembled_length_bytes),
                },
            )
            .await?;
            if let Some(upload) = pending.as_mut() {
                upload.mark_activity_complete();
            }
            Ok(())
        }
        Err(error) => {
            drop(upload);
            write_blob_upload_error(writer, version, request_id, expected, error).await
        }
    }
}

pub(super) async fn handle_commit_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    services: &ConnectionServices,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(upload) = pending.take() else {
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    };
    let expected = upload.expected();
    let repository = BlobCatalogRepository::new(services.pool.clone());
    match upload.commit(&repository).await {
        Ok(committed) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadCommitted {
                    digest: CanonicalBlobDigest::from_digest(committed.digest()),
                    byte_length: CanonicalU64::new(committed.byte_length()),
                },
            )
            .await
        }
        Err(error) => write_blob_upload_error(writer, version, request_id, expected, error).await,
    }
}

pub(super) async fn handle_abort_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.take().is_none() {
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::BlobUploadAborted {},
    )
    .await
}

pub(super) async fn handle_read_blob_metadata<Writer>(
    reader: &ClientReader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: CanonicalBlobDigest,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if services.blob_store_registry.is_none() {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    }
    let deadline = Instant::now() + BLOB_READ_TIMEOUT;
    let snapshot_permit = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => return Ok(()),
        () = wait_for_connection_loss(reader) => return Ok(()),
        () = sleep_until(deadline) => return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        ).await,
        permit = Arc::clone(&services.snapshot_reader_budget).acquire_owned() => permit
            .map_err(|_| ProcessConnectionError::SnapshotReaderBudgetClosed)?,
    };
    let repository = BlobCatalogRepository::new(services.pool.clone());
    let outcome = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => return Ok(()),
        () = wait_for_connection_loss(reader) => return Ok(()),
        outcome = tokio::time::timeout_at(
            deadline,
            read_blob_metadata(&repository, digest.into_digest()),
        ) => outcome.unwrap_or(Err(BlobReadError::Unavailable)),
    };
    drop(snapshot_permit);
    match outcome {
        Ok(metadata) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobMetadata {
                    digest,
                    byte_length: CanonicalU64::new(metadata.byte_length),
                    replica_count: CanonicalU64::new(metadata.replica_count),
                },
            )
            .await
        }
        Err(error) => write_blob_read_error(writer, version, request_id, None, error).await,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the wire boundary keeps correlation and exact range facts explicit"
)]
pub(super) async fn handle_read_blob_chunk<Writer>(
    reader: &ClientReader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: CanonicalBlobDigest,
    offset_bytes: CanonicalU64,
    length_bytes: CanonicalU64,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(registry) = services.blob_store_registry.as_deref() else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    if !(1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value()) {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::invalid_blob_read(RejectionDetail::BlobReadLengthOutOfRange {
                min_length_bytes: CanonicalU64::new(1),
                max_length_bytes: CanonicalU64::new(MAX_BLOB_READ_BYTES as u64),
                requested_length_bytes: length_bytes,
            }),
        )
        .await;
    }
    let Some(length) = NonZeroU64::new(length_bytes.value()) else {
        return Err(ProcessConnectionError::EncodeInvariant);
    };
    let deadline = Instant::now() + BLOB_READ_TIMEOUT;
    let snapshot_permit = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => return Ok(()),
        () = wait_for_connection_loss(reader) => return Ok(()),
        () = sleep_until(deadline) => return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        ).await,
        permit = Arc::clone(&services.snapshot_reader_budget).acquire_owned() => permit
            .map_err(|_| ProcessConnectionError::SnapshotReaderBudgetClosed)?,
    };
    let repository = BlobCatalogRepository::new(services.pool.clone());
    let entry = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => return Ok(()),
        () = wait_for_connection_loss(reader) => return Ok(()),
        outcome = tokio::time::timeout_at(
            deadline,
            read_blob_entry(&repository, digest.into_digest()),
        ) => outcome.unwrap_or(Err(BlobReadError::Unavailable)),
    };
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
            drop(snapshot_permit);
            return write_blob_read_error(
                writer,
                version,
                request_id,
                Some((offset_bytes, length_bytes)),
                error,
            )
            .await;
        }
    };
    if offset_bytes
        .value()
        .checked_add(length.get())
        .is_none_or(|end| end > entry.expected().byte_length())
    {
        drop(snapshot_permit);
        return write_blob_read_error(
            writer,
            version,
            request_id,
            Some((offset_bytes, length_bytes)),
            BlobReadError::RangeOutOfBounds {
                blob_length: entry.expected().byte_length(),
            },
        )
        .await;
    }
    drop(snapshot_permit);
    let Some(permit) = try_acquire_blob_read_permit(Arc::clone(&services.blob_read_budget)) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    let traversal = tokio::time::timeout_at(
        deadline,
        read_blob_chunk(registry, &entry, offset_bytes.value(), length),
    );
    let outcome = tokio::select! {
        biased;
        () = wait_for_shutdown(&mut shutdown) => return Ok(()),
        () = wait_for_connection_loss(reader) => return Ok(()),
        outcome = traversal => outcome.unwrap_or(Err(BlobReadError::Unavailable)),
    };
    match outcome {
        Ok(bytes) => {
            let spool = tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                () = wait_for_connection_loss(reader) => return Ok(()),
                () = sleep_until(deadline) => {
                    drop(permit);
                    return write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::Unavailable),
                    ).await;
                }
                spool = spool_single_message(
                    version,
                    request_id,
                    ServerMessage::BlobChunkRead {
                        digest,
                        offset_bytes,
                        bytes: BlobChunk::new(bytes),
                    },
                ) => spool,
            };
            drop(permit);
            let mut spool = match spool {
                Ok(spool) => spool,
                Err(error) => {
                    return write_snapshot_spool_error(writer, version, request_id, error).await;
                }
            };
            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => Ok(()),
                result = write_spooled_file(writer, &mut spool) => result,
            }
        }
        Err(error) => {
            drop(permit);
            write_blob_read_error(
                writer,
                version,
                request_id,
                Some((offset_bytes, length_bytes)),
                error,
            )
            .await
        }
    }
}

pub(super) async fn wait_for_connection_loss(reader: &ClientReader) {
    loop {
        let Ok(readiness) = reader
            .get_ref()
            .ready(Interest::READABLE | Interest::WRITABLE)
            .await
        else {
            return;
        };
        if readiness.is_read_closed() && readiness.is_write_closed() {
            return;
        }
        // Unconsumed pipelined bytes and an orderly write-half close keep the
        // socket ready, so back off before checking for full closure again.
        sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn write_blob_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    range: Option<(CanonicalU64, CanonicalU64)>,
    error: BlobReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        BlobReadError::NotFound => ProtocolError::blob_not_found(),
        BlobReadError::RangeOutOfBounds { blob_length } => {
            let Some((offset_bytes, length_bytes)) = range else {
                return Err(ProcessConnectionError::EncodeInvariant);
            };
            ProtocolError::invalid_blob_read(RejectionDetail::BlobReadRangeOutOfBounds {
                offset_bytes,
                length_bytes,
                blob_length_bytes: CanonicalU64::new(blob_length),
            })
        }
        BlobReadError::Missing => ProtocolError::without_detail(ErrorCode::BlobMissing),
        BlobReadError::Corrupt => ProtocolError::without_detail(ErrorCode::BlobCorrupt),
        BlobReadError::Unavailable => ProtocolError::without_detail(ErrorCode::Unavailable),
        BlobReadError::Integrity => {
            internal_protocol_error(None, InternalDiagnostic::BlobReadIntegrity)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

pub(super) async fn write_blob_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    detail: RejectionDetail,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_blob_upload(detail),
    )
    .await
}

pub(super) async fn write_bulk_ingest_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    active_kind: BulkIngestKind,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_bulk_ingest(RejectionDetail::BulkIngestAlreadyInProgress {
            active_kind,
        }),
    )
    .await
}

pub(super) async fn write_blob_upload_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    expected: ExpectedBlob,
    error: BlobUploadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        BlobUploadError::SizeExceeded { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadSizeExceeded {
                expected_length_bytes: CanonicalU64::new(expected.byte_length()),
                actual_length_bytes: CanonicalU64::new(observed),
            })
        }
        BlobUploadError::LengthMismatch { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadLengthMismatch {
                expected_length_bytes: CanonicalU64::new(expected.byte_length()),
                actual_length_bytes: CanonicalU64::new(observed),
            })
        }
        BlobUploadError::DigestMismatch { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadDigestMismatch {
                expected_digest: CanonicalBlobDigest::from_digest(expected.digest()),
                actual_digest: CanonicalBlobDigest::from_digest(observed),
            })
        }
        BlobUploadError::Unavailable => ProtocolError::without_detail(ErrorCode::Unavailable),
        BlobUploadError::PublicationAmbiguous => {
            ProtocolError::without_detail(ErrorCode::PublicationAmbiguous)
        }
        BlobUploadError::CommitAmbiguous => {
            ProtocolError::without_detail(ErrorCode::CommitAmbiguous)
        }
        BlobUploadError::Integrity => ProtocolError::without_detail(ErrorCode::Internal),
    };
    write_error(writer, version, request_id, protocol_error).await
}

pub(super) async fn handle_import_conversation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    format: ConversationImportFormat,
    source: ConversationImportSource,
    repository: ImportedConversationRepository,
    import_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let outcome = match format {
        ConversationImportFormat::ClaudeCodeSessionJsonlV3 => {
            execute_import(ResilientClaudeCodeJsonlConverter, source, repository).await
        }
        ConversationImportFormat::CodexRolloutJsonlV2 => {
            execute_import(ResilientCodexRolloutJsonlConverter, source, repository).await
        }
    };
    drop(import_permit);
    match outcome {
        Ok(CompletedImport {
            outcome: ImportConversationOutcome::Inserted { conversation },
            dropped_records,
        }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportInserted {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                    dropped_record_count: CanonicalU64::new(dropped_records.count()),
                    first_dropped_record_position: dropped_records
                        .first_source_line()
                        .map(CanonicalU64::new),
                },
            )
            .await
        }
        Ok(CompletedImport {
            outcome: ImportConversationOutcome::AlreadyImported { conversation },
            dropped_records,
        }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportAlreadyImported {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                    dropped_record_count: CanonicalU64::new(dropped_records.count()),
                    first_dropped_record_position: dropped_records
                        .first_source_line()
                        .map(CanonicalU64::new),
                },
            )
            .await
        }
        Err(OperationalImportError::InvalidSource(evidence)) => {
            write_import_rejection(
                writer,
                version,
                request_id,
                RejectionDetail::ConversationImportConversionFailed {
                    class: evidence.class,
                    record_ordinal: evidence.record_ordinal.map(CanonicalU64::new),
                },
            )
            .await
        }
        Err(OperationalImportError::Database) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::CommitAmbiguous),
            )
            .await
        }
        Err(OperationalImportError::Unavailable) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(OperationalImportError::Internal(diagnostic)) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await
        }
    }
}

pub(super) enum ConversationImportSource {
    Inline(Vec<u8>),
    Spooled(std::fs::File),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ImportRejectionEvidence {
    class: ConversationImportRejectionClass,
    record_ordinal: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OperationalImportError {
    InvalidSource(ImportRejectionEvidence),
    Database,
    Unavailable,
    Internal(InternalDiagnostic),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConversionFailureDisposition {
    Rejected(ImportRejectionEvidence),
    Internal,
}

pub(super) trait ClassifyConversationImportError {
    fn disposition(self) -> ConversionFailureDisposition;
}

pub(super) trait ClassifyConversationImportRecordFailure {
    fn disposition(self) -> ConversionFailureDisposition;
}

impl ClassifyConversationImportError for ClaudeCodeJsonlConversionError {
    fn disposition(self) -> ConversionFailureDisposition {
        claude_conversion_failure_disposition(self.failure())
    }
}

impl ClassifyConversationImportRecordFailure for ClaudeCodeJsonlConversionFailure {
    fn disposition(self) -> ConversionFailureDisposition {
        claude_conversion_failure_disposition(self)
    }
}

pub(super) fn claude_conversion_failure_disposition(
    failure: ClaudeCodeJsonlConversionFailure,
) -> ConversionFailureDisposition {
    use ClaudeCodeJsonlConversionFailure as Failure;
    let evidence = match failure {
        Failure::EmptySource => {
            import_evidence(ConversationImportRejectionClass::EmptySource, None)
        }
        Failure::BlankLine { line } => {
            import_evidence(ConversationImportRejectionClass::BlankLine, Some(line))
        }
        Failure::InvalidUtf8 { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidUtf8, Some(line))
        }
        Failure::InvalidJson { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidJson, Some(line))
        }
        Failure::JsonDepthExceeded { line } => import_evidence(
            ConversationImportRejectionClass::JsonDepthExceeded,
            Some(line),
        ),
        Failure::RawRecordTooLarge { line } => import_evidence(
            ConversationImportRejectionClass::RawRecordTooLarge,
            Some(line),
        ),
        Failure::TopLevelNotObject { line } => import_evidence(
            ConversationImportRejectionClass::TopLevelNotObject,
            Some(line),
        ),
        Failure::InvalidRecordType { line } => import_evidence(
            ConversationImportRejectionClass::InvalidRecordType,
            Some(line),
        ),
        Failure::InvalidSourceMetadata { line } => import_evidence(
            ConversationImportRejectionClass::InvalidSourceMetadata,
            Some(line),
        ),
        Failure::InvalidMessageEnvelope { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageEnvelope,
            Some(line),
        ),
        Failure::InvalidMessageRole { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageRole,
            Some(line),
        ),
        Failure::MessageRoleMismatch { line } => import_evidence(
            ConversationImportRejectionClass::MessageRoleMismatch,
            Some(line),
        ),
        Failure::InvalidMessageContent { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageContent,
            Some(line),
        ),
        Failure::InvalidContentBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidContentBlock,
            Some(line),
        ),
        Failure::InvalidToolResultBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResultBlock,
            Some(line),
        ),
        Failure::PositionExhausted | Failure::InvalidAggregate(_) => {
            return ConversionFailureDisposition::Internal;
        }
    };
    ConversionFailureDisposition::Rejected(evidence)
}

impl ClassifyConversationImportError for CodexRolloutJsonlConversionError {
    fn disposition(self) -> ConversionFailureDisposition {
        codex_conversion_failure_disposition(self.failure())
    }
}

impl ClassifyConversationImportRecordFailure for CodexRolloutJsonlConversionFailure {
    fn disposition(self) -> ConversionFailureDisposition {
        codex_conversion_failure_disposition(self)
    }
}

pub(super) fn codex_conversion_failure_disposition(
    failure: CodexRolloutJsonlConversionFailure,
) -> ConversionFailureDisposition {
    use CodexRolloutJsonlConversionFailure as Failure;
    let evidence = match failure {
        Failure::EmptySource => {
            import_evidence(ConversationImportRejectionClass::EmptySource, None)
        }
        Failure::BlankLine { line } => {
            import_evidence(ConversationImportRejectionClass::BlankLine, Some(line))
        }
        Failure::InvalidUtf8 { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidUtf8, Some(line))
        }
        Failure::InvalidJson { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidJson, Some(line))
        }
        Failure::JsonDepthExceeded { line } => import_evidence(
            ConversationImportRejectionClass::JsonDepthExceeded,
            Some(line),
        ),
        Failure::RawRecordTooLarge { line } => import_evidence(
            ConversationImportRejectionClass::RawRecordTooLarge,
            Some(line),
        ),
        Failure::TopLevelNotObject { line } => import_evidence(
            ConversationImportRejectionClass::TopLevelNotObject,
            Some(line),
        ),
        Failure::InvalidRecordType { line } | Failure::InvalidResponseItemType { line } => {
            import_evidence(
                ConversationImportRejectionClass::InvalidRecordType,
                Some(line),
            )
        }
        Failure::InvalidSourceMetadata { line } => import_evidence(
            ConversationImportRejectionClass::InvalidSourceMetadata,
            Some(line),
        ),
        Failure::InvalidResponseItemEnvelope { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageEnvelope,
            Some(line),
        ),
        Failure::InvalidMessageRole { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageRole,
            Some(line),
        ),
        Failure::InvalidMessageContent { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageContent,
            Some(line),
        ),
        Failure::InvalidMessageBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidContentBlock,
            Some(line),
        ),
        Failure::InvalidReasoning { line } | Failure::InvalidReasoningBlock { line, .. } => {
            import_evidence(
                ConversationImportRejectionClass::InvalidReasoning,
                Some(line),
            )
        }
        Failure::InvalidToolCall { line } => import_evidence(
            ConversationImportRejectionClass::InvalidToolCall,
            Some(line),
        ),
        Failure::InvalidToolResult { line } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResult,
            Some(line),
        ),
        Failure::InvalidToolResultBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResultBlock,
            Some(line),
        ),
        Failure::PositionExhausted | Failure::InvalidAggregate(_) => {
            return ConversionFailureDisposition::Internal;
        }
    };
    ConversionFailureDisposition::Rejected(evidence)
}

pub(super) const fn import_evidence(
    class: ConversationImportRejectionClass,
    record_ordinal: Option<u64>,
) -> ImportRejectionEvidence {
    ImportRejectionEvidence {
        class,
        record_ordinal,
    }
}

/// Converts typed import evidence into closed operational diagnostics.
///
/// Payload-bearing converter and repository errors are consumed here without
/// formatting. Only a fixed classification crosses into the Internal log
/// record, so source content, durable values, and database prose remain absent.
pub(super) fn operational_import_error<ConverterError>(
    error: ImportConversationError<ConverterError, ImportedConversationRepositoryError>,
) -> OperationalImportError
where
    ConverterError: ClassifyConversationImportError,
{
    match error {
        ImportConversationError::SourceRead => OperationalImportError::Unavailable,
        ImportConversationError::Conversion(error) => match error.disposition() {
            ConversionFailureDisposition::Rejected(evidence) => {
                OperationalImportError::InvalidSource(evidence)
            }
            ConversionFailureDisposition::Internal => OperationalImportError::Internal(
                InternalDiagnostic::ConversationImportContractDefect,
            ),
        },
        ImportConversationError::Store(error) => operational_import_repository_error(error),
        ImportConversationError::ConverterIdentityMismatch { .. }
        | ImportConversationError::ConverterFormatMismatch { .. }
        | ImportConversationError::ConverterEntryIdentitySequenceMismatch
        | ImportConversationError::StoreSourceDigestMismatch { .. }
        | ImportConversationError::StoreInsertedIdentityMismatch { .. } => {
            OperationalImportError::Internal(InternalDiagnostic::ConversationImportContractDefect)
        }
    }
}

fn operational_import_repository_error(
    error: ImportedConversationRepositoryError,
) -> OperationalImportError {
    match error {
        ImportedConversationRepositoryError::Database(_) => OperationalImportError::Database,
        ImportedConversationRepositoryError::BlobCatalog(
            signalbox_persistence::blob::BlobCatalogRepositoryError::Database(_),
        ) => OperationalImportError::Database,
        ImportedConversationRepositoryError::BlobCatalog(
            signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(_),
        ) => OperationalImportError::Unavailable,
        ImportedConversationRepositoryError::BlobStorage(
            ImportedRawBlobStorageError::Unavailable,
        ) => OperationalImportError::Unavailable,
        error => {
            OperationalImportError::Internal(imported_conversation_internal_diagnostic(&error))
        }
    }
}

pub(super) async fn execute_import<Converter>(
    mut converter: Converter,
    source: ConversationImportSource,
    repository: ImportedConversationRepository,
) -> Result<CompletedImport, OperationalImportError>
where
    Converter: StreamingResilientImportedConversationConverter + Send + 'static,
    Converter::Error: ClassifyConversationImportError,
    Converter::RecordFailure: ClassifyConversationImportRecordFailure + Copy,
{
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async move {
            let report = match source {
                ConversationImportSource::Inline(source) => {
                    let mut service = ImportConversationService::new(
                        UuidV7ImportedConversationIdGenerator,
                        converter,
                        repository,
                    );
                    service
                        .execute_resilient(&source)
                        .await
                        .map_err(operational_import_error)?
                }
                ConversationImportSource::Spooled(source) => {
                    let mut ids = UuidV7ImportedConversationIdGenerator;
                    let candidate = ids.next_conversation_id();
                    let format = converter.format();
                    let maximum_record_bytes = repository.maximum_raw_record_bytes();
                    let records = converter.convert_resilient_from_reader(
                        candidate,
                        BufReader::new(source),
                        maximum_record_bytes,
                        || ids.next_entry_id(),
                    );
                    match repository
                        .resolve_or_insert_stream(candidate, format, records)
                        .await
                        .map_err(|error| match error {
                            StreamingImportedConversationError::SourceRead => {
                                OperationalImportError::Unavailable
                            }
                            StreamingImportedConversationError::Conversion(error) => {
                                match error.disposition() {
                                    ConversionFailureDisposition::Rejected(evidence) => {
                                        OperationalImportError::InvalidSource(evidence)
                                    }
                                    ConversionFailureDisposition::Internal => {
                                        OperationalImportError::Internal(
                                            InternalDiagnostic::ConversationImportContractDefect,
                                        )
                                    }
                                }
                            }
                            StreamingImportedConversationError::ConverterContract => {
                                OperationalImportError::Internal(
                                    InternalDiagnostic::ConversationImportContractDefect,
                                )
                            }
                            StreamingImportedConversationError::Repository(error) => {
                                operational_import_repository_error(error)
                            }
                        })? {
                        StreamingImportedConversationReport::Imported {
                            outcome,
                            dropped_records,
                        } => {
                            let outcome = match outcome {
                                ImportedConversationStoreOutcome::Inserted {
                                    conversation, ..
                                } => ImportConversationOutcome::Inserted { conversation },
                                ImportedConversationStoreOutcome::AlreadyImported {
                                    conversation,
                                    ..
                                } => ImportConversationOutcome::AlreadyImported { conversation },
                            };
                            return Ok(CompletedImport {
                                outcome,
                                dropped_records,
                            });
                        }
                        StreamingImportedConversationReport::NoValidRecords {
                            first_failure,
                            ..
                        } => match first_failure.disposition() {
                            ConversionFailureDisposition::Rejected(evidence) => {
                                return Err(OperationalImportError::InvalidSource(evidence));
                            }
                            ConversionFailureDisposition::Internal => {
                                return Err(OperationalImportError::Internal(
                                    InternalDiagnostic::ConversationImportContractDefect,
                                ));
                            }
                        },
                    }
                }
            };
            match report {
                ImportConversationReport::Imported {
                    outcome,
                    skipped_records,
                } => {
                    let count = u64::try_from(skipped_records.len()).map_err(|_| {
                        OperationalImportError::Internal(
                            InternalDiagnostic::ConversationImportContractDefect,
                        )
                    })?;
                    let first = skipped_records.first().map(|record| record.source_line());
                    let dropped_records = ImportedConversationDropFacts::try_new(count, first)
                        .ok_or(OperationalImportError::Internal(
                            InternalDiagnostic::ConversationImportContractDefect,
                        ))?;
                    Ok(CompletedImport {
                        outcome,
                        dropped_records,
                    })
                }
                ImportConversationReport::NoValidRecords { skipped_records } => {
                    let failure =
                        skipped_records
                            .first()
                            .ok_or(OperationalImportError::Internal(
                                InternalDiagnostic::ConversationImportContractDefect,
                            ))?;
                    match failure.failure().disposition() {
                        ConversionFailureDisposition::Rejected(evidence) => {
                            Err(OperationalImportError::InvalidSource(evidence))
                        }
                        ConversionFailureDisposition::Internal => {
                            Err(OperationalImportError::Internal(
                                InternalDiagnostic::ConversationImportContractDefect,
                            ))
                        }
                    }
                }
            }
        })
    })
    .await
    .map_err(|_| {
        OperationalImportError::Internal(InternalDiagnostic::ConversationImportWorkerTerminated)
    })?
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CompletedImport {
    outcome: ImportConversationOutcome,
    dropped_records: ImportedConversationDropFacts,
}

pub(super) fn domain_imported_relationship(
    relationship: WireImportedSessionRelationship,
) -> DomainImportedSessionRelationship {
    match relationship {
        WireImportedSessionRelationship::Resume => DomainImportedSessionRelationship::Resume,
        WireImportedSessionRelationship::Fork => DomainImportedSessionRelationship::Fork,
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct WireImportedContinuationRequest {
    pub(super) command_uuid: uuid::Uuid,
    pub(super) conversation: CanonicalUuid,
    pub(super) through_position: CanonicalU64,
    pub(super) relationship: WireImportedSessionRelationship,
    pub(super) initial_model_selection: WireModelSelection,
    pub(super) model_settings: WireModelSettingsOverlay,
}

pub(super) async fn handle_create_session_from_imported_frontier<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    wire_request: WireImportedContinuationRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    imported_conversations: &ImportedConversationRepository,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let command_id = DurableCommandId::from_uuid(wire_request.command_uuid);
    let conversation_id = ImportedConversationId::from_uuid(wire_request.conversation.into_uuid());
    let relationship = domain_imported_relationship(wire_request.relationship);
    let model_selection = domain_model_selection(wire_request.initial_model_selection);
    let caller_model_settings = domain_model_settings_overlay(wire_request.model_settings);
    let through_position = wire_request.through_position;
    let repository =
        ImportedSessionRepository::new(pool.clone(), model_configuration.session_credential_pin());

    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command.imported_conversation() == conversation_id
                && command.imported_frontier().through_position().as_u64()
                    == through_position.value()
                && command.relationship() == relationship
                && command.initial_configuration_defaults().model() == model_selection
                && command
                    .initial_configuration_defaults()
                    .dangerous_tool_auto_approval()
                    == DangerousToolAutoApproval::Disabled
                && command
                    .initial_configuration_defaults()
                    .system_prompt()
                    .is_none()
                && command
                    .initial_configuration_defaults()
                    .model_settings()
                    .precedence()
                    .session()
                    == caller_model_settings
            {
                return write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCreated {
                        session_id: wire_uuid(recorded.applied_result().session().into_uuid()),
                        model_settings: wire_model_settings(
                            recorded
                                .command()
                                .initial_configuration_defaults()
                                .model_settings(),
                        ),
                    },
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(ImportedSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(ImportedSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ImportedSessionRepositoryError::ImportedConversation(
            ImportedConversationRepositoryError::Database(_)
            | ImportedConversationRepositoryError::BlobStorage(
                ImportedRawBlobStorageError::Unavailable,
            ),
        )) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ImportedSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(
            error @ (ImportedSessionRepositoryError::Preparation(_)
            | ImportedSessionRepositoryError::IdentityCollision(_)
            | ImportedSessionRepositoryError::ImportedConversation(_)
            | ImportedSessionRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_session_internal_diagnostic(&error);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    }

    // An unclaimed command resolves its wire address against the immutable
    // imported aggregate before any application construction, so an absent
    // conversation or an out-of-range position wins over a settings admission
    // failure and each still leaves the command identity unclaimed.
    let Some(position) = ImportedTranscriptPosition::try_from_u64(through_position.value()) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let lookup = match imported_conversations
        .lookup_frontier(conversation_id, position)
        .await
    {
        Ok(Some(lookup)) => lookup,
        Ok(None) => {
            return write_error(
                writer,
                version,
                request_id,
                imported_conversation_not_found(wire_request.conversation),
            )
            .await;
        }
        Err(
            ImportedConversationRepositoryError::Database(_)
            | ImportedConversationRepositoryError::BlobStorage(
                ImportedRawBlobStorageError::Unavailable,
            ),
        ) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(
            error @ (ImportedConversationRepositoryError::IdentityCollision(_)
            | ImportedConversationRepositoryError::BlobStorage(
                ImportedRawBlobStorageError::Integrity,
            )
            | ImportedConversationRepositoryError::BlobCatalog(_)
            | ImportedConversationRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_conversation_internal_diagnostic(&error);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    };
    let Some(frontier) = lookup.frontier() else {
        return write_error(
            writer,
            version,
            request_id,
            imported_position_out_of_range(
                wire_request.conversation,
                through_position,
                lookup.last_position().as_u64(),
            ),
        )
        .await;
    };
    let last_position = lookup.last_position().as_u64();

    let model_settings = match validate_session_model_settings(
        model_configuration,
        model_selection,
        caller_model_settings,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            return write_error(
                writer,
                version,
                request_id,
                model_settings_protocol_error(error),
            )
            .await;
        }
    };
    let Some(defaults) = SessionConfigurationDefaults::complete_with_model_settings(
        model_selection,
        DangerousToolAutoApproval::Disabled,
        None,
        model_settings,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };

    if model_configuration
        .resolve_session_model(model_selection)
        .is_err()
    {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    }

    let request = CreateSessionFromImportedFrontierRequest::try_new(
        command_id,
        frontier,
        relationship,
        defaults,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service = CreateSessionFromImportedFrontierService::new(
        UuidV7CreateSessionFromImportedFrontierIdGenerator,
        repository,
    );
    match service.execute(request).await {
        Ok(CreateSessionFromImportedFrontierOutcome::Applied(result)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionCreated {
                    session_id: wire_uuid(result.session().into_uuid()),
                    model_settings: wire_model_settings(model_settings),
                },
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                imported_conversation_not_found(wire_request.conversation),
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                imported_position_out_of_range(
                    wire_request.conversation,
                    through_position,
                    last_position,
                ),
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(ImportedSessionRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(ImportedSessionRepositoryError::ImportedConversation(
            ImportedConversationRepositoryError::Database(_)
            | ImportedConversationRepositoryError::BlobStorage(
                ImportedRawBlobStorageError::Unavailable,
            ),
        )) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(ImportedSessionRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (ImportedSessionRepositoryError::DifferentCommandKind { .. }
            | ImportedSessionRepositoryError::Preparation(_)
            | ImportedSessionRepositoryError::IdentityCollision(_)
            | ImportedSessionRepositoryError::ImportedConversation(_)
            | ImportedSessionRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_session_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await
        }
    }
}

/// Names the absent target as an imported conversation rather than a session.
pub(super) fn imported_conversation_not_found(
    imported_conversation_id: CanonicalUuid,
) -> ProtocolError {
    ProtocolError::rejected(RejectionDetail::ImportedConversationNotFound {
        imported_conversation_id,
    })
}

/// Distinguishes a valid identity carrying an out-of-range position from an
/// absent identity, naming the conversation's selectable range.
pub(super) fn imported_position_out_of_range(
    imported_conversation_id: CanonicalUuid,
    requested_position: CanonicalU64,
    last_position: u64,
) -> ProtocolError {
    // Imported positions are the contiguous sequence `1..=last_position`, so a
    // position this handler could not resolve is always beyond a positive
    // bound. A loaded aggregate that contradicts that is corrupt, and the
    // closed wire shape has no way to state the contradiction.
    if last_position == 0 || requested_position.value() <= last_position {
        return internal_protocol_error(None, InternalDiagnostic::ImportedFrontierRangeCorruption);
    }
    ProtocolError::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
        imported_conversation_id,
        requested_position,
        last_position: CanonicalU64::new(last_position),
    })
}

pub(super) async fn handle_read_imported_conversation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    imported_conversation_id: CanonicalUuid,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let conversation_id = ImportedConversationId::from_uuid(imported_conversation_id.into_uuid());
    let load =
        signalbox_persistence::conversation_import::load_normalized_entries(pool, conversation_id)
            .await;
    let entries = match load {
        Ok(Some(entries)) => entries,
        Ok(None) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::imported_conversation_absent(),
            )
            .await;
        }
        Err(ImportedConversationRepositoryError::Database(_)) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(
            error @ (ImportedConversationRepositoryError::IdentityCollision(_)
            | ImportedConversationRepositoryError::BlobStorage(_)
            | ImportedConversationRepositoryError::BlobCatalog(_)
            | ImportedConversationRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_conversation_internal_diagnostic(&error);
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    };
    let dropped_records = match signalbox_persistence::conversation_import::load_import_drop_facts(
        pool,
        conversation_id,
    )
    .await
    {
        Ok(Some(facts)) => facts,
        Ok(None) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, InternalDiagnostic::ConversationImportContractDefect),
            )
            .await;
        }
        Err(ImportedConversationRepositoryError::Database(_)) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(error) => {
            let diagnostic = imported_conversation_internal_diagnostic(&error);
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    };
    let spool_result = spool_imported_conversation(
        &entries,
        imported_conversation_id,
        dropped_records,
        version,
        request_id,
        configured_usize(model_configuration, "max_imported_text_preview_utf8_bytes"),
    )
    .await;
    drop(entries);
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(error) => return write_snapshot_spool_error(writer, version, request_id, error).await,
    };
    write_spooled_file(writer, &mut spool).await
}

pub(super) async fn spool_imported_conversation(
    entries: &[ImportedTranscriptEntryInput],
    imported_conversation_id: CanonicalUuid,
    dropped_records: ImportedConversationDropFacts,
    version: ProtocolVersion,
    request_id: RequestId,
    max_text_preview_utf8_bytes: Option<usize>,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ImportedConversationStart {
            imported_conversation_id,
            dropped_record_count: CanonicalU64::new(dropped_records.count()),
            first_dropped_record_position: dropped_records
                .first_source_line()
                .map(CanonicalU64::new),
        },
    )
    .await?;
    let mut entry_count = 0_u64;
    for entry in entries {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(entry.position().as_u64()),
                imported_entry_id: wire_uuid(entry.identity().into_uuid()),
                source_speaker: wire_imported_speaker_attestation(entry.source_speaker()),
                content_kind: wire_imported_content_kind(process_imported_content_kind(
                    entry.content(),
                )),
                text_preview: imported_text_preview(entry.content(), max_text_preview_utf8_bytes),
            },
        )
        .await?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)?;
    }
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ImportedConversationEnd {
            imported_conversation_id,
            entry_count: CanonicalU64::new(entry_count),
        },
    )
    .await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

/// Maps one entry's normalized content to the conservative wire kind through
/// the same content-variant classification the transcript projection uses.
pub(super) const fn process_imported_content_kind(
    content: &ImportedTranscriptContent,
) -> ProcessImportedContentKind {
    match content {
        ImportedTranscriptContent::SourceEvent { .. } => ProcessImportedContentKind::SourceEvent,
        ImportedTranscriptContent::SourceMessageBlock { .. } => {
            ProcessImportedContentKind::SourceMessageBlock
        }
        ImportedTranscriptContent::Text(_) => ProcessImportedContentKind::Text,
        ImportedTranscriptContent::ToolCall { .. } => ProcessImportedContentKind::ToolCall,
        ImportedTranscriptContent::ToolResult { .. } => ProcessImportedContentKind::ToolResult,
        ImportedTranscriptContent::Thinking { .. } => ProcessImportedContentKind::Thinking,
        ImportedTranscriptContent::RedactedThinking { .. } => {
            ProcessImportedContentKind::RedactedThinking
        }
        ImportedTranscriptContent::Document { .. } => ProcessImportedContentKind::Document,
        ImportedTranscriptContent::MessageContentAbsent(_) => {
            ProcessImportedContentKind::MessageContentAbsent
        }
    }
}

/// Previews exactly the text the transcript projection already carries in
/// full; every other imported content stays behind its kind alone.
pub(super) fn imported_text_preview(
    content: &ImportedTranscriptContent,
    max_utf8_bytes: Option<usize>,
) -> Option<ImportedTextPreview> {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text))
            if max_utf8_bytes != Some(0) =>
        {
            Some(ImportedTextPreview::of_exact_text_with_limit(
                text.as_str(),
                max_utf8_bytes,
            ))
        }
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(_)) => None,
        ImportedTranscriptContent::Text(
            ImportedSourceAttestation::AttestedAbsent | ImportedSourceAttestation::NotAttested,
        )
        | ImportedTranscriptContent::SourceEvent { .. }
        | ImportedTranscriptContent::SourceMessageBlock { .. }
        | ImportedTranscriptContent::ToolCall { .. }
        | ImportedTranscriptContent::ToolResult { .. }
        | ImportedTranscriptContent::Thinking { .. }
        | ImportedTranscriptContent::RedactedThinking { .. }
        | ImportedTranscriptContent::Document { .. }
        | ImportedTranscriptContent::MessageContentAbsent(_) => None,
    }
}

pub(super) const fn wire_imported_speaker_attestation(
    attestation: &ImportedSourceAttestation<DomainImportedSpeaker>,
) -> ImportedSourceSpeaker {
    match attestation {
        ImportedSourceAttestation::NotAttested => ImportedSourceSpeaker::NotAttested {},
        ImportedSourceAttestation::AttestedAbsent => ImportedSourceSpeaker::AttestedAbsent {},
        ImportedSourceAttestation::Attested(DomainImportedSpeaker::User) => {
            ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            }
        }
        ImportedSourceAttestation::Attested(DomainImportedSpeaker::Assistant) => {
            ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            }
        }
    }
}
