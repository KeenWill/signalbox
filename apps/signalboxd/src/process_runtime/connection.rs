use super::client_io::ArrivalReader;
use super::*;

pub(super) struct ConnectionDependencies {
    pub(super) metrics: Option<crate::telemetry::TelemetryMetrics>,
    pub(super) configuration_reload: Option<crate::configuration_reload::ConfigurationReload>,
    pub(super) recovery_reporter: Option<FatalRecoveryReporter>,
    pub(super) oauth_service: Option<Arc<crate::OauthCredentialService>>,
    pub(super) pool: PgPool,
    pub(super) eligibility_nudge: InProcessEligibilityNudge,
    pub(super) tool_dispatch_gate: InProcessToolDispatchGate,
    pub(super) goal_resumption: Option<PostgresGoalPassDisposition>,
    pub(super) model_configuration: HubModelConfiguration,
    pub(super) context_compaction_model: Arc<dyn ContextCompactionModel>,
    pub(super) template_configuration: SessionTemplateConfiguration,
    pub(super) fanouts: ProcessFanouts,
    pub(super) blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    pub(super) snapshot_reader_budget: Option<Arc<Semaphore>>,
}

pub(super) async fn serve_connections(
    listener: &LocalProcessListener,
    dependencies: ConnectionDependencies,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let mut occupancy = ConnectionOccupancy {
        metrics: dependencies.metrics,
        saturated: false,
    };
    let snapshot_reader_budget = dependencies
        .snapshot_reader_budget
        .ok_or(ProcessRuntimeError::InsufficientPoolCapacity)?;
    let blob_read_budget = dependencies.blob_store_registry.as_ref().map_or_else(
        || Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_READS)),
        |registry| registry.read_budget(),
    );
    let imported_storage = Arc::new(
        crate::imported_source_blobs::ImportedSourceBlobStorage::new(
            dependencies.pool.clone(),
            dependencies.blob_store_registry.clone(),
            dependencies
                .model_configuration
                .conversation_import_max_source_bytes(),
        ),
    );
    #[cfg(feature = "test-support")]
    let imported_conversations = if dependencies.blob_store_registry.is_none() {
        ImportedConversationRepository::new(dependencies.pool.clone())
    } else {
        ImportedConversationRepository::with_blob_storage(
            dependencies.pool.clone(),
            imported_storage,
        )
    };
    #[cfg(not(feature = "test-support"))]
    let imported_conversations = ImportedConversationRepository::with_blob_storage(
        dependencies.pool.clone(),
        imported_storage,
    );
    let services = ConnectionServices {
        configuration_reload: dependencies.configuration_reload,
        recovery_reporter: dependencies.recovery_reporter,
        oauth_service: dependencies.oauth_service,
        pool: dependencies.pool,
        eligibility_nudge: dependencies.eligibility_nudge,
        tool_dispatch_gate: dependencies.tool_dispatch_gate,
        goal_resumption: dependencies.goal_resumption,
        model_configuration: Arc::new(dependencies.model_configuration),
        context_compaction_model: dependencies.context_compaction_model,
        template_configuration: Arc::new(dependencies.template_configuration),
        fanouts: dependencies.fanouts,
        inbound_frame_budgets: InboundFrameBudgets::new(),
        import_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS)),
        import_waiter_budget: Arc::new(Semaphore::new(MAX_IMPORT_ADMISSION_WAITERS)),
        blob_read_budget,
        review_command_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEW_COMMANDS)),
        snapshot_reader_budget,
        blob_store_registry: dependencies.blob_store_registry,
        imported_conversations,
    };
    let mut connections = JoinSet::new();
    let mut accept_retry_at = Instant::now();
    loop {
        occupancy.observe(connections.len());
        if shutdown_requested(&shutdown) {
            break;
        }
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => break,
            accepted = accept_with_retry(&mut accept_retry_at, || listener.accept()), if connections.len() < MAX_ACTIVE_CONNECTIONS => {
                let (stream, _) = accepted.map_err(ProcessRuntimeError::Accept)?;
                let Some(stream) = admit_client_peer(stream, rustix::process::geteuid().as_raw()) else {
                    continue;
                };
                connections.spawn(serve_connection(
                    stream,
                    services.clone(),
                    shutdown.clone(),
                ));
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                inspect_connection_completion(completed)?;
            }
        }
    }

    while let Some(completed) = connections.join_next().await {
        occupancy.observe(connections.len());
        inspect_connection_completion(Some(completed))?;
    }
    Ok(())
}

fn admit_client_peer(stream: UnixStream, expected_uid: u32) -> Option<UnixStream> {
    match stream.peer_cred() {
        Ok(peer) if peer.uid() == expected_uid => Some(stream),
        Ok(peer) => {
            tracing::warn!(
                peer_uid = peer.uid(),
                expected_uid,
                "client peer failed same-user admission"
            );
            None
        }
        Err(_) => {
            tracing::warn!("client peer identity inspection failed");
            None
        }
    }
}

struct ConnectionOccupancy {
    metrics: Option<crate::telemetry::TelemetryMetrics>,
    saturated: bool,
}

impl ConnectionOccupancy {
    fn observe(&mut self, active: usize) {
        if let Some(metrics) = &self.metrics {
            metrics.observe_client_connections(active);
        }
        let saturated = active >= MAX_ACTIVE_CONNECTIONS;
        if saturated && !self.saturated {
            tracing::warn!(active_connections = active, "client connection cap reached");
        }
        self.saturated = saturated;
    }
}

impl Drop for ConnectionOccupancy {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            metrics.observe_client_connections(0);
        }
    }
}

// Bounds accept retry frequency during transient resource exhaustion.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(250);

