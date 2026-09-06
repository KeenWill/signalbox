use super::*;

pub(super) async fn write_process_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<CanonicalUuid>,
    error: ProcessReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ProcessReadError::Database(_) => ProtocolError::without_detail(ErrorCode::Unavailable),
        ProcessReadError::Corruption(_) => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::ProcessReadCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

/// Closed evidence for one server-side Internal response.
///
/// A variant owns both the operator class and cause code, preventing call sites
/// from pairing independent positional labels. No variant carries payload text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InternalDiagnostic {
    BlobReadIntegrity,
    ReviewWorkflowProjectionCorruption,
    ReviewOrchestrationStoreCorruption,
    ReviewOrchestrationWorkflowCorruption,
    ReviewOrchestrationSessionCorruption,
    ReviewOrchestrationServiceContract,
    ConversationImportAllocationFailure,
    ConversationImportContractDefect,
    ConversationImportWorkerTerminated,
    ImportedSessionDatabase,
    ImportedSessionCommitAmbiguous,
    ImportedSessionCommandKindMismatch,
    ImportedSessionPreparation,
    ImportedSessionIdentityCollision,
    ImportedSessionCorruption,
    ImportedConversationDatabase,
    ImportedConversationIdentityCollision,
    ImportedConversationCorruption,
    SessionDefaultsVersionMissing,
    SessionModelCredentialMissing,
    ContextCompactionRangeCorruption,
    ContextCompactionUnconfiguredTarget,
    ContextCompactionIdentityCollision,
    ContextCompactionRepositoryCorruption,
    ContextCompactionReadCorruption,
    ImportedFrontierRangeCorruption,
    TemplateSessionCreationCorruption,
    CommissionedDispatchCorruption,
    SessionCreationPreparation,
    SessionCreationDatabase,
    SessionCreationCommitAmbiguous,
    SessionCreationCommandKindMismatch,
    SessionCreationCorruption,
    ConversationListingCorruption,
    SessionMetadataDatabase,
    SessionMetadataCommitAmbiguous,
    SessionMetadataCommandKindMismatch,
    SessionMetadataCorruption,
    SessionDefaultsDatabase,
    SessionDefaultsCommitAmbiguous,
    SessionDefaultsCommandKindMismatch,
    SessionDefaultsCorruption,
    SessionDelegationDatabase,
    SessionDelegationCorruption,
    SessionDelegationContract,
    SystemPromptMemberMissing,
    SubmitInputCommandKindMismatch,
    SubmitInputIdentityCollision,
    SubmitInputCorruption,
    SubmitInputModelExecutionCorruption,
    SubmitInputModelExecutionIdentityCollision,
    SubmitInputModelExecutionNoLiveExecution,
    SubmitInputModelExecutionInvalidTransition,
    ToolLoopIdentityCollision,
    ToolLoopCorruption,
    ToolLoopInvalidTransition,
    ProcessReadCorruption,
    OperatorStatusCorruption,
    GoalRepositoryCorruption,
    SessionLifecycleCommandCorruption,
}

impl InternalDiagnostic {
    pub(super) const fn failure_class(self) -> OperatorFailureClass {
        match self {
            Self::ImportedSessionDatabase
            | Self::ImportedConversationDatabase
            | Self::SessionCreationDatabase
            | Self::SessionMetadataDatabase
            | Self::SessionDefaultsDatabase
            | Self::SessionDelegationDatabase => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::ImportedSessionCommitAmbiguous
            | Self::SessionCreationCommitAmbiguous
            | Self::SessionMetadataCommitAmbiguous
            | Self::SessionDefaultsCommitAmbiguous => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::ConversationImportAllocationFailure => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::ReviewOrchestrationServiceContract
            | Self::ConversationImportContractDefect
            | Self::ConversationImportWorkerTerminated
            | Self::ImportedSessionCommandKindMismatch
            | Self::ImportedSessionPreparation
            | Self::SessionCreationPreparation
            | Self::SessionCreationCommandKindMismatch
            | Self::SessionMetadataCommandKindMismatch
            | Self::SessionDefaultsCommandKindMismatch
            | Self::SessionDelegationContract
            | Self::ContextCompactionUnconfiguredTarget
            | Self::SystemPromptMemberMissing
            | Self::SubmitInputCommandKindMismatch
            | Self::SubmitInputModelExecutionNoLiveExecution
            | Self::SubmitInputModelExecutionInvalidTransition
            | Self::ToolLoopInvalidTransition => OperatorFailureClass::CallerOrHubBug,
            Self::ImportedSessionIdentityCollision
            | Self::ImportedConversationIdentityCollision
            | Self::ContextCompactionIdentityCollision
            | Self::SubmitInputIdentityCollision
            | Self::SubmitInputModelExecutionIdentityCollision
            | Self::ToolLoopIdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::BlobReadIntegrity
            | Self::ReviewWorkflowProjectionCorruption
            | Self::ReviewOrchestrationStoreCorruption
            | Self::ReviewOrchestrationWorkflowCorruption
            | Self::ReviewOrchestrationSessionCorruption
            | Self::ImportedSessionCorruption
            | Self::ImportedConversationCorruption
            | Self::SessionDefaultsVersionMissing
            | Self::SessionModelCredentialMissing
            | Self::ContextCompactionRangeCorruption
            | Self::ContextCompactionRepositoryCorruption
            | Self::ContextCompactionReadCorruption
            | Self::ImportedFrontierRangeCorruption
            | Self::TemplateSessionCreationCorruption
            | Self::CommissionedDispatchCorruption
            | Self::SessionCreationCorruption
            | Self::ConversationListingCorruption
            | Self::SessionMetadataCorruption
            | Self::SessionDefaultsCorruption
            | Self::SessionDelegationCorruption
            | Self::SubmitInputCorruption
            | Self::SubmitInputModelExecutionCorruption
            | Self::ToolLoopCorruption
            | Self::ProcessReadCorruption
            | Self::OperatorStatusCorruption
            | Self::GoalRepositoryCorruption
            | Self::SessionLifecycleCommandCorruption => OperatorFailureClass::FailClosedCorruption,
        }
    }

