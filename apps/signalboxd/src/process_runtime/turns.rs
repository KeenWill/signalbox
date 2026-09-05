use super::*;

#[derive(Debug)]
pub(super) struct ConfiguredSubmitInputTransaction<'configuration> {
    pub(super) repository: SubmitInputRepository,
    pub(super) model_configuration: &'configuration HubModelConfiguration,
    pub(super) principal: CommandPrincipal,
    pub(super) cascade_root_kind: ParentTerminationKind,
}

impl SubmitInputTransaction for ConfiguredSubmitInputTransaction<'_> {
    type Error = SubmitInputRepositoryError;

    async fn handle<NextTurn, NextToolCancellation, NextClosureDecision, NextClosureAttempt>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
        next_closure_decision: NextClosureDecision,
        next_closure_attempt: NextClosureAttempt,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
        NextClosureDecision: FnMut() -> DurableCommandId + Send,
        NextClosureAttempt: FnMut() -> signalbox_domain::TurnAttemptId + Send,
    {
        let outcome = self
            .repository
            .handle_with_candidates_alias_resolver_as(
                command,
                self.principal,
                self.cascade_root_kind,
                accepted_input,
                turn,
                cancellation_identities,
                next_reclassified_turn,
                next_tool_cancellation,
                next_closure_decision,
                next_closure_attempt,
                |alias| self.model_configuration.resolve_alias(alias),
            )
            .await?;

        Ok(match outcome {
            SubmitInputHandlingOutcome::Recorded(result) => SubmitInputOutcome::Recorded(result),
            SubmitInputHandlingOutcome::ConflictingReuse { command_id } => {
                SubmitInputOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed submit request is kept explicit at this wire-to-application adapter"
)]
pub(super) async fn handle_submit_input<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    content: UserInputContent,
    expected_defaults_version: Option<CanonicalU64>,
    model_settings: WireModelSettingsOverlay,
    delivery: Option<InputDelivery>,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
    blob_store_registry: Option<&BlobStoreRegistry>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let session = SessionId::from_uuid(session_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    let repository = match blob_store_registry {
        Some(registry) => repository.with_attachment_maximum_bytes(registry.max_blob_bytes()),
        None => repository,
    };
    let expected_version = expected_defaults_version
        .and_then(|version| SessionConfigurationDefaultsVersion::try_from_u64(version.value()));
    let model_settings = domain_model_settings_overlay(model_settings);
    let configuration = || {
        expected_version.map(|version| {
            PerInputConfigurationChoices::with_model_settings(
                version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            )
        })
    };
    let delivery = match delivery {
        None | Some(InputDelivery::StartWhenIdle {}) => configuration()
            .map(|configuration| DeliveryRequest::StartWhenNoActiveTurn { configuration }),
        Some(InputDelivery::Steer {
            expected_active_turn_id,
        }) if expected_defaults_version.is_none()
            && model_settings == DomainModelSettingsOverlay::inherit_all() =>
        {
            Some(DeliveryRequest::NextSafePoint {
                expected_active_turn: TurnId::from_uuid(expected_active_turn_id.into_uuid()),
            })
        }
        Some(InputDelivery::Queue {
            expected_active_turn_id,
        }) => configuration().map(|configuration| DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(expected_active_turn_id.into_uuid()),
            configuration,
        }),
        Some(InputDelivery::Steer { .. }) => None,
    };
    let Some(delivery) = delivery else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = SubmitInputRequest::try_new_with_content_limit(
        command_id,
        session,
        content,
        delivery,
        configured_usize(model_configuration, "max_message_utf8_bytes"),
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
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

/// Reconciles the exact active turn parked on an ambiguous model call.
///
/// The parked turn's terminal disposition is proof-bearing, so the user
/// supplies the interrupt authority the accepted lifecycle already defines and
/// the successor input the session continues with. The narrow precondition read
/// keeps this verb from becoming a general active-turn cancellation surface;
/// the authoritative transaction still revalidates the exact expected active
/// turn under the session lock.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed reconciliation request is kept explicit at this wire-to-application adapter"
)]
pub(super) async fn handle_reconcile_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: UserInputContent,
    expected_defaults_version: CanonicalU64,
    model_settings: WireModelSettingsOverlay,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
    blob_store_registry: Option<&BlobStoreRegistry>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let expected_active_turn = TurnId::from_uuid(expected_active_turn_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    let repository = match blob_store_registry {
        Some(registry) => repository.with_attachment_maximum_bytes(registry.max_blob_bytes()),
        None => repository,
    };
    // A command identity that already names durable intent must reach the
    // replay boundary unconditionally: the first handling already
    // released the wait, so re-applying the current-state precondition would
    // answer a retry of a committed decision with a refusal instead of its
    // recorded result.
    let command_is_claimed = match repository.load(command_id).await {
        Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => true,
        Ok(None) => false,
        Err(error) => {
            return write_submit_input_repository_error(
                writer, version, request_id, session_id, error,
            )
            .await;
        }
    };
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_settings = domain_model_settings_overlay(model_settings);
    if !command_is_claimed {
        match ProcessReadRepository::new(pool.clone())
            .model_call_recovery_precondition(session)
            .await
        {
            // An absent session is left to the authoritative transaction, whose
            // recorded `SessionNotFound` the wire contract promises.
            Ok(ProcessModelCallRecoveryPrecondition::SessionAbsent) => {}
            Ok(ProcessModelCallRecoveryPrecondition::Parked { turn })
                if turn == expected_active_turn => {}
            Ok(
                ProcessModelCallRecoveryPrecondition::NoParkedTurn
                | ProcessModelCallRecoveryPrecondition::Parked { .. },
            ) => {
                // The claim probe and this read are separate statements, so an
                // equal-identity request that overlapped ours can have released
                // the wait in between. Rechecking the claim before refusing
                // keeps the loser of that race on the replay boundary instead
                // of answering a committed decision with a refusal.
                match repository.load(command_id).await {
                    Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => {}
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::rejected(
                                RejectionDetail::TurnNotAwaitingReconciliation {
                                    session_id,
                                    turn_id: expected_active_turn_id,
                                },
                            ),
                        )
                        .await;
                    }
                    Err(error) => {
                        return write_submit_input_repository_error(
                            writer, version, request_id, session_id, error,
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                return write_process_read_error(
                    writer,
                    version,
                    request_id,
                    Some(session_id),
                    error,
                )
                .await;
            }
        }
    }
    let request = SubmitInputRequest::try_new_with_content_limit(
        command_id,
        session,
        content,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::with_model_settings(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            ),
        },
        configured_usize(model_configuration, "max_message_utf8_bytes"),
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
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

pub(super) const fn decode_descendant_scope(
    value: WireDescendantTerminationScope,
) -> DescendantTerminationScope {
    match value {
        WireDescendantTerminationScope::ParentAlone => DescendantTerminationScope::ParentAlone,
        WireDescendantTerminationScope::ParentAndDescendants => {
            DescendantTerminationScope::ParentAndDescendants
        }
    }
}

/// Stops the exact active turn through the accepted interrupt treatment.
///
/// The delivery is the `Interrupt` treatment the turn lifecycle already
/// defines: cancellation authority exists only as an applied interrupt bound
/// to an immediate successor, so the stop carries the successor
/// content and no standalone cancellation command is introduced. The
/// authoritative transaction validates the expected active turn under the
/// session lock and records every typed refusal, so no precondition read runs
/// here.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed stop request is kept explicit at this wire-to-application adapter"
)]
pub(super) async fn handle_stop_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: UserInputContent,
    expected_defaults_version: CanonicalU64,
    descendant_scope: DescendantTerminationScope,
    model_settings: WireModelSettingsOverlay,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
    blob_store_registry: Option<&BlobStoreRegistry>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let expected_active_turn = TurnId::from_uuid(expected_active_turn_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    let repository = match blob_store_registry {
        Some(registry) => repository.with_attachment_maximum_bytes(registry.max_blob_bytes()),
        None => repository,
    };
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_settings = domain_model_settings_overlay(model_settings);
    let request = SubmitInputRequest::try_new_with_content_limit(
        command_id,
        session,
        content,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope,
            configuration: PerInputConfigurationChoices::with_model_settings(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            ),
        },
        configured_usize(model_configuration, "max_message_utf8_bytes"),
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
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the shared submit-input execution keeps its wire and application collaborators explicit"
)]
pub(super) async fn run_submit_input<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    request: SubmitInputRequest,
    repository: SubmitInputRepository,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository,
            model_configuration,
            principal: CommandPrincipal::Operator,
            cascade_root_kind: ParentTerminationKind::Cancelled,
        },
        eligibility_nudge.clone(),
        tool_dispatch_gate.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(result),
        ))) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: wire_uuid(result.accepted_input().into_uuid()),
                    acceptance_position: CanonicalU64::new(result.acceptance_position().as_u64()),
                    turn_id: wire_uuid(result.turn().into_uuid()),
                    model_settings: wire_model_settings(
                        result.origin_configuration().effective().model_settings(),
                    ),
                },
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(rejected))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(map_rejection(rejected)?),
            )
            .await
        }
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(result),
        ))) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SteeringSubmitted {
                    session_id,
                    accepted_input_id: wire_uuid(result.accepted_input().into_uuid()),
                    acceptance_position: CanonicalU64::new(result.acceptance_position().as_u64()),
                    source_turn_id: wire_uuid(result.binding().source_turn().into_uuid()),
                },
            )
            .await
        }
        Err(error) => {
            write_submit_input_repository_error(writer, version, request_id, session_id, error)
                .await
        }
    }
}