async fn accept_with_retry<T, Accept, Accepted>(
    retry_at: &mut Instant,
    mut accept: Accept,
) -> io::Result<T>
where
    Accept: FnMut() -> Accepted,
    Accepted: Future<Output = io::Result<T>>,
{
    loop {
        // The deadline survives cancellation when a connection completes in select!.
        sleep_until(*retry_at).await;
        match accept().await {
            Ok(stream) => return Ok(stream),
            Err(error) if recoverable_accept_error(&error) => {
                tracing::warn!("process listener retrying a recoverable accept failure");
                *retry_at = Instant::now() + ACCEPT_RETRY_DELAY;
            }
            Err(error) => return Err(error),
        }
    }
}

fn recoverable_accept_error(error: &io::Error) -> bool {
    use rustix::io::Errno;
    matches!(
        Errno::from_io_error(error),
        Some(
            Errno::INTR
                | Errno::AGAIN
                | Errno::CONNABORTED
                | Errno::CONNRESET
                | Errno::MFILE
                | Errno::NFILE
                | Errno::NOBUFS
                | Errno::NOMEM
        )
    )
}

pub(super) fn inspect_connection_completion(
    completed: Option<Result<Result<(), ProcessConnectionError>, JoinError>>,
) -> Result<(), ProcessRuntimeError> {
    match completed {
        None | Some(Ok(Ok(()))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::PeerIo(error)))) => {
            drop(error);
            Ok(())
        }
        Some(Ok(Err(ProcessConnectionError::SpoolIo(error)))) => {
            Err(ProcessRuntimeError::SpoolIo(error))
        }
        Some(Ok(Err(
            ProcessConnectionError::Encode(_) | ProcessConnectionError::EncodeInvariant,
        ))) => {
            tracing::error!("process connection closed after an encoding failure");
            Ok(())
        }
        Some(Ok(Err(ProcessConnectionError::InboundFrameBudgetClosed))) => {
            Err(ProcessRuntimeError::InboundFrameBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::SnapshotReaderBudgetClosed))) => {
            Err(ProcessRuntimeError::SnapshotReaderBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::ImportBudgetClosed))) => {
            Err(ProcessRuntimeError::ImportBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::ReviewCommandBudgetClosed))) => {
            Err(ProcessRuntimeError::ReviewCommandBudgetClosed)
        }
        Some(Err(error)) => Err(ProcessRuntimeError::ConnectionTask(error)),
    }
}