    pub(super) const fn cause_code(self) -> &'static str {
        match self {
            Self::BlobReadIntegrity => "blob_read_integrity",
            Self::ReviewWorkflowProjectionCorruption => "review_workflow_projection_corruption",
            Self::ReviewOrchestrationStoreCorruption => "review_orchestration_store_corruption",
            Self::ReviewOrchestrationWorkflowCorruption => {
                "review_orchestration_workflow_corruption"
            }
            Self::ReviewOrchestrationSessionCorruption => "review_orchestration_session_corruption",
            Self::ReviewOrchestrationServiceContract => "review_orchestration_service_contract",
            Self::ConversationImportAllocationFailure => "conversation_import_allocation_failure",
            Self::ConversationImportContractDefect => "conversation_import_contract_defect",
            Self::ConversationImportWorkerTerminated => "conversation_import_worker_terminated",
            Self::ImportedSessionDatabase => "imported_session_database",
            Self::ImportedSessionCommitAmbiguous => "imported_session_commit_ambiguous",
            Self::ImportedSessionCommandKindMismatch => "imported_session_command_kind_mismatch",
            Self::ImportedSessionPreparation => "imported_session_preparation",
            Self::ImportedSessionIdentityCollision => "imported_session_identity_collision",
            Self::ImportedSessionCorruption => "imported_session_corruption",
            Self::ImportedConversationDatabase => "imported_conversation_database",
            Self::ImportedConversationIdentityCollision => {
                "imported_conversation_identity_collision"
            }
            Self::ImportedConversationCorruption => "imported_conversation_corruption",
            Self::SessionDefaultsVersionMissing => "session_defaults_version_missing",
            Self::SessionModelCredentialMissing => "session_model_credential_missing",
            Self::ContextCompactionRangeCorruption => "context_compaction_range_corruption",
            Self::ContextCompactionUnconfiguredTarget => "context_compaction_unconfigured_target",
            Self::ContextCompactionIdentityCollision => {
                "context_compaction_repository_identity_collision"
            }
            Self::ContextCompactionRepositoryCorruption => {
                "context_compaction_repository_corruption"
            }
            Self::ContextCompactionReadCorruption => "context_compaction_read_corruption",
            Self::ImportedFrontierRangeCorruption => "imported_frontier_range_corruption",
            Self::TemplateSessionCreationCorruption => "template_session_creation_corruption",
            Self::CommissionedDispatchCorruption => "commissioned_dispatch_corruption",
            Self::SessionCreationPreparation => "session_creation_preparation",
            Self::SessionCreationDatabase => "session_creation_database",
            Self::SessionCreationCommitAmbiguous => "session_creation_commit_ambiguous",
            Self::SessionCreationCommandKindMismatch => "session_creation_command_kind_mismatch",
            Self::SessionCreationCorruption => "session_creation_corruption",
            Self::ConversationListingCorruption => "conversation_listing_corruption",
            Self::SessionMetadataDatabase => "session_metadata_database",
            Self::SessionMetadataCommitAmbiguous => "session_metadata_commit_ambiguous",
            Self::SessionMetadataCommandKindMismatch => "session_metadata_command_kind_mismatch",
            Self::SessionMetadataCorruption => "session_metadata_corruption",
            Self::SessionDefaultsDatabase => "session_defaults_database",
            Self::SessionDefaultsCommitAmbiguous => "session_defaults_commit_ambiguous",
            Self::SessionDefaultsCommandKindMismatch => "session_defaults_command_kind_mismatch",
            Self::SessionDefaultsCorruption => "session_defaults_corruption",
            Self::SessionDelegationDatabase => "session_delegation_database",
            Self::SessionDelegationCorruption => "session_delegation_corruption",
            Self::SessionDelegationContract => "session_delegation_contract",
            Self::SystemPromptMemberMissing => "system_prompt_member_missing",
            Self::SubmitInputCommandKindMismatch => "submit_input_command_kind_mismatch",
            Self::SubmitInputIdentityCollision => "submit_input_identity_collision",
            Self::SubmitInputCorruption => "submit_input_corruption",
            Self::SubmitInputModelExecutionCorruption => "submit_input_model_execution_corruption",
            Self::SubmitInputModelExecutionIdentityCollision => {
                "submit_input_model_execution_identity_collision"
            }
            Self::SubmitInputModelExecutionNoLiveExecution => {
                "submit_input_model_execution_no_live_execution"
            }
            Self::SubmitInputModelExecutionInvalidTransition => {
                "submit_input_model_execution_invalid_transition"
            }
            Self::ToolLoopIdentityCollision => "tool_loop_identity_collision",
            Self::ToolLoopCorruption => "tool_loop_corruption",
            Self::ToolLoopInvalidTransition => "tool_loop_invalid_transition",
            Self::ProcessReadCorruption => "process_read_corruption",
            Self::OperatorStatusCorruption => "operator_status_corruption",
            Self::GoalRepositoryCorruption => "goal_repository_corruption",
            Self::SessionLifecycleCommandCorruption => "session_lifecycle_command_corruption",
        }
    }
}

/// Records one typed internal diagnostic without choosing a wire response.
///
/// Present session identities use the same canonical UUID display as surrounding
/// spans; absent identities leave an empty field. Typed evidence contains only
/// closed labels, so request content, credentials, tool arguments, and nested
/// prose stay out.
pub(super) fn record_internal_diagnostic(
    session_id: Option<uuid::Uuid>,
    diagnostic: InternalDiagnostic,
) {
    let failure_class = diagnostic.failure_class();
    let cause_code = diagnostic.cause_code();
    match session_id {
        Some(session_id) => tracing::error!(
            ?failure_class,
            cause_code,
            session_id = %session_id,
            "request failed an internal integrity check"
        ),
        None => tracing::error!(
            ?failure_class,
            cause_code,
            session_id = tracing::field::Empty,
            "request failed an internal integrity check"
        ),
    }
}

/// Records a fail-closed Internal response before returning its wire shape.
///
/// Every Internal construction routes through this function.
pub(super) fn internal_protocol_error(
    session_id: Option<uuid::Uuid>,
    diagnostic: InternalDiagnostic,
) -> ProtocolError {
    record_internal_diagnostic(session_id, diagnostic);
    ProtocolError::without_detail(ErrorCode::Internal)
}

pub(super) fn unavailable_protocol_error(diagnostic: InternalDiagnostic) -> ProtocolError {
    let failure_class = diagnostic.failure_class();
    let cause_code = diagnostic.cause_code();
    tracing::error!(
        ?failure_class,
        cause_code,
        session_id = tracing::field::Empty,
        "requested operation is unavailable"
    );
    ProtocolError::without_detail(ErrorCode::Unavailable)
}

pub(super) async fn write_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ProtocolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::Error {
            code: error.code,
            message: error.message.to_owned(),
            detail: error.detail,
        },
    )
    .await
}

pub(super) async fn write_message<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let frame = ServerFrame::try_new_for_version(version, request_id, message)
        .map_err(FrameEncodeError::Validation)?;
    let encoded = encode_server_line(&frame)?;
    writer.write_all(&encoded).await?;
    Ok(())
}

/// Writes one system-prompt-bearing message through a temporary-file spool.
///
/// A prompt response can approach the frame cap, so the direct
/// `write_message` path would retain the complete encoded frame while a peer
/// that stops reading blocks the write. Spooling first keeps per-connection
/// heap at fixed I/O buffers, and a pre-transmission spool failure stays
/// request-local as the ordinary `unavailable` response — never fatal daemon
/// evidence and never peer I/O — mirroring the snapshot paths
/// (docs/spec/process-protocol.md).
pub(super) async fn write_message_via_spool<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_single_message(version, request_id, message).await;
    let mut file = match spool_result {
        Ok(file) => file,
        Err(error) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut file).await
}

/// Writes one committed mutation receipt through a temporary-file spool.
///
/// The receipt's mutation has already durably committed, so a pre-transmission
/// spool failure must answer `commit_ambiguous` — the caller retries the exact
/// command identity to discover the recorded outcome — never `unavailable`,
/// whose contract states no requested mutation may have committed
/// (docs/spec/process-protocol.md).
pub(super) async fn write_mutation_receipt_via_spool<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_single_message(version, request_id, message).await;
    let mut file = match spool_result {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                error = %spool_error_display(&error),
                "committed defaults receipt spooling failed before response"
            );
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
    };
    write_spooled_file(writer, &mut file).await
}

pub(super) fn spool_error_display(error: &SnapshotSpoolError) -> String {
    match error {
        SnapshotSpoolError::Io(error) => error.to_string(),
        SnapshotSpoolError::Encode(error) => error.to_string(),
        SnapshotSpoolError::EncodeInvariant => String::from("encode invariant violated"),
    }
}

/// Encodes one message into a rewound temporary-file spool, classifying every
/// failure before the first transmitted byte as a spool failure.
pub(super) async fn spool_single_message(
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(&mut file, version, request_id, message).await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

pub(super) async fn write_spool_message(
    writer: &mut tokio::fs::File,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), SnapshotSpoolError> {
    write_message(writer, version, request_id, message)
        .await
        .map_err(SnapshotSpoolError::from_connection)
}

pub(super) enum IncomingLine {
    Complete(Vec<u8>),
    Oversized {
        request_id: RequestId,
        admitted_version: Option<ProtocolVersion>,
    },
}

pub(super) async fn read_frame_line<Reader>(
    reader: &mut Reader,
) -> Result<Option<IncomingLine>, ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IncomingLine::Complete(line)))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            let frame_len = line.len().saturating_add(consumed);
            if frame_len > MAX_FRAME_BYTES {
                let (request_id, admitted_version) = if frame_len == MAX_FRAME_BYTES + 1 {
                    line.extend_from_slice(&available[..newline]);
                    (
                        recover_bounded_client_request_id(&line),
                        recover_bounded_client_protocol_version(&line),
                    )
                } else {
                    (RequestId::uncorrelated(), None)
                };
                reader.consume(consumed);
                return Ok(Some(IncomingLine::Oversized {
                    request_id,
                    admitted_version,
                }));
            }
            line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Complete(line)));
        }
        if line.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            let consumed = available.len();
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Oversized {
                request_id: RequestId::uncorrelated(),
                admitted_version: None,
            }));
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

pub(super) fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