/// Closed submit-input disposition for one model-execution repository error.
///
/// The mapping retains the exact typed variant before its source is erased.
/// No database detail, transition label, or corruption payload enters the
/// diagnostic, so credentials and caller or model content remain excluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubmitInputModelExecutionDiagnostic {
    DatabaseUnavailable,
    CommitAmbiguous,
    Internal(InternalDiagnostic),
}

impl SubmitInputModelExecutionDiagnostic {
    pub(super) fn into_protocol_error(self, session_id: CanonicalUuid) -> ProtocolError {
        match self {
            Self::DatabaseUnavailable => ProtocolError::mutation_unavailable(false),
            Self::CommitAmbiguous => ProtocolError::mutation_unavailable(true),
            Self::Internal(diagnostic) => {
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic)
            }
        }
    }
}

pub(super) fn submit_input_model_execution_diagnostic(
    error: &signalbox_persistence::model_execution::ModelCallRepositoryError,
) -> SubmitInputModelExecutionDiagnostic {
    use signalbox_persistence::model_execution::ModelCallRepositoryError;

    match error {
        ModelCallRepositoryError::Database {
            commit_ambiguous, ..
        } => match commit_ambiguous {
            true => SubmitInputModelExecutionDiagnostic::CommitAmbiguous,
            false => SubmitInputModelExecutionDiagnostic::DatabaseUnavailable,
        },
        ModelCallRepositoryError::Corruption(_) => SubmitInputModelExecutionDiagnostic::Internal(
            InternalDiagnostic::SubmitInputModelExecutionCorruption,
        ),
        ModelCallRepositoryError::IdentityCollision(_) => {
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionIdentityCollision,
            )
        }
        ModelCallRepositoryError::NoLiveExecution => SubmitInputModelExecutionDiagnostic::Internal(
            InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution,
        ),
        ModelCallRepositoryError::InvalidTransition(_) => {
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionInvalidTransition,
            )
        }
    }
}