pub(super) async fn serve_connection(
    stream: UnixStream,
    services: ConnectionServices,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError> {
    let (reader, writer) = stream.into_split();
    let reader = ArrivalReader::new(reader);
    tokio::select! {
        biased;
        result = reader.watch_readiness() => result.map_err(Into::into),
        result = serve_connection_io(reader, writer, services, shutdown) => result,
    }
}

async fn serve_connection_io(
    reader: ArrivalReader,
    writer: tokio::net::unix::OwnedWriteHalf,
    services: ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError> {
    let bounds = services.model_configuration.numeric_bounds();
    let frame_deadline = bounds.duration("client_frame_deadline").flatten();
    let mut writer = super::client_io::ProgressWriter::new(
        writer,
        bounds.duration("client_write_progress_deadline").flatten(),
    );
    let mut reader = BufReader::with_capacity(INBOUND_READ_AHEAD_BYTES, reader);
    let mut pending_import = None;
    let mut pending_blob_upload = None;

    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let awaiting_bulk_ingest_deadline =
            pending_bulk_ingest_deadline(&pending_import, &pending_blob_upload, true);
        let import_state = if pending_import.is_some() || pending_blob_upload.is_some() {
            ConversationImportState::Active
        } else {
            ConversationImportState::Inactive
        };
        let inbound_frame_budget = services.inbound_frame_budgets.for_connection(import_state);
        let admitted_frame = tokio::select! {
            biased;
            () = wait_for_deadline(awaiting_bulk_ingest_deadline) => return Ok(()),
            frame = read_admitted_frame(
                &mut reader,
                inbound_frame_budget,
                &mut shutdown,
                frame_deadline,
            ) => frame?,
        };
        let Some((frame_buffer_permit, line)) = admitted_frame else {
            return Ok(());
        };
        let frame = match line {
            IncomingLine::Complete(line) => match decode_client_line(&line) {
                Ok(frame) => frame,
                Err(error) => {
                    let admitted_version = line
                        .strip_suffix(b"\n")
                        .and_then(recover_bounded_client_protocol_version);
                    let code = match error.kind() {
                        FrameDecodeErrorKind::UnsupportedVersion => ErrorCode::UnsupportedVersion,
                        FrameDecodeErrorKind::OversizedFrame
                        | FrameDecodeErrorKind::MalformedFrame => ErrorCode::MalformedFrame,
                    };
                    drop(line);
                    drop(pending_import.take());
                    drop(pending_blob_upload.take());
                    drop(frame_buffer_permit);
                    write_error(
                        &mut writer,
                        admitted_version.unwrap_or(ProtocolVersion::One),
                        error.request_id(),
                        ProtocolError::without_detail(code),
                    )
                    .await?;
                    return Ok(());
                }
            },
            IncomingLine::Oversized {
                request_id,
                admitted_version,
            } => {
                drop(pending_import.take());
                drop(pending_blob_upload.take());
                drop(frame_buffer_permit);
                write_error(
                    &mut writer,
                    admitted_version.unwrap_or(ProtocolVersion::One),
                    request_id,
                    ProtocolError::without_detail(ErrorCode::MalformedFrame),
                )
                .await?;
                return Ok(());
            }
        };
        let (version, request_id, request) = frame.into_parts();
        if !request_within_configured_collection_limits(&request, &services.model_configuration) {
            drop(frame_buffer_permit);
            write_error(
                &mut writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await?;
            continue;
        }
        let follows = matches!(request, ClientRequest::FollowSession { .. });
        let import_limit = services
            .model_configuration
            .conversation_import_max_source_bytes();
        let import_requires_permit = conversation_import_request_requires_permit(
            &request,
            import_state,
            import_limit,
            services
                .blob_store_registry
                .as_deref()
                .map_or(0, BlobStoreRegistry::max_blob_bytes),
        );
        let import_waiter_permit = if import_requires_permit
            && matches!(
                &request,
                ClientRequest::BeginConversationImport { .. }
                    | ClientRequest::BeginBlobUpload { .. }
            ) {
            let Some(permit) = acquire_import_waiter_permit(
                Arc::clone(&services.import_waiter_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            Some(permit)
        } else {
            None
        };
        let frame_buffer_permit = retain_inbound_frame_permit_during_import_admission(
            &request,
            import_requires_permit,
            frame_buffer_permit,
        );
        let import_permit = if import_requires_permit {
            let Some(permit) =
                acquire_import_permit(Arc::clone(&services.import_budget), &mut shutdown).await?
            else {
                return Ok(());
            };
            Some(permit)
        } else {
            None
        };
        let acquired_bulk_ingest_at = import_permit.as_ref().map(|_| Instant::now());
        drop(import_waiter_permit);
        let review_admission_deadline =
            pending_bulk_ingest_deadline(&pending_import, &pending_blob_upload, true);
        let Some((frame_buffer_permit, review_command_permit)) =
            acquire_review_command_permit_while_buffered(
                ReviewCommandAdmission::for_request(&request),
                frame_buffer_permit,
                Arc::clone(&services.review_command_budget),
                &mut shutdown,
                review_admission_deadline,
            )
            .await?
        else {
            return Ok(());
        };
        drop(frame_buffer_permit);
        let active_lifecycle_request =
            active_bulk_ingest_kind(&pending_import, &pending_blob_upload)
                .is_some_and(|kind| request_is_lifecycle_for_kind(&request, kind));
        let operation_deadline = pending_bulk_ingest_deadline(
            &pending_import,
            &pending_blob_upload,
            !active_lifecycle_request,
        )
        .or_else(|| acquired_bulk_ingest_at.map(|started| started + BULK_INGEST_SESSION_TIMEOUT));
        let request_result = Box::pin(handle_request(
            &mut reader,
            &mut writer,
            version,
            request_id,
            request,
            ConnectionRequestResources {
                import_permit,
                acquired_bulk_ingest_at,
                review_command_permit,
                pending_import: &mut pending_import,
                pending_blob_upload: &mut pending_blob_upload,
            },
            &services,
            shutdown.clone(),
        ));
        tokio::select! {
            biased;
            () = wait_for_deadline(operation_deadline) => return Ok(()),
            result = request_result => result?,
        }
        if follows {
            return Ok(());
        }
    }
}

pub(super) fn active_bulk_ingest_kind(
    pending_import: &Option<PendingConversationImport>,
    pending_blob_upload: &Option<PendingBlobUpload>,
) -> Option<BulkIngestKind> {
    if pending_import.is_some() {
        Some(BulkIngestKind::ConversationImport)
    } else if pending_blob_upload.is_some() {
        Some(BulkIngestKind::BlobUpload)
    } else {
        None
    }
}

pub(super) fn request_within_configured_collection_limits(
    request: &ClientRequest,
    configuration: &HubModelConfiguration,
) -> bool {
    match request {
        ClientRequest::RecordReviewFindings { findings, .. } => {
            configured_usize(configuration, "max_review_findings_per_run")
                .is_none_or(|maximum| findings.len() <= maximum)
        }
        ClientRequest::StartReviewOrchestration { concerns, .. } => {
            configured_usize(configuration, "max_review_orchestration_concerns")
                .is_none_or(|maximum| concerns.len() <= maximum)
        }
        _ => true,
    }
}

pub(super) fn request_is_lifecycle_for_kind(request: &ClientRequest, kind: BulkIngestKind) -> bool {
    match kind {
        BulkIngestKind::ConversationImport => matches!(
            request,
            ClientRequest::AppendConversationImport { .. }
                | ClientRequest::CommitConversationImport {}
                | ClientRequest::AbortConversationImport {}
        ),
        BulkIngestKind::BlobUpload => matches!(
            request,
            ClientRequest::AppendBlobUpload { .. }
                | ClientRequest::CommitBlobUpload {}
                | ClientRequest::AbortBlobUpload {}
        ),
    }
}

pub(super) fn request_is_cross_kind_bulk_ingest(
    request: &ClientRequest,
    active_kind: BulkIngestKind,
) -> bool {
    match active_kind {
        BulkIngestKind::ConversationImport => matches!(
            request,
            ClientRequest::BeginBlobUpload { .. }
                | ClientRequest::AppendBlobUpload { .. }
                | ClientRequest::CommitBlobUpload {}
                | ClientRequest::AbortBlobUpload {}
        ),
        BulkIngestKind::BlobUpload => matches!(
            request,
            ClientRequest::ImportConversation { .. }
                | ClientRequest::BeginConversationImport { .. }
                | ClientRequest::AppendConversationImport { .. }
                | ClientRequest::CommitConversationImport {}
                | ClientRequest::AbortConversationImport {}
        ),
    }
}

pub(super) fn pending_bulk_ingest_deadline(
    pending_import: &Option<PendingConversationImport>,
    pending_blob_upload: &Option<PendingBlobUpload>,
    include_idle: bool,
) -> Option<Instant> {
    let (started_at, idle_since) = if let Some(import) = pending_import {
        (import.started_at, import.idle_since)
    } else if let Some(upload) = pending_blob_upload {
        (upload.started_at(), upload.idle_since())
    } else {
        return None;
    };
    let session_deadline = started_at + BULK_INGEST_SESSION_TIMEOUT;
    if include_idle {
        Some(session_deadline.min(idle_since + BULK_INGEST_IDLE_TIMEOUT))
    } else {
        Some(session_deadline)
    }
}

pub(super) async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(super) async fn read_admitted_frame(
    reader: &mut ClientReader,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
    frame_deadline: Option<Duration>,
) -> Result<Option<(OwnedSemaphorePermit, IncomingLine)>, ProcessConnectionError> {
    let input_ready = tokio::select! {
        () = wait_for_shutdown(shutdown) => false,
        available = reader.fill_buf() => !available?.is_empty(),
    };
    if !input_ready {
        return Ok(None);
    }
    // fill_buf refills only an empty buffer, so retained bytes share its arrival time.
    let deadline = frame_deadline.map(|duration| reader.get_ref().received_at + duration);
    tokio::select! {
        biased;
        () = wait_for_deadline(deadline) => {
            Err(io::Error::new(io::ErrorKind::TimedOut, "client frame deadline elapsed").into())
        }
        frame = async {
            let Some(permit) = acquire_inbound_frame_permit(budget, shutdown).await? else {
                return Ok(None);
            };
            tokio::select! {
                biased;
                () = wait_for_shutdown(shutdown) => Ok(None),
                line = read_frame_line(reader) => Ok(line?.map(|line| (permit, line))),
            }
        } => frame,
    }
}

pub(super) async fn acquire_inbound_frame_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::InboundFrameBudgetClosed),
    }
}

pub(super) async fn acquire_snapshot_reader_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::SnapshotReaderBudgetClosed),
    }
}

