use super::*;

enum DelegationResponse {
    Spawned {
        tool_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        relationship: DelegationPolicy,
    },
    AwaitRegistered {
        tool_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        mode: DelegationWaitMode,
    },
    ChildResult {
        await_request_id: CanonicalUuid,
        spawning_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        outcome: DelegationOutcome,
        content: Option<String>,
        reason: DelegationReason,
        provenance: DelegationProvenance,
    },
    MessageSent {
        tool_request_id: CanonicalUuid,
        message_id: CanonicalUuid,
        direction: DelegationMessageDirection,
        ordinal: CanonicalU64,
        delivery_sequence: CanonicalU64,
    },
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

#[derive(Clone, Copy)]
pub(crate) enum DelegationRejectionOperation {
    Spawn,
    Await {
        child: CanonicalUuid,
        mode: DelegationWaitMode,
    },
    Message {
        peer: CanonicalUuid,
    },
}

impl DelegationRejectionOperation {
    const fn is_spawn(self) -> bool {
        match self {
            Self::Spawn => true,
            Self::Await { .. } | Self::Message { .. } => false,
        }
    }

    const fn is_await(self) -> bool {
        match self {
            Self::Await { .. } => true,
            Self::Spawn | Self::Message { .. } => false,
        }
    }

    const fn is_message(self) -> bool {
        match self {
            Self::Message { .. } => true,
            Self::Spawn | Self::Await { .. } => false,
        }
    }