pub(super) async fn write_submit_input_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: SubmitInputRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        SubmitInputRepositoryError::Database(_) => ProtocolError::mutation_unavailable(false),
        SubmitInputRepositoryError::CommitAmbiguous(_) => ProtocolError::mutation_unavailable(true),
        SubmitInputRepositoryError::ModelExecution(error) => {
            submit_input_model_execution_diagnostic(error.as_ref()).into_protocol_error(session_id)
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SubmitInputCommandKindMismatch,
        ),
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            internal_protocol_error(
                Some(session_id.into_uuid()),
                InternalDiagnostic::SubmitInputIdentityCollision,
            )
        }
        SubmitInputRepositoryError::UnsupportedModelSetting(error) => {
            ProtocolError::rejected(wire_unsupported_model_setting(error))
        }
        SubmitInputRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SubmitInputCorruption,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}

/// Converts imported-session repository evidence into one typed Internal diagnostic.
pub(super) fn imported_session_internal_diagnostic(
    error: &ImportedSessionRepositoryError,
) -> InternalDiagnostic {
    match error {
        ImportedSessionRepositoryError::Database(_) => InternalDiagnostic::ImportedSessionDatabase,
        ImportedSessionRepositoryError::CommitAmbiguous(_) => {
            InternalDiagnostic::ImportedSessionCommitAmbiguous
        }
        ImportedSessionRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::ImportedSessionCommandKindMismatch
        }
        ImportedSessionRepositoryError::Preparation(_) => {
            InternalDiagnostic::ImportedSessionPreparation
        }
        ImportedSessionRepositoryError::IdentityCollision(_) => {
            InternalDiagnostic::ImportedSessionIdentityCollision
        }
        ImportedSessionRepositoryError::ImportedConversation(_) => {
            InternalDiagnostic::ImportedSessionCorruption
        }
        ImportedSessionRepositoryError::Corruption(_) => {
            InternalDiagnostic::ImportedSessionCorruption
        }
    }
}