pub(super) fn conversation_import_request_requires_permit(
    request: &ClientRequest,
    import_state: ConversationImportState,
    limit: usize,
    max_blob_bytes: u64,
) -> bool {
    match import_state {
        ConversationImportState::Inactive => {}
        ConversationImportState::Active => return false,
    }
    match request {
        ClientRequest::ImportConversation { source, .. } => source.as_bytes().len() <= limit,
        ClientRequest::BeginConversationImport {
            declared_size_bytes,
            ..
        } => usize::try_from(declared_size_bytes.value()).is_ok_and(|size| size <= limit),
        ClientRequest::BeginBlobUpload {
            expected_length_bytes,
            ..
        } => (1..=max_blob_bytes).contains(&expected_length_bytes.value()),
        ClientRequest::CreateSession { .. }
        | ClientRequest::CreateSessionFromTemplate { .. }
        | ClientRequest::CommissionSession { .. }
        | ClientRequest::ListTemplates {}
        | ClientRequest::ListCredentialExclusions { .. }
        | ClientRequest::ReadCredentialPoolPolicy { .. }
        | ClientRequest::ReadRunnerStatus { .. }
        | ClientRequest::CancelProgramRun { .. }
        | ClientRequest::ClearCredentialExclusion { .. }
        | ClientRequest::ReadDeploymentLimits {}
        | ClientRequest::ListSessions {}
        | ClientRequest::ReadOperatorStatus {}
        | ClientRequest::ReloadConfiguration { .. }
        | ClientRequest::UpdateSessionPlacement { .. }
        | ClientRequest::AttachGoal { .. }
        | ClientRequest::ReadGoal { .. }
        | ClientRequest::ResumeGoal { .. }
        | ClientRequest::StopGoal { .. }
        | ClientRequest::StopSession { .. }
        | ClientRequest::SupersedeSession { .. }
        | ClientRequest::AbandonSession { .. }
        | ClientRequest::CloseSessionFailed { .. }
        | ClientRequest::ResumeSession { .. }
        | ClientRequest::AdoptSession { .. }
        | ClientRequest::ReleaseSession { .. }
        | ClientRequest::ReleaseStart { .. }
        | ClientRequest::SupersedeGoal { .. }
        | ClientRequest::SubmitInput { .. }
        | ClientRequest::CompactSession { .. }
        | ClientRequest::ReadTranscript { .. }
        | ClientRequest::FollowSession { .. }
        | ClientRequest::SpawnSession { .. }
        | ClientRequest::AwaitSession { .. }
        | ClientRequest::SendSessionMessage { .. }
        | ClientRequest::ListSessionMetadata { .. }
        | ClientRequest::ListConversations { .. }
        | ClientRequest::ListModelAliases {}
        | ClientRequest::ListModelCapabilities {}
        | ClientRequest::ReadSessionMetadata { .. }
        | ClientRequest::ReplaceSessionMetadata { .. }
        | ClientRequest::ReplaceSessionDefaults { .. }
        | ClientRequest::ReadSessionDefaults { .. }
        | ClientRequest::AppendConversationImport { .. }
        | ClientRequest::CommitConversationImport {}
        | ClientRequest::AbortConversationImport {}
        | ClientRequest::AppendBlobUpload { .. }
        | ClientRequest::CommitBlobUpload {}
        | ClientRequest::AbortBlobUpload {}
        | ClientRequest::ReadBlobMetadata { .. }
        | ClientRequest::ReadBlobChunk { .. }
        | ClientRequest::ReadImportedConversation { .. }
        | ClientRequest::CreateSessionFromImportedFrontier { .. }
        | ClientRequest::ReconcileTurn { .. }
        | ClientRequest::CreateReviewTarget { .. }
        | ClientRequest::StartReviewRun { .. }
        | ClientRequest::ActivateReviewPass { .. }
        | ClientRequest::CompleteReviewPass { .. }
        | ClientRequest::RecordReviewFindings { .. }
        | ClientRequest::RecordReviewFindingEvent { .. }
        | ClientRequest::ReserveReviewExternalLink { .. }
        | ClientRequest::AttachReviewExternalLink { .. }
        | ClientRequest::ReadReviewTarget { .. }
        | ClientRequest::ReadReviewRun { .. }
        | ClientRequest::ReadReviewFinding { .. }
        | ClientRequest::ListReviewFindings { .. }
        | ClientRequest::StartReviewOrchestration { .. }
        | ClientRequest::RecordReviewImportOutcome { .. }
        | ClientRequest::RecordReviewConcernOutcome { .. }
        | ClientRequest::RecordReviewJudgmentPlan { .. }
        | ClientRequest::RecordReviewJudgmentEffect { .. }
        | ClientRequest::RecordReviewRepairOutcomes { .. }
        | ClientRequest::RecordReviewPublicationOutcomes { .. }
        | ClientRequest::ReadReviewOrchestration { .. }
        | ClientRequest::StopTurn { .. }
        | ClientRequest::DecideToolRequest { .. }
        | ClientRequest::OverrideDeniedToolRequest { .. }
        | ClientRequest::ReplaceLostRunner { .. }
        | ClientRequest::AbandonLostRunner { .. }
        | ClientRequest::RegisterWorkspace { .. }
        | ClientRequest::MintGitRemote { .. }
        | ClientRequest::WithdrawGitRemote { .. }
        | ClientRequest::PromotePendingRunner { .. }
        | ClientRequest::ProvisionOauthCredential { .. }
        | ClientRequest::ReprovisionOauthCredential { .. }
        | ClientRequest::DeleteOauthCredential { .. } => false,
    }
}
pub(super) fn retain_inbound_frame_permit_during_import_admission(
    request: &ClientRequest,
    import_requires_permit: bool,
    permit: OwnedSemaphorePermit,
) -> Option<OwnedSemaphorePermit> {
    if import_requires_permit
        && matches!(
            request,
            ClientRequest::BeginConversationImport { .. } | ClientRequest::BeginBlobUpload { .. }
        )
    {
        None
    } else {
        Some(permit)
    }
}