pub(super) async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !shutdown_requested(shutdown) {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

pub(super) async fn run_until_shutdown<Output, Operation>(
    shutdown: &mut watch::Receiver<bool>,
    operation: Operation,
) -> Option<Output>
where
    Operation: Future<Output = Output>,
{
    tokio::select! {
        () = wait_for_shutdown(shutdown) => None,
        output = operation => Some(output),
    }
}

pub(super) fn wire_provider_failure_cause(
    cause: ProcessProviderModelCallFailureCause,
) -> FailedModelCallCause {
    match cause {
        ProcessProviderModelCallFailureCause::CredentialRejected => {
            FailedModelCallCause::CredentialRejected
        }
        ProcessProviderModelCallFailureCause::PermissionDenied => {
            FailedModelCallCause::PermissionDenied
        }
        ProcessProviderModelCallFailureCause::InvalidRequest => {
            FailedModelCallCause::InvalidRequest
        }
        ProcessProviderModelCallFailureCause::TargetNotFound => {
            FailedModelCallCause::TargetNotFound
        }
        ProcessProviderModelCallFailureCause::RequestTooLarge => {
            FailedModelCallCause::RequestTooLarge
        }
        ProcessProviderModelCallFailureCause::RateLimited => FailedModelCallCause::RateLimited,
        ProcessProviderModelCallFailureCause::QuotaExhausted => {
            FailedModelCallCause::QuotaExhausted
        }
        ProcessProviderModelCallFailureCause::Overloaded => FailedModelCallCause::Overloaded,
        ProcessProviderModelCallFailureCause::ProviderInternal => {
            FailedModelCallCause::ProviderInternal
        }
        ProcessProviderModelCallFailureCause::Unrecognized => FailedModelCallCause::Unrecognized,
    }
}

pub(super) fn wire_attachment_preparation_failure_cause(
    cause: signalbox_persistence::process_read::ProcessAttachmentPreparationFailureCause,
) -> FailedModelCallCause {
    use signalbox_persistence::process_read::ProcessAttachmentPreparationFailureCause;

    match cause {
        ProcessAttachmentPreparationFailureCause::TooLarge => {
            FailedModelCallCause::AttachmentTooLarge
        }
        ProcessAttachmentPreparationFailureCause::Missing => {
            FailedModelCallCause::AttachmentMissing
        }
        ProcessAttachmentPreparationFailureCause::Corrupt => {
            FailedModelCallCause::AttachmentCorrupt
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_goal_user_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_uuid: uuid::Uuid,
    session_id: CanonicalUuid,
    action: GoalUserAction,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let schedules_turn = match &action {
        GoalUserAction::Attach(_) | GoalUserAction::Resume(_) | GoalUserAction::Supersede(_) => {
            true
        }
        GoalUserAction::Stop { .. } => false,
    };
    let command = GoalUserCommand::new(DurableCommandId::from_uuid(command_uuid), session, action);
    let candidates = schedules_turn.then(|| {
        GoalTurnCandidates::new(
            AcceptedInputId::from_uuid(uuid::Uuid::now_v7()),
            TurnId::from_uuid(uuid::Uuid::now_v7()),
        )
    });
    let outcome = GoalRepository::new(services.pool.clone())
        .handle_user_command(command, candidates, |alias| {
            services.model_configuration.resolve_alias(alias)
        })
        .await;
    match outcome {
        Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(event))) => {
            if schedules_turn {
                let _ = services.eligibility_nudge.nudge(session);
            }
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::GoalTransitionApplied {
                    session_id,
                    event_ordinal: CanonicalU64::new(event.ordinal().get()),
                    generation: CanonicalU64::new(event.generation().get()),
                },
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(reason))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::GoalCommandRejected {
                    session_id,
                    reason: wire_goal_command_rejection(reason),
                }),
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::TargetBusy {
            session: blocking_session,
        }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::CommissionTargetBusy {
                    session_id: CanonicalUuid::from_uuid(blocking_session.into_uuid()),
                }),
            )
            .await
        }
        // A client goal command names no expected lineage head, so it applies
        // to whatever state the session lock reveals. Reaching this answer
        // means the repository decided a question this request never asked.
        Ok(GoalCommandHandlingOutcome::LineageMoved) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
        Err(error) => {
            write_goal_repository_error(
                writer,
                version,
                request_id,
                Some(session_id.into_uuid()),
                error,
            )
            .await
        }
    }
}

pub(super) async fn handle_session_lifecycle_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_uuid: uuid::Uuid,
    session_id: CanonicalUuid,
    operation: SessionLifecycleOperation,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let command = SessionLifecycleCommand::new(
        DurableCommandId::from_uuid(command_uuid),
        session,
        operation,
    );
    let outcome = SessionLifecycleCommandRepository::new(services.pool.clone())
        .handle(command.clone(), CommandPrincipal::Operator)
        .await;
    match outcome {
        Ok(SessionLifecycleCommandHandlingOutcome::Recorded(
            SessionLifecycleCommandResult::Applied(application),
        )) => {
            if lifecycle_command_needs_eligibility_nudge(&application, command.operation()) {
                let _ = services.eligibility_nudge.nudge(session);
            }
            if matches!(command.operation(), SessionLifecycleOperation::Adopt { .. })
                && let Some(goal_resumption) = &services.goal_resumption
            {
                goal_resumption.arm_blocked_goal_resumption(session);
            }
            if let SessionLifecycleApplication::ClosurePending {
                live_turn,
                defaults_version,
                ..
            } = application
                && !closure_settled(services, session).await
                && interrupt_for_closure(services, &command, live_turn, defaults_version)
                    .await
                    .is_err()
            {
                // The closure is committed; a retransmission replays it and
                // re-issues the interrupt.
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::mutation_commit_ambiguous(),
                )
                .await;
            }
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionLifecycleCommandApplied {
                    session_id,
                    effect: wire_lifecycle_effect(application),
                },
            )
            .await
        }
        Ok(SessionLifecycleCommandHandlingOutcome::Recorded(
            SessionLifecycleCommandResult::Rejected(reason),
        )) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::SessionLifecycleCommandRejected {
                    session_id,
                    reason: wire_lifecycle_rejection(reason),
                }),
            )
            .await
        }
        Ok(SessionLifecycleCommandHandlingOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionLifecycleCommandRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionLifecycleCommandRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            SessionLifecycleCommandRepositoryError::Corruption(_)
            | SessionLifecycleCommandRepositoryError::Lifecycle(_),
        ) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionLifecycleCommandCorruption,
                ),
            )
            .await
        }
    }
}

pub(super) fn lifecycle_command_needs_eligibility_nudge(
    application: &SessionLifecycleApplication,
    operation: &SessionLifecycleOperation,
) -> bool {
    *application == SessionLifecycleApplication::StartReleased
        || matches!(
            operation,
            SessionLifecycleOperation::Release | SessionLifecycleOperation::Resume
        )
}

/// Hands a committed closure's live turn to the committed interrupt
/// machinery under a fresh core-owned identity.
pub(super) async fn interrupt_for_closure(
    services: &ConnectionServices,
    command: &SessionLifecycleCommand,
    live_turn: TurnId,
    expected_version: SessionConfigurationDefaultsVersion,
) -> Result<(), ()> {
    let session = command.session();
    let (descendant_scope, cascade_root_kind) = match command.operation() {
        SessionLifecycleOperation::Stop {
            descendant_scope, ..
        } => (*descendant_scope, ParentTerminationKind::Stopped),
        _ => (
            DescendantTerminationScope::ParentAlone,
            ParentTerminationKind::Cancelled,
        ),
    };
    let Ok(content) = UserContent::try_text(String::from("The session was closed.")) else {
        return Err(());
    };
    let selected_model = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT direct_selection_id
           FROM turn_origin_effective_model_configuration($1, $2)",
    )
    .bind(live_turn.into_uuid())
    .bind(session.into_uuid())
    .fetch_optional(&services.pool)
    .await
    .map_err(|error| {
        tracing::warn!(session = %session.into_uuid(), cause = %error,
            "closure interrupt could not load its live turn configuration");
    })?
    .ok_or_else(|| {
        tracing::warn!(session = %session.into_uuid(), turn = %live_turn.into_uuid(),
            "closure interrupt live turn has no effective model configuration");
    })?;
    let request = SubmitInputRequest::try_new_core_interrupt(
        DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
        session,
        content,
        live_turn,
        descendant_scope,
        PerInputConfigurationChoices::new(
            expected_version,
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(selected_model),
            )),
        ),
    );
    let Ok(request) = request else {
        return Err(());
    };
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository: SubmitInputRepository::new(services.pool.clone()),
            model_configuration: services.model_configuration.as_ref(),
            principal: CommandPrincipal::Core,
            cascade_root_kind,
        },
        services.eligibility_nudge.clone(),
        services.tool_dispatch_gate.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(_))) => Ok(()),
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptAlreadyApplied {
                session: rejected_session,
                active_turn,
                ..
            },
        ))) if rejected_session == session && active_turn == live_turn => Ok(()),
        Ok(other) => {
            if closure_settled(services, session).await {
                return Ok(());
            }
            tracing::warn!(session = %session.into_uuid(), outcome = ?other,
                "closure interrupt was not applied");
            Err(())
        }
        Err(error) => {
            tracing::warn!(session = %session.into_uuid(), cause = %error,
                "closure interrupt failed");
            Err(())
        }
    }
}