/// Converts imported-conversation evidence without formatting its payload.
pub(super) fn imported_conversation_internal_diagnostic(
    error: &ImportedConversationRepositoryError,
) -> InternalDiagnostic {
    match error {
        ImportedConversationRepositoryError::Database(_) => {
            InternalDiagnostic::ImportedConversationDatabase
        }
        ImportedConversationRepositoryError::IdentityCollision(_) => {
            InternalDiagnostic::ImportedConversationIdentityCollision
        }
        ImportedConversationRepositoryError::BlobStorage(
            ImportedRawBlobStorageError::Unavailable,
        ) => InternalDiagnostic::ImportedConversationDatabase,
        ImportedConversationRepositoryError::BlobStorage(
            ImportedRawBlobStorageError::Integrity,
        ) => InternalDiagnostic::ImportedConversationCorruption,
        ImportedConversationRepositoryError::BlobCatalog(_) => {
            InternalDiagnostic::ImportedConversationCorruption
        }
        ImportedConversationRepositoryError::Corruption(_) => {
            InternalDiagnostic::ImportedConversationCorruption
        }
    }
}

/// Converts create-session evidence without formatting command or database detail.
pub(super) fn create_session_internal_diagnostic(
    error: &CreateSessionError<CreateSessionRepositoryError>,
) -> InternalDiagnostic {
    match error {
        CreateSessionError::Preparation(_) => InternalDiagnostic::SessionCreationPreparation,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_)) => {
            InternalDiagnostic::SessionCreationDatabase
        }
        CreateSessionError::Transaction(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            InternalDiagnostic::SessionCreationCommitAmbiguous
        }
        CreateSessionError::Transaction(CreateSessionRepositoryError::DifferentCommandKind {
            ..
        }) => InternalDiagnostic::SessionCreationCommandKindMismatch,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Corruption(_)) => {
            InternalDiagnostic::SessionCreationCorruption
        }
    }
}

/// Converts metadata evidence without formatting command or durable content.
pub(super) fn session_metadata_internal_diagnostic(
    error: &SessionMetadataRepositoryError,
) -> InternalDiagnostic {
    match error {
        SessionMetadataRepositoryError::Database(_) => InternalDiagnostic::SessionMetadataDatabase,
        SessionMetadataRepositoryError::CommitAmbiguous(_) => {
            InternalDiagnostic::SessionMetadataCommitAmbiguous
        }
        SessionMetadataRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::SessionMetadataCommandKindMismatch
        }
        SessionMetadataRepositoryError::Corruption(_) => {
            InternalDiagnostic::SessionMetadataCorruption
        }
    }
}

/// Converts defaults-replacement evidence into one typed Internal diagnostic.
pub(super) fn session_defaults_internal_diagnostic(
    error: &ReplaceSessionDefaultsRepositoryError,
) -> InternalDiagnostic {
    match error {
        ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous: false,
            ..
        } => InternalDiagnostic::SessionDefaultsDatabase,
        ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous: true,
            ..
        } => InternalDiagnostic::SessionDefaultsCommitAmbiguous,
        ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::SessionDefaultsCommandKindMismatch
        }
        ReplaceSessionDefaultsRepositoryError::Corruption(_) => {
            InternalDiagnostic::SessionDefaultsCorruption
        }
    }
}