    const fn peer(self) -> Option<CanonicalUuid> {
        match self {
            Self::Spawn => None,
            Self::Await { child, .. } => Some(child),
            Self::Message { peer } => Some(peer),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DelegationRejectionExpectation {
    pub(crate) session: CanonicalUuid,
    pub(crate) turn: CanonicalUuid,
    pub(crate) tool_request: CanonicalUuid,
    pub(crate) operation: DelegationRejectionOperation,
}

pub(crate) fn delegation_rejection_matches(
    detail: Option<RejectionDetail>,
    expected: DelegationRejectionExpectation,
) -> bool {
    let Some(detail) = detail else {
        return false;
    };
    match detail {
        RejectionDetail::DelegationRequestNotInTurn {
            session_id,
            turn_id,
            tool_request_id,
        } => {
            session_id == expected.session
                && turn_id == expected.turn
                && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationToolRequestNotExecutable {
            tool_request_id, ..
        } => tool_request_id == expected.tool_request,
        RejectionDetail::SessionNotFound { session_id } => session_id == expected.session,
        RejectionDetail::ToolRequestNotFound { tool_request_id } => {
            tool_request_id == expected.tool_request
        }
        RejectionDetail::ToolRequestNotInSession {
            session_id,
            tool_request_id,
        } => session_id == expected.session && tool_request_id == expected.tool_request,
        RejectionDetail::DelegationSpawnConflict { tool_request_id } => {
            expected.operation.is_spawn() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegatedChildIdentityCollision { .. } => false,
        RejectionDetail::DelegationRelationNotFound {
            session_id,
            peer_session_id,
        } => {
            !expected.operation.is_spawn()
                && session_id == expected.session
                && expected.operation.peer() == Some(peer_session_id)
        }
        RejectionDetail::DelegationAwaitConflict { tool_request_id } => {
            expected.operation.is_await() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationMessageConflict { tool_request_id } => {
            expected.operation.is_message() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationMessageIdentityCollision { .. } => false,
        RejectionDetail::DelegationEventOrdinalExhausted { .. } => false,
        RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id,
            ..
        } => match expected.operation {
            DelegationRejectionOperation::Spawn => false,
            DelegationRejectionOperation::Await {
                mode: DelegationWaitMode::Background,
                ..
            } => recipient_session_id == expected.session,
            DelegationRejectionOperation::Await {
                mode: DelegationWaitMode::Foreground,
                ..
            } => false,
            DelegationRejectionOperation::Message { peer } => recipient_session_id == peer,
        },
        RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. }
        | RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    }
}

fn classify_delegation_response(message: ServerMessage) -> DelegationResponse {
    match message {
        ServerMessage::SessionSpawned {
            tool_request_id,
            child_session_id,
            relationship,
        } => DelegationResponse::Spawned {
            tool_request_id,
            child_session_id,
            relationship,
        },
        ServerMessage::SessionAwaitRegistered {
            tool_request_id,
            child_session_id,
            mode,
        } => DelegationResponse::AwaitRegistered {
            tool_request_id,
            child_session_id,
            mode,
        },
        ServerMessage::ChildResult {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        } => DelegationResponse::ChildResult {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        },
        ServerMessage::SessionMessageSent {
            tool_request_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        } => DelegationResponse::MessageSent {
            tool_request_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        },
        ServerMessage::Error {
            code,
            message,
            detail,
        } => DelegationResponse::Error {
            code,
            message,
            detail,
        },
        ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionCommissioned { .. }
        | ServerMessage::SessionLifecycleCommandApplied { .. }
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
        | ServerMessage::ConfigurationReloaded { .. }
        | ServerMessage::ConfigurationReloadFailed { .. }
        | ServerMessage::DeploymentLimits { .. } => DelegationResponse::Unexpected,
    }
}

async fn read_delegation_text_argument(
    argument: DelegationTextArgument,
) -> Result<String, ClientError> {
    match argument {
        DelegationTextArgument::Inline(text) => validate_delegation_content(text),
        DelegationTextArgument::File(path) => read_delegation_content_file(&path).await,
    }
}

pub(crate) async fn read_delegation_content_file(path: &Path) -> Result<String, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ClientError::delegation_content_file(path, error))?;
    let read_limit = u64::try_from(MAX_CONTENT_FRAGMENT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol(
            "delegation content read bound overflow",
        ))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ClientError::delegation_content_file(path, error))?;
    if bytes.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "delegation content exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| ClientError::delegation_content_file_utf8(path, error))?;
    validate_delegation_content(text)
}

fn validate_delegation_content(text: String) -> Result<String, ClientError> {
    if text.is_empty() || text.len() > MAX_CONTENT_FRAGMENT_BYTES || text.contains('\0') {
        return Err(ClientError::Input(
            "delegation content must be nonempty, at most 1 MiB, and contain no U+0000",
        ));
    }
    Ok(text)
}

pub(crate) async fn session_delegation(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: SessionCommand,
) -> Result<(), ClientError> {
    match command {
        SessionCommand::Spawn {
            session_id,
            turn_id,
            tool_request_id,
            task,
            relationship,
        } => {
            let task = read_delegation_text_argument(task).await?;
            let mut connection = client
                .mutation_request(ClientRequest::SpawnSession {
                    session_id,
                    turn_id,
                    tool_request_id,
                    task,
                    relationship,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::Spawned {
                    tool_request_id: recorded_request,
                    child_session_id,
                    relationship: recorded_relationship,
                } => {
                    if recorded_request == tool_request_id
                        && child_session_id != session_id
                        && recorded_relationship == relationship
                    {
                        output.session_spawned(SessionSpawnedPresentation {
                            tool_request_id,
                            child_session_id,
                            relationship,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("spawn returned an unexpected receipt")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Spawn,
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "spawn returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::AwaitRegistered { .. }
                | DelegationResponse::ChildResult { .. }
                | DelegationResponse::MessageSent { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("spawn returned an unexpected receipt").mutation())
                }
            }
        }
        SessionCommand::Await {
            session_id,
            turn_id,
            tool_request_id,
            child_session_id,
            mode,
        } => {
            let mut connection = client
                .mutation_request(ClientRequest::AwaitSession {
                    session_id,
                    turn_id,
                    tool_request_id,
                    child_session_id,
                    mode,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::AwaitRegistered {
                    tool_request_id: recorded_request,
                    child_session_id: recorded_child,
                    mode: recorded_mode,
                } => {
                    if recorded_request == tool_request_id
                        && recorded_child == child_session_id
                        && recorded_mode == mode
                        && mode == DelegationWaitMode::Background
                        && session_id != child_session_id
                    {
                        output.session_await_registered(SessionAwaitRegisteredPresentation {
                            tool_request_id,
                            child_session_id,
                            mode,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("await returned an unexpected response")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::ChildResult {
                    await_request_id: recorded_request,
                    spawning_request_id,
                    child_session_id: recorded_child,
                    outcome,
                    content,
                    reason,
                    provenance,
                } => {
                    if mode == DelegationWaitMode::Foreground
                        && recorded_request == tool_request_id
                        && recorded_child == child_session_id
                        && session_id != child_session_id
                        && delegation_provenance_matches(
                            DelegationProvenanceExpectation {
                                parent_session_id: session_id,
                                child_session_id,
                            },
                            provenance,
                        )
                    {
                        output.child_result(ChildResultPresentation {
                            await_request_id: tool_request_id,
                            spawning_request_id,
                            child_session_id,
                            outcome,
                            content: content.as_ref(),
                            reason,
                            provenance,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("await returned an unexpected response")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Await {
                                    child: child_session_id,
                                    mode,
                                },
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "await returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::Spawned { .. }
                | DelegationResponse::MessageSent { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("await returned an unexpected response").mutation())
                }
            }
        }
        SessionCommand::Message {
            session_id,
            turn_id,
            tool_request_id,
            peer_session_id,
            content,
        } => {
            let content = read_delegation_text_argument(content).await?;
            let mut connection = client
                .mutation_request(ClientRequest::SendSessionMessage {
                    session_id,
                    turn_id,
                    tool_request_id,
                    peer_session_id,
                    content,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::MessageSent {
                    tool_request_id: recorded_request,
                    message_id,
                    direction,
                    ordinal,
                    delivery_sequence,
                } => {
                    if recorded_request == tool_request_id && session_id != peer_session_id {
                        output.session_message_sent(SessionMessageSentPresentation {
                            tool_request_id,
                            peer_session_id,
                            message_id,
                            direction,
                            ordinal: ordinal.value(),
                            delivery_sequence: delivery_sequence.value(),
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("message returned an unexpected receipt")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Message {
                                    peer: peer_session_id,
                                },
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "message returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::Spawned { .. }
                | DelegationResponse::AwaitRegistered { .. }
                | DelegationResponse::ChildResult { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("message returned an unexpected receipt").mutation())
                }
            }
        }
    }
}

struct DelegationProvenanceExpectation {
    parent_session_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
}

fn delegation_provenance_matches(
    expectation: DelegationProvenanceExpectation,
    provenance: DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ChildTurn {
            child_session_id, ..
        } => child_session_id == expectation.child_session_id,
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => parent_session_id == expectation.parent_session_id,
    }
}