/// A closure whose live turn ended between the command and its interrupt has
/// already settled through the deferred trigger; the interrupt's rejection is
/// then not a failure to report.
pub(super) async fn closure_settled(services: &ConnectionServices, session: SessionId) -> bool {
    matches!(
        SessionLifecycleRepository::new(services.pool.clone())
            .load(session)
            .await,
        Ok(Some(record)) if record.state().is_terminal()
    )
}

pub(super) const fn wire_lifecycle_effect(
    value: SessionLifecycleApplication,
) -> SessionLifecycleEffect {
    match value {
        SessionLifecycleApplication::StartReleased => SessionLifecycleEffect::StartReleased {},
        SessionLifecycleApplication::Closed { .. } => SessionLifecycleEffect::Closed {},
        SessionLifecycleApplication::ClosurePending { live_turn, .. } => {
            SessionLifecycleEffect::ClosurePending {
                live_turn_id: CanonicalUuid::from_uuid(live_turn.into_uuid()),
            }
        }
        SessionLifecycleApplication::Resumed { .. } => SessionLifecycleEffect::Resumed {},
        SessionLifecycleApplication::OwnershipChanged => {
            SessionLifecycleEffect::OwnershipChanged {}
        }
    }
}

pub(super) const fn wire_lifecycle_rejection(
    value: DomainLifecycleRejection,
) -> WireLifecycleRejection {
    match value {
        DomainLifecycleRejection::SessionNotFound => WireLifecycleRejection::SessionNotFound,
        DomainLifecycleRejection::TransitionNotAdmitted => {
            WireLifecycleRejection::TransitionNotAdmitted
        }
        DomainLifecycleRejection::RequiresParked => WireLifecycleRejection::RequiresParked,
        DomainLifecycleRejection::ReleaseWhileParked => WireLifecycleRejection::ReleaseWhileParked,
        DomainLifecycleRejection::OwnershipUnchanged => WireLifecycleRejection::OwnershipUnchanged,
        DomainLifecycleRejection::FinishConditionAlreadyDeclared => {
            WireLifecycleRejection::FinishConditionAlreadyDeclared
        }
        DomainLifecycleRejection::StandingCauseMismatch => {
            WireLifecycleRejection::StandingCauseMismatch
        }
        DomainLifecycleRejection::SuccessorNotFound => WireLifecycleRejection::SuccessorNotFound,
        DomainLifecycleRejection::SuccessorIsSelf => WireLifecycleRejection::SuccessorIsSelf,
        DomainLifecycleRejection::GoalResumeRequired => WireLifecycleRejection::GoalResumeRequired,
        DomainLifecycleRejection::GoalOutcomeMismatch => {
            WireLifecycleRejection::GoalOutcomeMismatch
        }
        DomainLifecycleRejection::PendingTerminalConflict => {
            WireLifecycleRejection::PendingTerminalConflict
        }
    }
}

pub(super) const fn domain_session_failure_cause(
    value: WireSessionFailureCause,
) -> DomainSessionFailureCause {
    match value {
        WireSessionFailureCause::ProviderTransient => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderTransient)
        }
        WireSessionFailureCause::ProviderQuotaExhausted => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderQuotaExhausted)
        }
        WireSessionFailureCause::ProviderOverloaded => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderOverloaded)
        }
        WireSessionFailureCause::InfrastructureFailure => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::InfrastructureFailure)
        }
        WireSessionFailureCause::RetryBudgetExhausted => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::RetryBudgetExhausted)
        }
        WireSessionFailureCause::ContextCompactionWall => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ContextCompactionWall)
        }
        WireSessionFailureCause::ContextHeadroomExhausted => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ContextHeadroomExhausted)
        }
        WireSessionFailureCause::BrokenToolchain => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::BrokenToolchain)
        }
        WireSessionFailureCause::ModerationBlock => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ModerationBlock)
        }
    }
}

pub(super) async fn handle_read_goal<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let loaded = GoalRepository::new(pool.clone())
        .load_goal(SessionId::from_uuid(session_id.into_uuid()))
        .await;
    let goal = match loaded {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(error) => {
            drop(snapshot_permit);
            return write_goal_repository_error(
                writer,
                version,
                request_id,
                Some(session_id.into_uuid()),
                error,
            )
            .await;
        }
    };
    let spool_result = spool_goal_snapshot(&goal, version, request_id, session_id).await;
    drop(goal);
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(error) => return write_snapshot_spool_error(writer, version, request_id, error).await,
    };
    write_spooled_file(writer, &mut spool).await
}

pub(super) async fn spool_goal_snapshot(
    goal: &Goal,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryStart {
            session_id,
            current_generation: CanonicalU64::new(goal.current().generation().get()),
            current_statement: goal.current().statement().as_str().to_owned(),
        },
    )
    .await?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryState {
            current_state: wire_goal_state(goal.current().state()),
        },
    )
    .await?;
    for event in goal.events() {
        let wire_event = wire_goal_event(event).map_err(SnapshotSpoolError::from_connection)?;
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(event.ordinal().get()),
                generation: CanonicalU64::new(event.generation().get()),
                event: wire_event,
            },
        )
        .await?;
    }
    let event_count =
        u64::try_from(goal.events().len()).map_err(|_| SnapshotSpoolError::EncodeInvariant)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryEnd {
            event_count: CanonicalU64::new(event_count),
        },
    )
    .await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

pub(super) fn wire_goal_state(state: &GoalState) -> GoalLifecycleState {
    match state {
        GoalState::Pursuing => GoalLifecycleState::Pursuing {},
        GoalState::Blocked { reason, need } => GoalLifecycleState::Blocked {
            reason: wire_goal_blocked_reason(*reason),
            need: need.as_str().to_owned(),
        },
        GoalState::Achieved { report } => GoalLifecycleState::Achieved {
            turn_id: wire_uuid(report.turn().into_uuid()),
            tool_request_id: wire_uuid(report.tool_request().into_uuid()),
        },
        GoalState::UserStopped => GoalLifecycleState::UserStopped {},
        GoalState::Superseded { by_generation } => GoalLifecycleState::Superseded {
            by_generation: CanonicalU64::new(by_generation.get()),
        },
        GoalState::SessionClosed { outcome } => GoalLifecycleState::SessionClosed {
            outcome: wire_session_closure_outcome(*outcome),
        },
    }
}

pub(super) fn wire_session_closure_outcome(
    outcome: signalbox_domain::SessionClosureOutcome,
) -> SessionClosureOutcome {
    match outcome {
        signalbox_domain::SessionClosureOutcome::FailedRetryable => {
            SessionClosureOutcome::FailedRetryable
        }
        signalbox_domain::SessionClosureOutcome::FailedStructural => {
            SessionClosureOutcome::FailedStructural
        }
        signalbox_domain::SessionClosureOutcome::FailedUnknown => {
            SessionClosureOutcome::FailedUnknown
        }
        signalbox_domain::SessionClosureOutcome::Stopped => SessionClosureOutcome::Stopped,
        signalbox_domain::SessionClosureOutcome::Superseded => SessionClosureOutcome::Superseded,
        signalbox_domain::SessionClosureOutcome::Abandoned => SessionClosureOutcome::Abandoned,
        signalbox_domain::SessionClosureOutcome::Retired => SessionClosureOutcome::Retired,
    }
}

pub(super) fn wire_lifecycle_actor(actor: signalbox_domain::LifecycleActor) -> LifecycleActorClass {
    match actor {
        signalbox_domain::LifecycleActor::Core { .. } => LifecycleActorClass::Core,
        signalbox_domain::LifecycleActor::Operator => LifecycleActorClass::Operator,
        signalbox_domain::LifecycleActor::Module { .. } => LifecycleActorClass::Module,
        signalbox_domain::LifecycleActor::Watchdog => LifecycleActorClass::Watchdog,
    }
}