/// Records one user tool decision through the canonical decision command.
///
/// A claimed command identity reaches the durable replay boundary
/// unconditionally. Otherwise a narrow read refuses, before any
/// command is recorded, a decision whose named session does not own the named
/// request; an absent request is left to the transaction's recorded
/// `request_not_found`, and every other outcome is the recorded result of the
/// canonical command.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed decision request is kept explicit at this wire-to-application adapter"
)]
pub(super) async fn handle_decide_tool_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    decision: ToolDecision,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let request = ToolRequestId::from_uuid(tool_request_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let domain_decision = match decision {
        ToolDecision::Approve {} => ToolApprovalDecision::Approve,
        ToolDecision::Deny { reason } => match ToolDenialReason::try_new(reason) {
            Ok(reason) => ToolApprovalDecision::Deny {
                reason: Some(reason),
            },
            Err(_) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
        },
    };
    let Ok(command) = DecideToolRequest::try_new(command_id, request, domain_decision) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let command_is_claimed = match repository.load_recorded_decision(command_id).await {
        Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => true,
        Ok(None) => false,
        Err(error) => {
            return write_tool_loop_error(writer, version, request_id, session_id, error).await;
        }
    };
    if !command_is_claimed {
        match ProcessReadRepository::new(pool.clone())
            .tool_request_session(request)
            .await
        {
            // An absent request is left to the authoritative transaction,
            // whose recorded `request_not_found` rejection the wire
            // contract promises.
            Ok(None) => {}
            Ok(Some(owning_session)) if owning_session == session => {}
            Ok(Some(_)) => {
                // The claim probe and this read are separate statements, so an
                // equal-identity request that overlapped ours can have
                // recorded the decision in between. Rechecking the claim
                // before refusing keeps the loser of that race on the replay
                // boundary instead of answering a committed decision with a
                // refusal.
                match repository.load_recorded_decision(command_id).await {
                    Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => {}
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::rejected(RejectionDetail::ToolRequestNotInSession {
                                session_id,
                                tool_request_id,
                            }),
                        )
                        .await;
                    }
                    Err(error) => {
                        return write_tool_loop_error(
                            writer, version, request_id, session_id, error,
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                return write_process_read_error(
                    writer,
                    version,
                    request_id,
                    Some(session_id),
                    error,
                )
                .await;
            }
        }
    }
    let mut service = DecideToolRequestService::new(UuidV7ToolLoopIdGenerator, repository);
    match service.execute(command).await {
        Ok(prepared) => match prepared.result() {
            DecideToolRequestResult::Applied(applied) => {
                // An applied decision can open the executing phase; the nudge
                // lets the scheduler resume the tool round promptly, and the
                // durable sweep remains the backstop.
                let _ = eligibility_nudge.nudge(session);
                write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::ToolRequestDecided {
                        tool_request_id,
                        decision: wire_tool_decision(applied.resolution().decision())?,
                    },
                )
                .await
            }
            DecideToolRequestResult::Rejected(rejected) => {
                let detail = match *rejected {
                    DecideToolRequestRejectedResult::RequestNotFound { request } => {
                        RejectionDetail::ToolRequestNotFound {
                            tool_request_id: wire_uuid(request.into_uuid()),
                        }
                    }
                    DecideToolRequestRejectedResult::AlreadyResolved { request } => {
                        RejectionDetail::ToolRequestAlreadyResolved {
                            tool_request_id: wire_uuid(request.into_uuid()),
                        }
                    }
                    DecideToolRequestRejectedResult::NotEarliestUndecided { request, earliest } => {
                        RejectionDetail::ToolRequestNotEarliestUndecided {
                            tool_request_id: wire_uuid(request.into_uuid()),
                            earliest_tool_request_id: wire_uuid(earliest.into_uuid()),
                        }
                    }
                };
                write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
            }
        },
        Err(error) => write_tool_loop_error(writer, version, request_id, session_id, error).await,
    }
}

pub(super) fn wire_tool_decision(
    decision: &ToolApprovalDecision,
) -> Result<ToolDecision, ProcessConnectionError> {
    match decision {
        ToolApprovalDecision::Approve => Ok(ToolDecision::Approve {}),
        ToolApprovalDecision::Deny {
            reason: Some(reason),
        } => Ok(ToolDecision::Deny {
            reason: reason.as_str().to_owned(),
        }),
        // The wire surface requires a denial reason, so every
        // decision it records carries one; a reason-free denial cannot be
        // projected as this receipt.
        ToolApprovalDecision::Deny { reason: None } => Err(ProcessConnectionError::EncodeInvariant),
    }
}