pub(super) async fn acquire_import_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ImportBudgetClosed),
    }
}

pub(super) async fn acquire_import_waiter_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ImportBudgetClosed),
    }
}

pub(super) fn try_acquire_blob_read_permit(budget: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    budget.try_acquire_owned().ok()
}

pub(super) async fn acquire_review_command_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ReviewCommandBudgetClosed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReviewCommandAdmission {
    Required,
    NotRequired,
}

impl ReviewCommandAdmission {
    pub(super) const fn for_request(request: &ClientRequest) -> Self {
        if is_review_mutation(request) {
            Self::Required
        } else {
            Self::NotRequired
        }
    }
}

pub(super) async fn acquire_review_command_permit_while_buffered(
    review_admission: ReviewCommandAdmission,
    frame_buffer_permit: Option<OwnedSemaphorePermit>,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<
    Option<(Option<OwnedSemaphorePermit>, Option<OwnedSemaphorePermit>)>,
    ProcessConnectionError,
> {
    let review_command_permit = match review_admission {
        ReviewCommandAdmission::Required => {
            let permit = tokio::select! {
                biased;
                () = wait_for_deadline(deadline) => return Ok(None),
                permit = acquire_review_command_permit(budget, shutdown) => permit?,
            };
            let Some(permit) = permit else {
                return Ok(None);
            };
            Some(permit)
        }
        ReviewCommandAdmission::NotRequired => None,
    };
    Ok(Some((frame_buffer_permit, review_command_permit)))
}

/// One closed snapshot-reader admission class, decided for every request before
/// dispatch.
///
/// The decision lives here rather than in each dispatch arm because a verb that
/// forgets to reserve capacity does not fail: it quietly spends the connections
/// [`RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS`] holds back for the outbox
/// dispatcher and mutations. An exhaustive match makes a later verb state its
/// class instead of inheriting one by omission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapshotReaderAdmission {
    /// The request requires no snapshot-reader admission.
    NotRequired,
    /// The request holds one pooled connection across its database phase — a
    /// multi-statement read, a `REPEATABLE READ` transaction, or a spool.
    OneConnection,
}

impl SnapshotReaderAdmission {
    pub(super) const fn for_request(request: &ClientRequest) -> Self {
        match request {
            ClientRequest::ListSessions {}
            | ClientRequest::ReadOperatorStatus {}
            | ClientRequest::ReadRunnerStatus { .. }
            | ClientRequest::ReadGoal { .. }
            | ClientRequest::ReadTranscript { .. }
            | ClientRequest::FollowSession { .. }
            | ClientRequest::ListSessionMetadata { .. }
            | ClientRequest::ListConversations { .. }
            | ClientRequest::ReadImportedConversation { .. }
            // The metadata point read is not one statement: it opens a
            // transaction, sets `REPEATABLE READ ONLY`, selects, and commits.
            | ClientRequest::ReadSessionMetadata { .. }
            // Each review read opens its own `REPEATABLE READ` transaction, and
            // the findings listing opens two and then walks the finding graph.
            | ClientRequest::ReadReviewTarget { .. }
            | ClientRequest::ReadReviewRun { .. }
            | ClientRequest::ReadReviewFinding { .. }
            | ClientRequest::ListReviewFindings { .. }
            // The coherent orchestration snapshot reads every adapter fact
            // inside one `REPEATABLE READ` transaction on a single connection.
            | ClientRequest::ReadReviewOrchestration { .. } => Self::OneConnection,
            ClientRequest::CreateSession { .. }
            | ClientRequest::CreateSessionFromTemplate { .. }
            | ClientRequest::CommissionSession { .. }
            | ClientRequest::ListTemplates {}
            | ClientRequest::ListCredentialExclusions { .. }
            | ClientRequest::ReadCredentialPoolPolicy { .. }
        | ClientRequest::CancelProgramRun { .. }
        | ClientRequest::ClearCredentialExclusion { .. }
        | ClientRequest::ReloadConfiguration { .. }
        | ClientRequest::ReadDeploymentLimits {}
            | ClientRequest::UpdateSessionPlacement { .. }
            | ClientRequest::AttachGoal { .. }
            | ClientRequest::ResumeGoal { .. }
            | ClientRequest::StopGoal { .. }
            | ClientRequest::StopSession { .. }
            | ClientRequest::SupersedeSession { .. }
            | ClientRequest::AbandonSession { .. }
            | ClientRequest::CloseSessionFailed { .. }
            | ClientRequest::ResumeSession { .. }
            | ClientRequest::AdoptSession { .. }
            | ClientRequest::ReleaseSession { .. }
            | ClientRequest::ReleaseStart { .. }
            | ClientRequest::SupersedeGoal { .. }
            | ClientRequest::SubmitInput { .. }
            | ClientRequest::CompactSession { .. }
            | ClientRequest::SpawnSession { .. }
            | ClientRequest::AwaitSession { .. }
            | ClientRequest::SendSessionMessage { .. }
            | ClientRequest::ListModelAliases {}
            | ClientRequest::ListModelCapabilities {}
            | ClientRequest::ReplaceSessionMetadata { .. }
            | ClientRequest::ReplaceSessionDefaults { .. }
            | ClientRequest::ReadSessionDefaults { .. }
            | ClientRequest::ImportConversation { .. }
            | ClientRequest::BeginConversationImport { .. }
            | ClientRequest::AppendConversationImport { .. }
            | ClientRequest::CommitConversationImport {}
            | ClientRequest::AbortConversationImport {}
            | ClientRequest::BeginBlobUpload { .. }
            | ClientRequest::AppendBlobUpload { .. }
            | ClientRequest::CommitBlobUpload {}
            | ClientRequest::AbortBlobUpload {}
            | ClientRequest::ReadBlobMetadata { .. }
            | ClientRequest::ReadBlobChunk { .. }
            | ClientRequest::CreateSessionFromImportedFrontier { .. }
            | ClientRequest::ReconcileTurn { .. }
            | ClientRequest::CreateReviewTarget { .. }
            | ClientRequest::StartReviewRun { .. }
            | ClientRequest::ActivateReviewPass { .. }
            | ClientRequest::CompleteReviewPass { .. }
            | ClientRequest::RecordReviewFindings { .. }
            | ClientRequest::RecordReviewFindingEvent { .. }
            | ClientRequest::ReserveReviewExternalLink { .. }
            | ClientRequest::AttachReviewExternalLink { .. }
            | ClientRequest::StartReviewOrchestration { .. }
            | ClientRequest::RecordReviewImportOutcome { .. }
            | ClientRequest::RecordReviewConcernOutcome { .. }
            | ClientRequest::RecordReviewJudgmentPlan { .. }
            | ClientRequest::RecordReviewJudgmentEffect { .. }
            | ClientRequest::RecordReviewRepairOutcomes { .. }
            | ClientRequest::RecordReviewPublicationOutcomes { .. }
            | ClientRequest::StopTurn { .. }
            | ClientRequest::DecideToolRequest { .. }
            | ClientRequest::OverrideDeniedToolRequest { .. }
            | ClientRequest::ReplaceLostRunner { .. }
            | ClientRequest::AbandonLostRunner { .. }
            | ClientRequest::RegisterWorkspace { .. }
        | ClientRequest::MintGitRemote { .. }
        | ClientRequest::WithdrawGitRemote { .. }
        | ClientRequest::PromotePendingRunner { .. }
            | ClientRequest::ProvisionOauthCredential { .. }
        | ClientRequest::ReprovisionOauthCredential { .. }
        | ClientRequest::DeleteOauthCredential { .. } => Self::NotRequired,
        }
    }
}