pub(super) fn wire_goal_event(
    event: &GoalEvent,
) -> Result<GoalHistoryEvent, ProcessConnectionError> {
    match event.kind() {
        GoalEventKind::Commissioned {
            statement,
            provenance,
        } => Ok(GoalHistoryEvent::Commissioned {
            statement: statement.as_str().to_owned(),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Blocked { block, need } => Ok(GoalHistoryEvent::Blocked {
            reason: wire_goal_blocked_reason(block.reason_kind()),
            need: need.as_str().to_owned(),
            provenance: wire_goal_blocked_provenance(*block),
        }),
        GoalEventKind::Resumed {
            guidance,
            provenance,
        } => Ok(GoalHistoryEvent::Resumed {
            guidance: guidance.as_ref().map(|value| value.as_str().to_owned()),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Achieved { report, provenance } => Ok(GoalHistoryEvent::Achieved {
            report: report.as_str().to_owned(),
            turn_id: wire_uuid(provenance.turn().into_uuid()),
            tool_request_id: wire_uuid(provenance.tool_request().into_uuid()),
        }),
        GoalEventKind::UserStopped { provenance } => Ok(GoalHistoryEvent::UserStopped {
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Superseded {
            replacement_statement,
            provenance,
        } => Ok(GoalHistoryEvent::Superseded {
            replacement_statement: replacement_statement.as_str().to_owned(),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::SessionClosed {
            outcome,
            provenance,
        } => Ok(GoalHistoryEvent::SessionClosed {
            outcome: wire_session_closure_outcome(*outcome),
            actor: wire_lifecycle_actor(*provenance),
        }),
    }
}

pub(super) fn wire_goal_blocked_provenance(
    value: GoalBlockProvenance,
) -> WireGoalBlockedProvenance {
    match value {
        GoalBlockProvenance::Model { provenance, .. }
        | GoalBlockProvenance::FinishCheck { provenance } => WireGoalBlockedProvenance::Model {
            turn_id: wire_uuid(provenance.turn().into_uuid()),
            tool_request_id: wire_uuid(provenance.tool_request().into_uuid()),
        },
        GoalBlockProvenance::ExecutionFailure { provenance } => {
            WireGoalBlockedProvenance::ExecutionFailure {
                turn_id: wire_uuid(provenance.turn().into_uuid()),
            }
        }
    }
}

pub(super) const fn wire_goal_blocked_reason(
    value: GoalBlockedReasonKind,
) -> WireGoalBlockedReason {
    match value {
        GoalBlockedReasonKind::UserInputRequired => WireGoalBlockedReason::UserInputRequired,
        GoalBlockedReasonKind::ExternalChangeRequired => {
            WireGoalBlockedReason::ExternalChangeRequired
        }
        GoalBlockedReasonKind::AuthorizationRequired => {
            WireGoalBlockedReason::AuthorizationRequired
        }
        GoalBlockedReasonKind::ExecutionFailure => WireGoalBlockedReason::ExecutionFailure,
        GoalBlockedReasonKind::FinishCheckFailed => WireGoalBlockedReason::FinishCheckFailed,
    }
}

pub(super) const fn wire_goal_command_rejection(
    value: DomainGoalCommandRejection,
) -> WireGoalCommandRejection {
    match value {
        DomainGoalCommandRejection::SessionNotFound => WireGoalCommandRejection::SessionNotFound,
        DomainGoalCommandRejection::SessionClosing => WireGoalCommandRejection::SessionClosing,
        DomainGoalCommandRejection::GoalAlreadyAttached => {
            WireGoalCommandRejection::GoalAlreadyAttached
        }
        DomainGoalCommandRejection::GoalNotAttached => WireGoalCommandRejection::GoalNotAttached,
        DomainGoalCommandRejection::UnknownModelAlias => {
            WireGoalCommandRejection::UnknownModelAlias
        }
        DomainGoalCommandRejection::AcceptancePositionExhausted => {
            WireGoalCommandRejection::AcceptancePositionExhausted
        }
        DomainGoalCommandRejection::RequiresBlocked => WireGoalCommandRejection::RequiresBlocked,
        DomainGoalCommandRejection::RequiresPursuingOrBlocked => {
            WireGoalCommandRejection::RequiresPursuingOrBlocked
        }
        DomainGoalCommandRejection::GenerationExhausted => {
            WireGoalCommandRejection::GenerationExhausted
        }
        DomainGoalCommandRejection::EventOrdinalExhausted => {
            WireGoalCommandRejection::EventOrdinalExhausted
        }
    }
}

pub(super) fn wire_goal_command_id(
    value: DurableCommandId,
) -> Result<signalbox_process_protocol::CommandId, ProcessConnectionError> {
    signalbox_process_protocol::CommandId::try_from_uuid(value.into_uuid())
        .map_err(|_| ProcessConnectionError::EncodeInvariant)
}

pub(super) async fn write_goal_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<uuid::Uuid>,
    error: GoalRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        GoalRepositoryError::Database(_) => ProtocolError::mutation_definitely_unavailable(),
        GoalRepositoryError::CommitAmbiguous(_) => ProtocolError::mutation_commit_ambiguous(),
        GoalRepositoryError::DifferentCommandKind { .. } => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        GoalRepositoryError::Corruption(_) => {
            internal_protocol_error(session_id, InternalDiagnostic::GoalRepositoryCorruption)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

pub(super) fn wire_uuid(value: uuid::Uuid) -> CanonicalUuid {
    CanonicalUuid::from_uuid(value)
}

pub(super) struct ProtocolError {
    pub(super) code: ErrorCode,
    pub(super) message: &'static str,
    pub(super) detail: ErrorDetail,
}

impl ProtocolError {
    /// The selected session exists but the named immutable defaults epoch
    /// was never installed; the wire code remains the shared `not_found`.
    pub(super) const fn defaults_epoch_not_found() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested defaults epoch was not found on the selected session",
            detail: ErrorDetail::none(),
        }
    }

    /// No imported conversation has the named identity. The absent read target
    /// is never a session; the wire code remains the shared `not_found`.
    pub(super) const fn imported_conversation_absent() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested imported conversation was not found",
            detail: ErrorDetail::none(),
        }
    }

    pub(super) const fn blob_not_found() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested blob was not found",
            detail: ErrorDetail::none(),
        }
    }

    pub(super) const fn without_detail(code: ErrorCode) -> Self {
        Self {
            code,
            message: match code {
                ErrorCode::MalformedFrame => "the protocol frame is malformed",
                ErrorCode::UnsupportedVersion => {
                    "the protocol version is unsupported; supported version: 1"
                }
                ErrorCode::InvalidRequest => "the request values are invalid",
                ErrorCode::NotFound => "the requested session was not found",
                ErrorCode::BlobMissing => "all recorded blob replicas are missing",
                ErrorCode::BlobCorrupt => "all usable blob replicas are corrupt",
                ErrorCode::ConflictingReuse => {
                    "the command identity already names different intent"
                }
                ErrorCode::Rejected => "the command was rejected by current durable state",
                ErrorCode::ResyncRequired => {
                    "the follow stream fell behind; reconnect for a fresh snapshot"
                }
                ErrorCode::Unavailable => "the requested operation is unavailable",
                ErrorCode::PublicationAmbiguous => {
                    "the blob publication is ambiguous; retry the exact upload"
                }
                ErrorCode::CommitAmbiguous => {
                    "the mutation commit is ambiguous; retry the exact command"
                }
                ErrorCode::Internal => "the request failed an internal integrity check",
            },
            detail: ErrorDetail::none(),
        }
    }

    pub(super) const fn invalid_import(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "conversation import was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    pub(super) const fn invalid_blob_upload(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "blob upload was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    pub(super) const fn invalid_blob_read(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "blob read was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    pub(super) const fn invalid_bulk_ingest(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "bulk ingest was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    pub(super) const fn mutation_definitely_unavailable() -> Self {
        Self::without_detail(ErrorCode::Unavailable)
    }

    pub(super) const fn mutation_commit_ambiguous() -> Self {
        Self::without_detail(ErrorCode::CommitAmbiguous)
    }

    pub(super) const fn mutation_unavailable(commit_ambiguous: bool) -> Self {
        if commit_ambiguous {
            Self::mutation_commit_ambiguous()
        } else {
            Self::mutation_definitely_unavailable()
        }
    }

    pub(super) const fn rejected(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::Rejected,
            message: "the command was rejected by current durable state",
            detail: ErrorDetail::rejected(detail),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum ProcessUpdate {
    Durable {
        cursor: u64,
        session: SessionId,
        event: ProcessUpdateEvent,
    },
    ProviderTextDelta(ProviderTextDelta),
}

impl ProcessUpdate {
    pub(super) fn from_outbox(event: &DispatchedOutboxEvent) -> Option<Self> {
        Some(Self::Durable {
            cursor: event.sequence(),
            session: event.session()?,
            event: ProcessUpdateEvent::from_outbox(event.kind())?,
        })
    }
}

#[derive(Clone, Debug)]
pub(super) enum ProcessUpdateEvent {
    SessionCreated,
    SessionModelSettingsChanged(DomainSessionModelSettingsChanged),
    TurnModelSettingsResolved(DomainTurnModelSettingsResolved),
    InputAccepted {
        accepted_input: signalbox_domain::AcceptedInputId,
        turn: signalbox_domain::TurnId,
        acceptance_position: u64,
        content: UserContent,
    },
    GoalTurnRetired {
        turn: signalbox_domain::TurnId,
    },
    TurnActivated {
        turn: signalbox_domain::TurnId,
        current_attempt: signalbox_domain::TurnAttemptId,
    },
    ModelCallTransition {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        state: DispatchedModelCallState,
    },
    ToolBatchTransition {
        turn: signalbox_domain::TurnId,
        producing_call: signalbox_domain::ModelCallId,
        state: DispatchedToolBatchState,
    },
    ToolApprovalDecided {
        turn: signalbox_domain::TurnId,
        approval: signalbox_domain::ToolApprovalResolution,
        decider: signalbox_domain::ToolApprovalDecider,
    },
    RunnerStateTransition {
        runner: signalbox_domain::RunnerId,
        placement_revision: signalbox_domain::RunnerGeneration,
        sandbox: signalbox_domain::RunnerSandboxProfile,
        working_directory: Option<signalbox_domain::RunnerWorkingDirectory>,
        state: DispatchedRunnerState,
    },
    ContextCompacted {
        compaction: signalbox_domain::ContextCompactionId,
        call: signalbox_domain::ModelCallId,
        through_position: u64,
        summary_entry: signalbox_domain::SemanticTranscriptEntryId,
        result_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnCompleted {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        completion_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnFailed {
        turn: signalbox_domain::TurnId,
        failure_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnRefused {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnCancelled {
        turn: signalbox_domain::TurnId,
        cancellation_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnReconciliationRequired {
        turn: signalbox_domain::TurnId,
        operation: DispatchedReconciliationOperation,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    DelegationUpdate(DispatchedDelegationUpdate),
}

impl ProcessUpdateEvent {
    pub(super) fn from_outbox(event: &DispatchedOutboxEventKind) -> Option<Self> {
        Some(match event {
            DispatchedOutboxEventKind::SessionCreated(_) => Self::SessionCreated,
            // The module-facing lifecycle kinds have no wire projection yet.
            DispatchedOutboxEventKind::SessionStateChanged(_)
            | DispatchedOutboxEventKind::SessionTerminal(_)
            | DispatchedOutboxEventKind::GoalChanged(_)
            | DispatchedOutboxEventKind::CommandSettled { .. }
            | DispatchedOutboxEventKind::InjectionSettled { .. }
            | DispatchedOutboxEventKind::SessionOwnershipChanged(_) => return None,
            DispatchedOutboxEventKind::SessionModelSettingsChanged(event) => {
                Self::SessionModelSettingsChanged(event.clone())
            }
            DispatchedOutboxEventKind::TurnModelSettingsResolved(event) => {
                Self::TurnModelSettingsResolved(event.clone())
            }
            DispatchedOutboxEventKind::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => Self::InputAccepted {
                accepted_input: *accepted_input,
                turn: *turn,
                acceptance_position: acceptance_position.as_u64(),
                content: content.clone(),
            },
            DispatchedOutboxEventKind::TurnTerminal { turn, disposition } => match disposition {
                DispatchedTurnTerminalDisposition::Completed {
                    call,
                    completion_entry,
                    terminal_frontier,
                } => Self::TurnCompleted {
                    turn: *turn,
                    call: *call,
                    completion_entry: *completion_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Refused {
                    call,
                    terminal_frontier,
                } => Self::TurnRefused {
                    turn: *turn,
                    call: *call,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Failed {
                    failure_entry,
                    terminal_frontier,
                } => Self::TurnFailed {
                    turn: *turn,
                    failure_entry: *failure_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Cancelled {
                    cancellation_entry,
                    terminal_frontier,
                } => Self::TurnCancelled {
                    turn: *turn,
                    cancellation_entry: *cancellation_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation,
                    terminal_frontier,
                } => Self::TurnReconciliationRequired {
                    turn: *turn,
                    operation: *operation,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Retired => Self::GoalTurnRetired { turn: *turn },
            },
            DispatchedOutboxEventKind::TurnActivated {
                turn,
                current_attempt,
            } => Self::TurnActivated {
                turn: *turn,
                current_attempt: *current_attempt,
            },
            DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
                Self::ModelCallTransition {
                    turn: *turn,
                    call: *call,
                    state: *state,
                }
            }
            DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => Self::ToolBatchTransition {
                turn: *turn,
                producing_call: *producing_call,
                state: *state,
            },
            DispatchedOutboxEventKind::ToolApprovalDecided {
                turn,
                approval,
                decider,
            } => Self::ToolApprovalDecided {
                turn: *turn,
                approval: approval.clone(),
                decider: *decider,
            },
            DispatchedOutboxEventKind::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox,
                working_directory,
                state,
            } => Self::RunnerStateTransition {
                runner: *runner,
                placement_revision: *placement_revision,
                sandbox: *sandbox,
                working_directory: working_directory.clone(),
                state: *state,
            },
            DispatchedOutboxEventKind::ContextCompacted {
                compaction,
                call,
                through_position,
                summary_entry,
                result_frontier,
            } => Self::ContextCompacted {
                compaction: *compaction,
                call: *call,
                through_position: *through_position,
                summary_entry: *summary_entry,
                result_frontier: *result_frontier,
            },
            DispatchedOutboxEventKind::DelegationUpdate(update) => {
                Self::DelegationUpdate(update.clone())
            }
            DispatchedOutboxEventKind::DelegationWake(_) => return None,
        })
    }

    pub(super) fn wire(&self) -> Result<SessionEvent, ProcessConnectionError> {
        let event = match self {
            Self::SessionCreated => SessionEvent::SessionCreated {},
            Self::SessionModelSettingsChanged(event) => SessionEvent::SessionModelSettingsChanged {
                command_id: signalbox_process_protocol::CommandId::try_from_uuid(
                    event.command_id().into_uuid(),
                )
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                prior_defaults_version: CanonicalU64::new(event.prior_defaults_version().as_u64()),
                installed_defaults_version: CanonicalU64::new(
                    event.installed_defaults_version().as_u64(),
                ),
                prior_model: wire_domain_model_selection(event.prior_model()),
                installed_model: wire_domain_model_selection(event.installed_model()),
                prior_settings: wire_model_settings(event.prior_settings()),
                installed_settings: wire_model_settings(event.installed_settings()),
                caller_override: wire_model_settings_overlay(event.caller_override()),
                adjustments: event
                    .adjustments()
                    .iter()
                    .copied()
                    .map(wire_model_change_adjustment)
                    .collect(),
            },
            Self::TurnModelSettingsResolved(event) => SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: wire_uuid(event.accepted_input().into_uuid()),
                turn_id: wire_uuid(event.turn().into_uuid()),
                defaults_version: CanonicalU64::new(event.defaults_version().as_u64()),
                requested_model: wire_frozen_model_selection(event.selection()),
                selected_direct_id: wire_uuid(event.selection().selected_direct().into_uuid()),
                per_call_override: wire_model_settings_overlay(event.per_call_override()),
                settings: wire_model_settings(event.settings()),
                adjusted_from_selection_id: event
                    .adjusted_from_selection()
                    .map(|selection| wire_uuid(selection.into_uuid())),
                adjustments: event
                    .adjustments()
                    .iter()
                    .copied()
                    .map(wire_model_change_adjustment)
                    .collect(),
            },
            Self::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => SessionEvent::InputAccepted {
                accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                turn_id: wire_uuid(turn.into_uuid()),
                acceptance_position: CanonicalU64::new(*acceptance_position),
                content: wire_user_content(content),
            },
            Self::GoalTurnRetired { turn } => SessionEvent::GoalTurnRetired {
                turn_id: wire_uuid(turn.into_uuid()),
            },
            Self::TurnActivated {
                turn,
                current_attempt,
            } => SessionEvent::TurnActivated {
                turn_id: wire_uuid(turn.into_uuid()),
                current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            },
            Self::ModelCallTransition { turn, call, state } => SessionEvent::ModelCallTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                state: wire_model_call_state(*state),
            },
            Self::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => SessionEvent::ToolBatchTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(producing_call.into_uuid()),
                state: match state {
                    DispatchedToolBatchState::Proposed { frontier } => ToolBatchState::Proposed {
                        frontier_id: wire_uuid(frontier.into_uuid()),
                    },
                    DispatchedToolBatchState::ResultsProjected { frontier } => {
                        ToolBatchState::ResultsProjected {
                            frontier_id: wire_uuid(frontier.into_uuid()),
                        }
                    }
                    DispatchedToolBatchState::RecoveryRequired { attempt } => {
                        ToolBatchState::RecoveryRequired {
                            tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        }
                    }
                },
            },
            Self::ToolApprovalDecided {
                turn,
                approval,
                decider,
            } => {
                let decision = match approval.decision() {
                    ToolApprovalDecision::Approve => WireToolApprovalEventDecision::Approve {},
                    ToolApprovalDecision::Deny { reason } => WireToolApprovalEventDecision::Deny {
                        reason: reason.as_ref().map(|value| value.as_str().to_owned()),
                    },
                };
                let decider = match decider {
                    signalbox_domain::ToolApprovalDecider::User { command } => {
                        WireToolApprovalEventDecider::User {
                            command_id: wire_uuid(command.into_uuid()),
                        }
                    }
                    signalbox_domain::ToolApprovalDecider::Delegate { model, call } => {
                        WireToolApprovalEventDecider::Delegate {
                            model_selection_id: wire_uuid(model.into_uuid()),
                            model_call_id: wire_uuid(call.into_uuid()),
                        }
                    }
                    signalbox_domain::ToolApprovalDecider::UserOverride {
                        command,
                        denied_request,
                    } => WireToolApprovalEventDecider::UserOverride {
                        command_id: wire_uuid(command.into_uuid()),
                        overridden_tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                };
                SessionEvent::ToolApprovalDecided {
                    turn_id: wire_uuid(turn.into_uuid()),
                    tool_request_id: wire_uuid(approval.request().into_uuid()),
                    decision,
                    decider,
                    rationale: approval.rationale().map(|value| value.as_str().to_owned()),
                }
            }
            Self::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox,
                working_directory,
                state,
            } => SessionEvent::RunnerStateTransition {
                runner_id: wire_uuid(runner.into_uuid()),
                placement_revision: WireRunnerPlacementRevision::try_new(placement_revision.get())
                    .ok_or(ProcessConnectionError::EncodeInvariant)?,
                sandbox_profile: match sandbox {
                    signalbox_domain::RunnerSandboxProfile::Ambient => {
                        WireRunnerSandboxProfile::Ambient
                    }
                    signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted => {
                        WireRunnerSandboxProfile::WorkspaceRestricted
                    }
                },
                working_directory: working_directory
                    .as_ref()
                    .map(|directory| {
                        WireRunnerWorkingDirectory::try_new(directory.as_str().to_owned())
                            .map_err(|_| ProcessConnectionError::EncodeInvariant)
                    })
                    .transpose()?,
                state: match state {
                    DispatchedRunnerState::Pinned => WireRunnerStateTransitionState::Pinned,
                    DispatchedRunnerState::Suspect => WireRunnerStateTransitionState::Suspect,
                    DispatchedRunnerState::Connected => WireRunnerStateTransitionState::Connected,
                    DispatchedRunnerState::RunnerLostBeforePin => {
                        WireRunnerStateTransitionState::RunnerLostBeforePin
                    }
                    DispatchedRunnerState::RunnerLost => WireRunnerStateTransitionState::RunnerLost,
                    DispatchedRunnerState::Replaced => WireRunnerStateTransitionState::Replaced,
                    DispatchedRunnerState::WorkingDirectoryChanged => {
                        WireRunnerStateTransitionState::WorkingDirectoryChanged
                    }
                    DispatchedRunnerState::Abandoned => WireRunnerStateTransitionState::Abandoned,
                },
            },
            Self::ContextCompacted {
                compaction,
                call,
                through_position,
                summary_entry,
                result_frontier,
            } => SessionEvent::ContextCompacted {
                context_compaction_id: wire_uuid(compaction.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                through_position: CanonicalU64::new(*through_position),
                summary_entry_id: wire_uuid(summary_entry.into_uuid()),
                result_frontier_id: wire_uuid(result_frontier.into_uuid()),
            },
            Self::TurnCompleted {
                turn,
                call,
                completion_entry,
                terminal_frontier,
            } => SessionEvent::TurnCompleted {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                completion_entry_id: wire_uuid(completion_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnFailed {
                turn,
                failure_entry,
                terminal_frontier,
            } => SessionEvent::TurnFailed {
                turn_id: wire_uuid(turn.into_uuid()),
                failure_entry_id: wire_uuid(failure_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnRefused {
                turn,
                call,
                terminal_frontier,
            } => SessionEvent::TurnRefused {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnCancelled {
                turn,
                cancellation_entry,
                terminal_frontier,
            } => SessionEvent::TurnCancelled {
                turn_id: wire_uuid(turn.into_uuid()),
                cancellation_entry_id: wire_uuid(cancellation_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnReconciliationRequired {
                turn,
                operation,
                terminal_frontier,
            } => match operation {
                DispatchedReconciliationOperation::ModelCall(call) => {
                    SessionEvent::TurnReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(call.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
                DispatchedReconciliationOperation::ToolAttempt(attempt) => {
                    SessionEvent::TurnToolReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
            },
            Self::DelegationUpdate(update) => wire_delegation_update(update),
        };
        Ok(event)
    }
}

pub(super) fn wire_delegation_update(update: &DispatchedDelegationUpdate) -> SessionEvent {
    match update {
        DispatchedDelegationUpdate::ChildSpawned {
            spawning_request,
            child,
            policy,
        } => SessionEvent::ChildSpawned {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            relationship: wire_delegation_policy(*policy),
        },
        DispatchedDelegationUpdate::ChildWaiting {
            spawning_request,
            child,
            awaiting_request,
            mode,
        } => SessionEvent::ChildWaiting {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            await_request_id: wire_uuid(awaiting_request.into_uuid()),
            mode: match mode {
                DispatchedDelegationWaitMode::Foreground => WireDelegationWaitMode::Foreground,
                DispatchedDelegationWaitMode::Background => WireDelegationWaitMode::Background,
            },
        },
        DispatchedDelegationUpdate::ChildLifecycleDisposition {
            spawning_request,
            child,
            event_ordinal: _,
            outcome,
            reason,
            provenance,
        } => SessionEvent::ChildLifecycleDisposition {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
        },
        DispatchedDelegationUpdate::ChildResult {
            spawning_request,
            child,
            outcome,
            reason,
            provenance,
            content,
        } => SessionEvent::ChildResult {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
            content: content.clone(),
        },
        DispatchedDelegationUpdate::SessionMessage {
            spawning_request,
            message,
            sender,
            recipient,
            message_ordinal,
            delivery_sequence,
            content,
        } => SessionEvent::SessionMessage {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            message_id: wire_uuid(message.into_uuid()),
            sender_session_id: wire_uuid(sender.into_uuid()),
            recipient_session_id: wire_uuid(recipient.into_uuid()),
            ordinal: CanonicalU64::new(*message_ordinal),
            delivery_sequence: CanonicalU64::new(*delivery_sequence),
            content: content.clone(),
        },
    }
}

pub(super) const fn wire_delegation_policy(
    policy: DispatchedDelegationPolicy,
) -> WireDelegationPolicy {
    match policy {
        DispatchedDelegationPolicy::Background => WireDelegationPolicy::Background {},
        DispatchedDelegationPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => WireDelegationPolicy::Bound {
            on_parent_stopped: wire_bound_child_action(on_parent_stopped),
            on_parent_cancelled: wire_bound_child_action(on_parent_cancelled),
        },
    }
}

pub(super) const fn wire_bound_child_action(
    action: DispatchedBoundChildAction,
) -> WireBoundChildAction {
    match action {
        DispatchedBoundChildAction::KeepRunning => WireBoundChildAction::KeepRunning,
        DispatchedBoundChildAction::Stop => WireBoundChildAction::Stop,
        DispatchedBoundChildAction::Cancel => WireBoundChildAction::Cancel,
    }
}

pub(super) const fn wire_delegation_outcome(
    outcome: DispatchedDelegationOutcome,
) -> WireDelegationOutcome {
    match outcome {
        DispatchedDelegationOutcome::ResultReturned => WireDelegationOutcome::Returned,
        DispatchedDelegationOutcome::ChildFailed => WireDelegationOutcome::Failed,
        DispatchedDelegationOutcome::ChildStopped => WireDelegationOutcome::Stopped,
        DispatchedDelegationOutcome::ChildCancelled => WireDelegationOutcome::Cancelled,
        DispatchedDelegationOutcome::ContinueRunning => WireDelegationOutcome::ContinueRunning,
        DispatchedDelegationOutcome::AlreadyTerminal => WireDelegationOutcome::AlreadyTerminal,
    }
}

pub(super) const fn wire_delegation_reason(
    reason: DispatchedDelegationReason,
) -> WireDelegationReason {
    match reason {
        DispatchedDelegationReason::ChildCompleted => WireDelegationReason::ChildCompleted,
        DispatchedDelegationReason::ChildExecutionFailed => {
            WireDelegationReason::ChildExecutionFailed
        }
        DispatchedDelegationReason::ChildResultUnavailable => {
            WireDelegationReason::ChildResultUnavailable
        }
        DispatchedDelegationReason::ChildCancelled => WireDelegationReason::ChildCancelled,
        DispatchedDelegationReason::ParentStoppedWithDescendants => {
            WireDelegationReason::ParentStopped
        }
        DispatchedDelegationReason::ParentCancelledWithDescendants => {
            WireDelegationReason::ParentCancelled
        }
    }
}

pub(super) fn wire_delegation_provenance(
    provenance: DispatchedDelegationProvenance,
) -> WireDelegationProvenance {
    match provenance {
        DispatchedDelegationProvenance::ChildTurn { session, turn } => {
            WireDelegationProvenance::ChildTurn {
                child_session_id: wire_uuid(session.into_uuid()),
                child_turn_id: wire_uuid(turn.into_uuid()),
            }
        }
        DispatchedDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        } => WireDelegationProvenance::ParentTurnCommand {
            parent_session_id: wire_uuid(session.into_uuid()),
            parent_turn_id: wire_uuid(turn.into_uuid()),
            command_id: wire_uuid(command.into_uuid()),
            descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
        },
        DispatchedDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        } => WireDelegationProvenance::ParentGoalCommand {
            parent_session_id: wire_uuid(session.into_uuid()),
            goal_generation: CanonicalU64::new(goal_generation),
            command_id: wire_uuid(command.into_uuid()),
            descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
        },
        DispatchedDelegationProvenance::ParentLifecycleCommand { session, command } => {
            WireDelegationProvenance::ParentLifecycleCommand {
                parent_session_id: wire_uuid(session.into_uuid()),
                command_id: wire_uuid(command.into_uuid()),
                descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
            }
        }
    }
}

pub(super) const fn wire_model_call_state(state: DispatchedModelCallState) -> ModelCallState {
    match state {
        DispatchedModelCallState::Prepared => ModelCallState::Prepared {},
        DispatchedModelCallState::InFlight => ModelCallState::InFlight {},
        DispatchedModelCallState::CancellationRequested => ModelCallState::CancellationRequested {},
        DispatchedModelCallState::Terminal(disposition) => ModelCallState::Terminal {
            disposition: match disposition {
                DispatchedModelCallDisposition::Completed => ModelCallDisposition::Completed,
                DispatchedModelCallDisposition::KnownFailed => ModelCallDisposition::KnownFailed,
                DispatchedModelCallDisposition::Refused => ModelCallDisposition::Refused,
                DispatchedModelCallDisposition::Cancelled => ModelCallDisposition::Cancelled,
                DispatchedModelCallDisposition::Ambiguous => ModelCallDisposition::Ambiguous,
            },
        },
    }
}

#[derive(Debug)]
pub(super) enum ProcessConnectionError {
    PeerIo(io::Error),
    SpoolIo(io::Error),
    Encode(FrameEncodeError),
    EncodeInvariant,
    InboundFrameBudgetClosed,
    ImportBudgetClosed,
    ReviewCommandBudgetClosed,
    SnapshotReaderBudgetClosed,
}

impl From<io::Error> for ProcessConnectionError {
    fn from(error: io::Error) -> Self {
        Self::PeerIo(error)
    }
}

impl From<FrameEncodeError> for ProcessConnectionError {
    fn from(error: FrameEncodeError) -> Self {
        Self::Encode(error)
    }
}

impl fmt::Display for ProcessConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PeerIo(_) => "the local process peer I/O failed",
            Self::SpoolIo(_) => "the local process snapshot spool I/O failed",
            Self::Encode(_) => "the local process connection could not encode a frame",
            Self::EncodeInvariant => {
                "the local process connection could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process connection lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process connection lost its conversation import budget"
            }
            Self::ReviewCommandBudgetClosed => {
                "the local process connection lost its review-command budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process connection lost its snapshot reader budget"
            }
        })
    }
}

impl Error for ProcessConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PeerIo(error) | Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::ReviewCommandBudgetClosed
            | Self::SnapshotReaderBudgetClosed => None,
        }
    }
}

/// Fatal local-process runtime failure.
#[derive(Debug)]
pub enum ProcessRuntimeError {
    /// The guarded listener could not accept a connection.
    Accept(io::Error),
    /// A completed snapshot spool could not be read for transmission.
    SpoolIo(io::Error),
    /// A server frame could not satisfy the closed wire contract.
    Encode(FrameEncodeError),
    /// Runtime-owned values could not be represented by the closed wire contract.
    EncodeInvariant,
    /// The runtime-owned aggregate inbound frame budget closed unexpectedly.
    InboundFrameBudgetClosed,
    /// The runtime-owned conversation-import budget closed unexpectedly.
    ImportBudgetClosed,
    /// The runtime-owned review-command budget closed unexpectedly.
    ReviewCommandBudgetClosed,
    /// The runtime-owned snapshot-reader budget closed unexpectedly.
    SnapshotReaderBudgetClosed,
    /// The application pool cannot reserve capacity outside snapshot reads.
    InsufficientPoolCapacity,
    /// A connection task panicked or was cancelled unexpectedly.
    ConnectionTask(JoinError),
    /// The durable outbox dispatcher failed.
    Dispatch(OutboxDispatchError),
    /// The single dispatcher produced an impossible retry result.
    UnexpectedDispatcherRetry,
    /// The revalidated socket path could not be cleaned up.
    CleanupSocket(LocalSocketError),
}

impl fmt::Display for ProcessRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accept(_) => "the local process listener failed",
            Self::SpoolIo(_) => "the local process server could not read a snapshot spool",
            Self::Encode(_) => "the local process server could not encode a frame",
            Self::EncodeInvariant => {
                "the local process server could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process server lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process server lost its conversation import budget"
            }
            Self::ReviewCommandBudgetClosed => {
                "the local process server lost its review-command budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process server lost its snapshot reader budget"
            }
            Self::InsufficientPoolCapacity => {
                "the local process server cannot reserve database pool capacity"
            }
            Self::ConnectionTask(_) => "a local process connection task failed",
            Self::Dispatch(_) => "the durable process-update dispatcher failed",
            Self::UnexpectedDispatcherRetry => {
                "the process-update dispatcher unexpectedly requested retry"
            }
            Self::CleanupSocket(_) => "the local process socket could not be cleaned up",
        })
    }
}

impl Error for ProcessRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) => Some(error),
            Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::Dispatch(error) => Some(error),
            Self::CleanupSocket(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::ReviewCommandBudgetClosed
            | Self::SnapshotReaderBudgetClosed
            | Self::InsufficientPoolCapacity
            | Self::UnexpectedDispatcherRetry => None,
        }
    }
}