/// Records one user override of a delegate denial through the canonical
/// override command.
///
/// A claimed command identity reaches the durable replay boundary
/// unconditionally. The session is part of the canonical override
/// payload, so an other-session request is the transaction's recorded
/// `request_not_in_session` rejection rather than a pre-command refusal, and
/// every outcome is the recorded result of the canonical command.
pub(super) async fn handle_override_denied_tool_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let request = ToolRequestId::from_uuid(tool_request_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let Ok(command) = OverrideDeniedToolRequest::try_new(command_id, session, request) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let mut service = OverrideDeniedToolRequestService::new(repository);
    match service.execute(command).await {
        Ok(prepared) => match prepared.result() {
            OverrideDeniedToolRequestResult::Applied(applied) => {
                write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::ToolDenialOverridden {
                        tool_request_id: wire_uuid(applied.recorded().denied_request().into_uuid()),
                    },
                )
                .await
            }
            OverrideDeniedToolRequestResult::Rejected(rejected) => {
                let detail = match *rejected {
                    OverrideDeniedToolRequestRejectedResult::RequestNotFound { denied_request } => {
                        RejectionDetail::ToolRequestNotFound {
                            tool_request_id: wire_uuid(denied_request.into_uuid()),
                        }
                    }
                    OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                        session,
                        denied_request,
                    } => RejectionDetail::ToolRequestNotInSession {
                        session_id: wire_uuid(session.into_uuid()),
                        tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                    OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                        denied_request,
                    } => RejectionDetail::ToolRequestNotDelegateDenied {
                        tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                    OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                        denied_request,
                    } => RejectionDetail::ToolRequestNotTerminallyDenied {
                        tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                    OverrideDeniedToolRequestRejectedResult::AlreadyOverridden {
                        denied_request,
                    } => RejectionDetail::ToolDenialAlreadyOverridden {
                        tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                };
                write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
            }
        },
        Err(error) => write_tool_loop_error(writer, version, request_id, session_id, error).await,
    }
}

pub(super) async fn write_tool_loop_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: ToolLoopRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        ToolLoopRepositoryError::Database {
            commit_ambiguous, ..
        } => ProtocolError::mutation_unavailable(commit_ambiguous),
        // Any difference under a claimed identity — including a different
        // command kind — is conflicting reuse, per the identity-and-commands
        // registry contract.
        ToolLoopRepositoryError::ConflictingCommandReuse
        | ToolLoopRepositoryError::DifferentCommandKind => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        ToolLoopRepositoryError::IdentityCollision => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopIdentityCollision,
        ),
        ToolLoopRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopCorruption,
        ),
        ToolLoopRepositoryError::InvalidTransition(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopInvalidTransition,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}

pub(super) fn admitted_user_content(content: UserInputContent) -> Result<UserContent, ()> {
    let parts = content
        .into_parts()
        .into_iter()
        .map(|part| match part {
            signalbox_process_protocol::UserInputPart::Text { text } => {
                signalbox_domain::UserContentPart::try_text(text).map_err(|_| ())
            }
            signalbox_process_protocol::UserInputPart::Attachment {
                digest,
                kind,
                media_type,
                display_filename,
            } => Ok(signalbox_domain::UserContentPart::Attachment {
                digest: digest.into_digest(),
                kind: match kind {
                    signalbox_process_protocol::UserAttachmentKind::Image => {
                        signalbox_domain::AttachmentKind::Image
                    }
                    signalbox_process_protocol::UserAttachmentKind::Document => {
                        signalbox_domain::AttachmentKind::Document
                    }
                    signalbox_process_protocol::UserAttachmentKind::File => {
                        signalbox_domain::AttachmentKind::File
                    }
                },
                media_type: signalbox_domain::DeclaredMediaType::try_new(media_type)
                    .map_err(|_| ())?,
                display_filename: display_filename
                    .map(signalbox_domain::AttachmentDisplayFilename::try_new)
                    .transpose()
                    .map_err(|_| ())?,
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    UserContent::try_parts(parts).map_err(|_| ())
}

pub(crate) fn wire_user_content(content: &UserContent) -> UserInputContent {
    UserInputContent::from_parts(
        content
            .parts()
            .iter()
            .map(|part| match part {
                signalbox_domain::UserContentPart::Text { value } => {
                    signalbox_process_protocol::UserInputPart::Text {
                        text: value.as_str().to_owned(),
                    }
                }
                signalbox_domain::UserContentPart::Attachment {
                    digest,
                    kind,
                    media_type,
                    display_filename,
                } => signalbox_process_protocol::UserInputPart::Attachment {
                    digest: signalbox_process_protocol::CanonicalBlobDigest::from_digest(*digest),
                    kind: match kind {
                        signalbox_domain::AttachmentKind::Image => {
                            signalbox_process_protocol::UserAttachmentKind::Image
                        }
                        signalbox_domain::AttachmentKind::Document => {
                            signalbox_process_protocol::UserAttachmentKind::Document
                        }
                        signalbox_domain::AttachmentKind::File => {
                            signalbox_process_protocol::UserAttachmentKind::File
                        }
                    },
                    media_type: media_type.as_str().to_owned(),
                    display_filename: display_filename
                        .as_ref()
                        .map(signalbox_domain::AttachmentDisplayFilename::as_str)
                        .map(str::to_owned),
                },
            })
            .collect(),
    )
}