/// The snapshot-reader capacity one request holds, or `None` when shutdown
/// cancelled the wait and the request goes unanswered.
pub(super) async fn admit_snapshot_reader(
    request: &ClientRequest,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<Option<OwnedSemaphorePermit>>, ProcessConnectionError> {
    match SnapshotReaderAdmission::for_request(request) {
        SnapshotReaderAdmission::NotRequired => Ok(Some(None)),
        SnapshotReaderAdmission::OneConnection => {
            Ok(acquire_snapshot_reader_permit(budget, shutdown)
                .await?
                .map(Some))
        }
    }
}

pub(super) fn configured_usize(
    configuration: &HubModelConfiguration,
    field: &'static str,
) -> Option<usize> {
    configured_u64(configuration, field).and_then(|value| usize::try_from(value).ok())
}

pub(super) fn configured_u64(
    configuration: &HubModelConfiguration,
    field: &'static str,
) -> Option<u64> {
    configuration.numeric_bounds().integer(field).flatten()
}

pub(super) const fn lower_optional_usize(
    left: Option<usize>,
    right: Option<usize>,
) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left < right { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub(super) fn snapshot_reader_capacity(
    max_pool_connections: u32,
    configured_limit: Option<usize>,
) -> Option<usize> {
    let available =
        max_pool_connections.checked_sub(RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?;
    if available == 0 {
        return None;
    }
    usize::try_from(available).ok().and_then(|available| {
        let admitted = configured_limit.map_or(available, |limit| available.min(limit));
        (admitted > 0).then_some(admitted)
    })
}

/// Builds the daemon-wide snapshot-reader admission budget once so the process
/// runtime and the browser HTTP reads draw permits from the same ceiling.
///
/// The ceiling honours the operator's `max_concurrent_snapshot_readers` bound
/// when the hub configuration carries one; callers without a configuration
/// (deterministic routers) fall back to the pool-derived ceiling alone.
pub fn shared_snapshot_reader_budget(
    max_pool_connections: u32,
    model_configuration: Option<&HubModelConfiguration>,
) -> Option<Arc<Semaphore>> {
    let configured_limit = model_configuration.and_then(|configuration| {
        configured_usize(configuration, "max_concurrent_snapshot_readers")
    });
    snapshot_reader_capacity(max_pool_connections, configured_limit)
        .map(|capacity| Arc::new(Semaphore::new(capacity)))
}

pub(super) const fn is_review_mutation(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::CreateReviewTarget { .. }
            | ClientRequest::StartReviewRun { .. }
            | ClientRequest::ActivateReviewPass { .. }
            | ClientRequest::CompleteReviewPass { .. }
            | ClientRequest::RecordReviewFindings { .. }
            | ClientRequest::RecordReviewFindingEvent { .. }
            | ClientRequest::ReserveReviewExternalLink { .. }
            | ClientRequest::AttachReviewExternalLink { .. }
            | ClientRequest::StartReviewOrchestration { .. }
            | ClientRequest::RecordReviewImportOutcome { .. }
            | ClientRequest::RecordReviewConcernOutcome { .. }
            | ClientRequest::RecordReviewJudgmentPlan { .. }
            | ClientRequest::RecordReviewJudgmentEffect { .. }
            | ClientRequest::RecordReviewRepairOutcomes { .. }
            | ClientRequest::RecordReviewPublicationOutcomes { .. }
    )
}

pub(super) fn canonical_review_request_digest(request: &mut ClientRequest) -> Option<[u8; 32]> {
    if let ClientRequest::RecordReviewFindings { findings, .. } = request {
        findings.sort_unstable_by_key(|finding| finding.finding_id.into_uuid());
    }
    serde_json::to_vec(request).ok().map(|bytes| {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        digest
    })
}

pub(super) struct ReviewResponseWriter<'a, Writer> {
    writer: &'a mut Writer,
    command_permit: Option<OwnedSemaphorePermit>,
}

impl<'a, Writer> ReviewResponseWriter<'a, Writer> {
    pub(super) const fn new(
        writer: &'a mut Writer,
        command_permit: Option<OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            writer,
            command_permit,
        }
    }

    fn release_command_permit(&mut self) {
        self.command_permit.take();
    }
}

impl<Writer> AsyncWrite for ReviewResponseWriter<'_, Writer>
where
    Writer: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_shutdown(context)
    }
}

pub(super) struct ConnectionRequestResources<'connection> {
    pub(super) import_permit: Option<OwnedSemaphorePermit>,
    pub(super) acquired_bulk_ingest_at: Option<Instant>,
    pub(super) review_command_permit: Option<OwnedSemaphorePermit>,
    pub(super) pending_import: &'connection mut Option<PendingConversationImport>,
    pub(super) pending_blob_upload: &'connection mut Option<PendingBlobUpload>,
}

pub(super) struct PendingConversationImport {
    pub(super) format: ConversationImportFormat,
    pub(super) declared_size_bytes: u64,
    pub(super) actual_size_bytes: u64,
    pub(super) source: Vec<u8>,
    pub(super) import_permit: OwnedSemaphorePermit,
    pub(super) started_at: Instant,
    pub(super) idle_since: Instant,
}

#[cfg(test)]
mod tests {
    use super::super::tests::capture_telemetry;
    use super::*;
    use rustix::io::Errno;
    use std::future::ready;
    use tokio::time::timeout;

    #[tokio::test]
    async fn mismatching_peer_uid_is_refused_and_closed() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let observed = server.peer_cred().unwrap().uid();
        let log = capture_telemetry(|| {
            assert!(admit_client_peer(server, observed.wrapping_add(1)).is_none());
        });
        assert!(log.contains(&format!("peer_uid={observed}")));
        assert_eq!(
            log.matches("client peer failed same-user admission")
                .count(),
            1
        );
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn matching_peer_uid_can_exchange_bytes() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = admit_client_peer(server, rustix::process::geteuid().as_raw()).unwrap();
        client.write_all(b"x").await.unwrap();
        assert_eq!(server.read_u8().await.unwrap(), b'x');
    }

    #[test]
    fn saturation_warns_once_per_episode_and_exports_occupancy() {
        let metrics = crate::telemetry::TelemetryMetrics::new().unwrap();
        let mut occupancy = ConnectionOccupancy {
            metrics: Some(metrics.clone()),
            saturated: false,
        };
        let first = capture_telemetry(|| {
            occupancy.observe(MAX_ACTIVE_CONNECTIONS - 1);
            occupancy.observe(MAX_ACTIVE_CONNECTIONS);
            occupancy.observe(MAX_ACTIVE_CONNECTIONS);
        });
        assert_eq!(first.matches("client connection cap reached").count(), 1);
        assert!(
            metrics
                .render()
                .unwrap()
                .contains("signalbox_client_connections_active 128\n")
        );
        let second = capture_telemetry(|| {
            occupancy.observe(MAX_ACTIVE_CONNECTIONS - 1);
            occupancy.observe(MAX_ACTIVE_CONNECTIONS);
        });
        assert_eq!(second.matches("client connection cap reached").count(), 1);
        drop(occupancy);
        assert!(
            metrics
                .render()
                .unwrap()
                .contains("signalbox_client_connections_active 0\n")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn partial_frame_expires_even_when_more_bytes_arrive() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = BufReader::new(ArrivalReader::new(server));
        let budget = Arc::new(Semaphore::new(1));
        let (_shutdown, mut shutdown) = watch::channel(false);
        client.write_all(b"{").await.unwrap();
        let frame = read_admitted_frame(
            &mut reader,
            budget.clone(),
            &mut shutdown,
            Some(Duration::from_secs(1)),
        );
        tokio::pin!(frame);
        assert!(
            timeout(Duration::from_millis(750), &mut frame)
                .await
                .is_err()
        );
        client.write_all(b" ").await.unwrap();
        let error = frame.await.err().expect("incomplete frame times out");
        assert!(
            matches!(error, ProcessConnectionError::PeerIo(error) if error.kind() == io::ErrorKind::TimedOut)
        );
        assert_eq!(budget.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pipelined_frame_deadline_includes_previous_request_handling() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = BufReader::new(ArrivalReader::new(server));
        let budget = Arc::new(Semaphore::new(1));
        let (_shutdown, mut shutdown) = watch::channel(false);
        let bound = Duration::from_secs(1);
        client.write_all(b"{}\n{\n{").await.unwrap();
        let received_at = Instant::now();
        for expected in [b"{}\n".as_slice(), b"{\n".as_slice()] {
            let (permit, line) =
                read_admitted_frame(&mut reader, budget.clone(), &mut shutdown, Some(bound))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(matches!(line, IncomingLine::Complete(bytes) if bytes == expected));
            drop(permit);
            // Each preceding request takes time while later input stays buffered.
            tokio::time::advance(Duration::from_millis(400)).await;
        }
        assert_eq!(reader.buffer(), b"{");
        let error = read_admitted_frame(&mut reader, budget, &mut shutdown, Some(bound))
            .await
            .err()
            .expect("pipelined partial frame times out");
        assert!(
            matches!(error, ProcessConnectionError::PeerIo(error) if error.kind() == io::ErrorKind::TimedOut)
        );
        assert_eq!(received_at.elapsed(), bound);
    }

    #[tokio::test(start_paused = true)]
    async fn socket_queued_frame_deadline_starts_during_previous_request_handling() {
        for handling_time in [Duration::from_millis(800), Duration::from_secs(2)] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let (server, _writer) = server.into_split();
            let mut reader = BufReader::new(ArrivalReader::new(server));
            let readiness = reader.get_ref().watch_readiness();
            let budget = Arc::new(Semaphore::new(1));
            let (_shutdown, mut shutdown) = watch::channel(false);
            let bound = Duration::from_secs(1);
            tokio::select! {
                biased;
                result = readiness => panic!("readiness watcher ended: {result:?}"),
                () = async {
                    client.write_all(b"{}\n").await.unwrap();
                    let first = read_admitted_frame(
                        &mut reader, budget.clone(), &mut shutdown, Some(bound),
                    ).await.unwrap().unwrap();
                    drop(first);
                    assert!(reader.buffer().is_empty());

                    // B arrives only after A has entered request handling.
                    client.write_all(b"{").await.unwrap();
                    reader.get_ref().get_ref().readable().await.unwrap();
                    let arrived = Instant::now();
                    sleep(handling_time).await;
                    assert!(reader.buffer().is_empty(), "B stays in the kernel buffer");
                    let error = read_admitted_frame(
                        &mut reader, budget, &mut shutdown, Some(bound),
                    ).await.err().expect("queued partial frame expires");
                    assert!(matches!(error, ProcessConnectionError::PeerIo(error)
                        if error.kind() == io::ErrorKind::TimedOut));
                    assert_eq!(arrived.elapsed(), handling_time.max(bound));
                } => {}
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn next_frame_uses_its_refill_arrival_after_an_idle_gap() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = BufReader::new(ArrivalReader::new(server));
        let budget = Arc::new(Semaphore::new(1));
        let (_shutdown, mut shutdown) = watch::channel(false);
        let bound = Duration::from_secs(1);
        client.write_all(b"{}\n").await.unwrap();
        let first = read_admitted_frame(&mut reader, budget.clone(), &mut shutdown, Some(bound))
            .await
            .unwrap()
            .unwrap();
        drop(first);
        assert!(reader.buffer().is_empty());
        tokio::time::advance(Duration::from_secs(60)).await;
        client.write_all(b"{").await.unwrap();
        let received_at = Instant::now();
        let error = read_admitted_frame(&mut reader, budget, &mut shutdown, Some(bound))
            .await
            .err()
            .expect("fresh partial frame times out after its own bound");
        assert!(
            matches!(error, ProcessConnectionError::PeerIo(error) if error.kind() == io::ErrorKind::TimedOut)
        );
        assert_eq!(received_at.elapsed(), bound);
    }

    #[tokio::test(start_paused = true)]
    async fn frame_deadline_includes_waiting_for_admission() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = BufReader::new(ArrivalReader::new(server));
        let budget = Arc::new(Semaphore::new(0));
        let (_shutdown, mut shutdown) = watch::channel(false);
        client.write_all(b"{}\n").await.unwrap();
        let error = read_admitted_frame(
            &mut reader,
            budget,
            &mut shutdown,
            Some(Duration::from_secs(1)),
        )
        .await
        .err()
        .expect("admission wait times out");
        assert!(
            matches!(error, ProcessConnectionError::PeerIo(error) if error.kind() == io::ErrorKind::TimedOut)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_connection_can_send_a_frame_after_the_deadline_duration() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (server, _writer) = server.into_split();
        let mut reader = BufReader::new(ArrivalReader::new(server));
        let budget = Arc::new(Semaphore::new(1));
        let (_shutdown, mut shutdown) = watch::channel(false);
        let frame = read_admitted_frame(
            &mut reader,
            budget,
            &mut shutdown,
            Some(Duration::from_secs(1)),
        );
        tokio::pin!(frame);
        assert!(timeout(Duration::from_secs(60), &mut frame).await.is_err());
        client.write_all(b"{}\n").await.unwrap();
        let (_, line) = frame.await.unwrap().unwrap();
        assert!(matches!(line, IncomingLine::Complete(bytes) if bytes == b"{}\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn transient_accept_failures_retry_until_a_connection_arrives() {
        let started = Instant::now();
        let mut retry_at = started;
        let mut attempts = [
            Err(Errno::CONNABORTED.into()),
            Err(Errno::MFILE.into()),
            Ok(()),
        ]
        .into_iter();
        let result = accept_with_retry(&mut retry_at, || {
            ready(attempts.next().expect("three accept outcomes"))
        })
        .await;
        assert!(result.is_ok());
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(attempts.next().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_listener_failure_is_not_retried() {
        let mut retry_at = Instant::now();
        let mut attempts = [Err::<(), _>(io::Error::from(Errno::BADF))].into_iter();
        let error = accept_with_retry(&mut retry_at, || {
            ready(attempts.next().expect("only one accept attempt"))
        })
        .await
        .expect_err("invalid listener fails");
        assert_eq!(error.raw_os_error(), Some(Errno::BADF.raw_os_error()));
    }

    #[tokio::test(start_paused = true)]
    async fn connection_completion_does_not_reset_accept_backoff() {
        let started = Instant::now();
        let mut retry_at = started;
        let mut calls = 0;
        tokio::select! {
            _ = sleep(Duration::from_millis(100)) => {},
            _ = accept_with_retry(&mut retry_at, || {
                calls += 1;
                ready(Err::<(), _>(io::Error::from(Errno::MFILE)))
            }) => panic!("recoverable failure must wait"),
        }
        assert_eq!(calls, 1);
        accept_with_retry(&mut retry_at, || ready(Ok(())))
            .await
            .expect("next connection accepted");
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn connection_frame_validation_failure_does_not_fail_the_runtime() {
        let failure = FrameEncodeError::Validation(
            signalbox_process_protocol::FrameValidationError::UnsupportedVersion,
        );
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::Encode(failure)))))
                .is_ok()
        );
    }

    #[test]
    fn connection_encoding_invariant_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::EncodeInvariant))))
                .is_ok()
        );
    }
}
