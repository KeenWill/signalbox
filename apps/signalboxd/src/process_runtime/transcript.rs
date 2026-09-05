use super::*;

pub(super) async fn handle_read_transcript<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    let spool_result = spool_transcript(
        ProcessReadRepository::new(pool.clone()),
        selected_session,
        version,
        request_id,
        model_configuration,
    )
    .await;
    drop(snapshot_permit);
    let spool = match spool_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, Some(session_id), error)
                .await;
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_transcript(writer, spool).await.map(|_| ())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the versioned follow stream keeps each protocol and runtime boundary explicit"
)]
pub(super) async fn handle_follow_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    fanouts: &ProcessFanouts,
    mut shutdown: watch::Receiver<bool>,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    let mut subscription = fanouts.streaming.subscribe();
    let snapshot_result = run_until_shutdown(
        &mut shutdown,
        spool_transcript(
            ProcessReadRepository::new(pool.clone()),
            selected_session,
            version,
            request_id,
            model_configuration,
        ),
    )
    .await;
    drop(snapshot_permit);
    let Some(snapshot_result) = snapshot_result else {
        return Ok(());
    };
    let spool = match snapshot_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return run_until_shutdown(
                &mut shutdown,
                write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::NotFound),
                ),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return run_until_shutdown(
                &mut shutdown,
                write_process_read_error(writer, version, request_id, Some(session_id), error),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    let mut updates_queued_at_snapshot = subscription.len();
    let Some(snapshot_write) =
        run_until_shutdown(&mut shutdown, write_spooled_transcript(writer, spool)).await
    else {
        return Ok(());
    };
    let mut observed_cursor = snapshot_write?;

    loop {
        let update = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            update = subscription.recv() => update,
        };
        let update = match update {
            Ok(update) => update,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                return run_until_shutdown(
                    &mut shutdown,
                    write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::ResyncRequired),
                    ),
                )
                .await
                .unwrap_or(Ok(()));
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let queued_at_snapshot = consume_snapshot_queued_update(&mut updates_queued_at_snapshot);
        match update {
            ProcessUpdate::Durable {
                cursor,
                session,
                event,
            } => {
                if cursor <= observed_cursor {
                    continue;
                }
                observed_cursor = cursor;
                if session != selected_session {
                    continue;
                }
                let message = ServerMessage::SessionEvent {
                    cursor: CanonicalU64::new(cursor),
                    session_id,
                    event: event.wire()?,
                };
                let Some(event_write) = run_until_shutdown(
                    &mut shutdown,
                    write_message(writer, version, request_id, message),
                )
                .await
                else {
                    return Ok(());
                };
                event_write?;
            }
            ProcessUpdate::ProviderTextDelta(delta) => {
                if queued_at_snapshot || delta.session() != selected_session {
                    continue;
                }
                for content in content_fragments(delta.text()) {
                    let message = ServerMessage::ProviderTextDelta {
                        session_id,
                        turn_id: wire_uuid(delta.turn().into_uuid()),
                        model_call_id: wire_uuid(delta.call().into_uuid()),
                        part_index: CanonicalU64::new(u64::from(delta.part_index())),
                        content,
                    };
                    let Some(delta_write) = run_until_shutdown(
                        &mut shutdown,
                        write_message(writer, version, request_id, message),
                    )
                    .await
                    else {
                        return Ok(());
                    };
                    delta_write?;
                }
            }
        }
    }
}

pub(super) fn consume_snapshot_queued_update(remaining: &mut usize) -> bool {
    if *remaining == 0 {
        false
    } else {
        *remaining -= 1;
        true
    }
}

pub(super) struct TranscriptSpool {
    file: tokio::fs::File,
    cursor: u64,
}

pub(super) enum TranscriptSpoolError {
    Read(ProcessReadError),
    Spool(SnapshotSpoolError),
}

pub(super) async fn spool_transcript(
    repository: ProcessReadRepository,
    session: SessionId,
    version: ProtocolVersion,
    request_id: RequestId,
    model_configuration: &HubModelConfiguration,
) -> Result<Option<TranscriptSpool>, TranscriptSpoolError> {
    let reader = repository.open_transcript(session).await;
    let Some(mut reader) = reader.map_err(TranscriptSpoolError::Read)? else {
        return Ok(None);
    };
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    let session_id = wire_uuid(reader.session().into_uuid());
    let cursor = CanonicalU64::new(reader.cursor());
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::TranscriptSnapshotStart {
            session_id,
            cursor,
            runner: reader
                .runner()
                .map(wire_runner_projection)
                .transpose()
                .map_err(TranscriptSpoolError::Spool)?,
        },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    let mut model_calls_ended = false;
    let mut model_call_count = 0_u64;
    while let Some(item) = reader
        .next_item()
        .await
        .map_err(TranscriptSpoolError::Read)?
    {
        match item {
            ProcessTranscriptItem::Turn(turn) => {
                write_transcript_turn(&mut file, version, request_id, &turn)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::ModelCallUsage(usage) => {
                write_model_call_usage(
                    &mut file,
                    version,
                    request_id,
                    model_call_count,
                    &usage,
                    model_configuration,
                )
                .await
                .map_err(SnapshotSpoolError::from_connection)
                .map_err(TranscriptSpoolError::Spool)?;
                model_call_count = model_call_count
                    .checked_add(1)
                    .ok_or(SnapshotSpoolError::EncodeInvariant)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::Entry(entry) => {
                if !model_calls_ended {
                    write_model_calls_end(&mut file, version, request_id, model_call_count)
                        .await
                        .map_err(SnapshotSpoolError::from_connection)
                        .map_err(TranscriptSpoolError::Spool)?;
                    model_calls_ended = true;
                }
                write_transcript_entry(&mut file, version, request_id, &entry)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
        }
    }
    let summary = reader
        .summary()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(TranscriptSpoolError::Spool)?;
    if !model_calls_ended {
        write_model_calls_end(&mut file, version, request_id, model_call_count)
            .await
            .map_err(SnapshotSpoolError::from_connection)
            .map_err(TranscriptSpoolError::Spool)?;
    }
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::TranscriptSnapshotEnd {
            session_id,
            cursor,
            turn_count: CanonicalU64::new(summary.turn_count()),
            entry_count: CanonicalU64::new(summary.entry_count()),
        },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    Ok(Some(TranscriptSpool {
        file,
        cursor: summary.cursor(),
    }))
}

pub(super) fn wire_runner_projection(
    projection: &ProcessRunnerProjection,
) -> Result<WireRunnerProjection, SnapshotSpoolError> {
    let selector = match projection.selector() {
        RunnerSelector::Identity(runner) => WireRunnerProjectionSelector::Runner {
            runner_id: wire_uuid(runner.into_uuid()),
        },
        RunnerSelector::CapabilityClass(capability) => {
            WireRunnerProjectionSelector::CapabilityClass {
                name: WireRunnerCapabilityClass::try_new(capability.as_str().to_owned())
                    .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
            }
        }
    };
    let sandbox_profile = match projection.sandbox() {
        DomainRunnerSandboxProfile::Ambient => WireRunnerSandboxProfile::Ambient,
        DomainRunnerSandboxProfile::WorkspaceRestricted => {
            WireRunnerSandboxProfile::WorkspaceRestricted
        }
    };
    let state = match projection.state() {
        ProcessRunnerProjectionState::Unpinned => WireRunnerProjectionState::Unpinned,
        ProcessRunnerProjectionState::Pinned => WireRunnerProjectionState::Pinned,
        ProcessRunnerProjectionState::RunnerLostBeforePin => {
            WireRunnerProjectionState::RunnerLostBeforePin
        }
        ProcessRunnerProjectionState::RunnerLost => WireRunnerProjectionState::RunnerLost,
        ProcessRunnerProjectionState::RunnerAbandoned => WireRunnerProjectionState::RunnerAbandoned,
    };
    let connection_health = projection.connection_health().map(|health| match health {
        ProcessRunnerConnectionHealth::Connected => WireRunnerConnectionHealth::Connected,
        ProcessRunnerConnectionHealth::Suspect => WireRunnerConnectionHealth::Suspect,
        ProcessRunnerConnectionHealth::Shutdown => WireRunnerConnectionHealth::Shutdown,
        ProcessRunnerConnectionHealth::Lost => WireRunnerConnectionHealth::Lost,
    });
    WireRunnerProjection::try_new(
        selector,
        projection
            .runner()
            .map(|runner| wire_uuid(runner.into_uuid())),
        WireRunnerPlacementRevision::try_new(projection.placement_revision().get())
            .ok_or(SnapshotSpoolError::EncodeInvariant)?,
        sandbox_profile,
        projection
            .credential_profile()
            .map(|profile| WireRunnerCredentialProfileName::try_new(profile.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        projection
            .repository()
            .map(|repository| WireRunnerRepositoryKey::try_new(repository.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        projection
            .working_directory()
            .map(|directory| WireRunnerWorkingDirectory::try_new(directory.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        connection_health,
        state,
    )
    .map_err(|_| SnapshotSpoolError::EncodeInvariant)
}

pub(super) async fn write_spooled_transcript<Writer>(
    writer: &mut Writer,
    mut spool: TranscriptSpool,
) -> Result<u64, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_spooled_file(writer, &mut spool.file).await?;
    Ok(spool.cursor)
}

pub(super) async fn write_spooled_file<Writer>(
    writer: &mut Writer,
    file: &mut tokio::fs::File,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(ProcessConnectionError::SpoolIo)?;
        if read == 0 {
            return Ok(());
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(ProcessConnectionError::PeerIo)?;
    }
}

pub(super) async fn write_transcript_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    turn: &ProcessTranscriptTurn,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptTurn {
            turn_id: wire_uuid(turn.turn().into_uuid()),
            acceptance_position: CanonicalU64::new(turn.acceptance_position()),
            state: wire_turn_state(turn.state()),
            model_settings: turn.model_settings().map(wire_turn_model_settings),
        },
    )
    .await
}

pub(super) async fn write_model_call_usage<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    model_call_index: u64,
    evidence: &ProcessTranscriptModelCallUsage,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let usage = evidence.usage();
    let cost = model_configuration
        .derive_model_call_cost(
            evidence.target(),
            evidence.credential_profile(),
            crate::configuration::ModelCallInputUsage::from_persisted(
                usage.input_tokens(),
                evidence.input_token_semantics(),
            ),
            usage.output_tokens(),
            usage.cache_creation_input_tokens(),
            usage.cache_read_input_tokens(),
        )
        .map(|cost| -> Result<_, ProcessConnectionError> {
            Ok(ModelCallDollarCost {
                amount_usd: CanonicalDollarAmount::try_new(
                    cost.amount_usd().normalize().to_string(),
                )
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                rate_version: BillingRateVersion::try_new(cost.rate_version().to_owned())
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                label: match cost.billing_kind() {
                    crate::BillingKind::ApiMetered => ModelCallCostLabel::Real,
                    crate::BillingKind::Subscription => ModelCallCostLabel::MeteredEquivalent,
                },
            })
        })
        .transpose()?;
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptModelCallUsage {
            model_call_index: CanonicalU64::new(model_call_index),
            turn_id: wire_uuid(evidence.turn().into_uuid()),
            model_call_id: wire_uuid(evidence.call().into_uuid()),
            usage_provenance: match evidence.provenance() {
                ProcessModelCallUsageProvenance::Reported => UsageProvenance::Reported,
                ProcessModelCallUsageProvenance::Estimated => UsageProvenance::Estimated,
            },
            usage: ModelCallTokenUsage {
                input_tokens: usage.input_tokens().map(CanonicalU64::new),
                output_tokens: usage.output_tokens().map(CanonicalU64::new),
                cache_creation_input_tokens: usage
                    .cache_creation_input_tokens()
                    .map(CanonicalU64::new),
                cache_read_input_tokens: usage.cache_read_input_tokens().map(CanonicalU64::new),
            },
            cost,
        },
    )
    .await
}

pub(super) async fn write_model_calls_end<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    model_call_count: u64,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptModelCallsEnd {
            model_call_count: CanonicalU64::new(model_call_count),
        },
    )
    .await
}

pub(super) async fn write_transcript_entry<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    entry: &ProcessTranscriptEntry,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match entry {
        ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            source_session,
            entry,
            spawning_request,
            parent_session,
            parent_turn,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegatedTask {
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        parent_session_id: wire_uuid(parent_session.into_uuid()),
                        parent_turn_id: wire_uuid(parent_turn.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            source_session,
            entry,
            spawning_request,
            message,
            sender,
            recipient,
            ordinal,
            delivery_sequence,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegationMessage {
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        message_id: wire_uuid(message.into_uuid()),
                        sender_session_id: wire_uuid(sender.into_uuid()),
                        recipient_session_id: wire_uuid(recipient.into_uuid()),
                        ordinal: CanonicalU64::new(*ordinal),
                        delivery_sequence: CanonicalU64::new(*delivery_sequence),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::DelegationResult {
            entry_index,
            source_session,
            entry,
            awaiting_request,
            spawning_request,
            child,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegationResult {
                        await_request_id: wire_uuid(awaiting_request.into_uuid()),
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        child_session_id: wire_uuid(child.into_uuid()),
                        mode: match mode {
                            DispatchedDelegationWaitMode::Foreground => {
                                WireDelegationWaitMode::Foreground
                            }
                            DispatchedDelegationWaitMode::Background => {
                                WireDelegationWaitMode::Background
                            }
                        },
                        delivery_sequence: delivery_sequence.map(CanonicalU64::new),
                        outcome: wire_delegation_outcome(*outcome),
                        content: content.clone(),
                        reason: wire_delegation_reason(*reason),
                        provenance: wire_delegation_provenance(*provenance),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn,
            defaults_version,
            selected,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ModelIdentityChanged {
                        turn_id: wire_uuid(turn.into_uuid()),
                        defaults_version: CanonicalU64::new(*defaults_version),
                        selected_model_id: wire_uuid(selected.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ContextSummary {
            entry_index,
            source_session,
            entry,
            model_call,
            first,
            through,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::ContextSummary {
                        model_call_id: wire_uuid(model_call.into_uuid()),
                        first_source_session_id: wire_uuid(first.source_session().into_uuid()),
                        first_entry_id: wire_uuid(first.entry().into_uuid()),
                        through_source_session_id: wire_uuid(through.source_session().into_uuid()),
                        through_entry_id: wire_uuid(through.entry().into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input,
            turn,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptUserEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                    turn_id: wire_uuid(turn.into_uuid()),
                    content: wire_user_content(content),
                },
            )
            .await
        }
        ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            request,
            name,
            arguments,
            approval,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_name: name.clone(),
                        arguments: arguments.clone(),
                        approval: approval.as_ref().map(|approval| TranscriptToolApproval {
                            decision: match approval.decision() {
                                ToolApprovalDecision::Approve => {
                                    WireToolApprovalEventDecision::Approve {}
                                }
                                ToolApprovalDecision::Deny { reason } => {
                                    WireToolApprovalEventDecision::Deny {
                                        reason: reason
                                            .as_ref()
                                            .map(|reason| reason.as_str().to_owned()),
                                    }
                                }
                            },
                            decider: match approval.decider() {
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
                                    overridden_tool_request_id: wire_uuid(
                                        denied_request.into_uuid(),
                                    ),
                                },
                            },
                            rationale: approval
                                .rationale()
                                .map(|rationale| rationale.as_str().to_owned()),
                        }),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request,
            attempt,
            disposition: _,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolExecutionResult {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolDenied {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolDenied {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolClosed {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnFailed {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnFailed {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                        content_kind: wire_imported_content_kind(*content_kind),
                    },
                },
            )
            .await
        }
    }
}

pub(super) async fn write_content<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    entry_index: u64,
    content: &str,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut fragments = content_fragments(content).peekable();
    let mut fragment_index = 0_u64;
    while let Some(fragment) = fragments.next() {
        let final_fragment = fragments.peek().is_none();
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(entry_index),
                fragment_index: CanonicalU64::new(fragment_index),
                final_fragment,
                content_fragment: fragment,
            },
        )
        .await?;
        if !final_fragment {
            fragment_index = fragment_index
                .checked_add(1)
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
        }
    }
    Ok(())
}

pub(super) fn map_rejection(
    rejected: SubmitInputRejectedResult,
) -> Result<RejectionDetail, ProcessConnectionError> {
    Ok(match rejected {
        SubmitInputRejectedResult::AttachmentBlobNotFound { digest } => {
            RejectionDetail::AttachmentBlobNotFound {
                digest: signalbox_process_protocol::CanonicalBlobDigest::from_digest(digest),
            }
        }
        SubmitInputRejectedResult::AttachmentByteBudgetExceeded { maximum_bytes } => {
            RejectionDetail::AttachmentByteBudgetExceeded {
                maximum_bytes: PositiveCanonicalU64::try_new(maximum_bytes)
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
            }
        }
        SubmitInputRejectedResult::SessionNotFound { session } => {
            RejectionDetail::SessionNotFound {
                session_id: wire_uuid(session.into_uuid()),
            }
        }
        SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        } => RejectionDetail::ActiveTurnPresent {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session,
            expected,
            current,
        } => RejectionDetail::DefaultsVersionMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected: CanonicalU64::new(expected.as_u64()),
            current: CanonicalU64::new(current.as_u64()),
        },
        SubmitInputRejectedResult::UnknownModelAlias { session, alias } => {
            RejectionDetail::UnknownModelAlias {
                session_id: wire_uuid(session.into_uuid()),
                alias_id: wire_uuid(alias.into_uuid()),
            }
        }
        SubmitInputRejectedResult::AcceptancePositionExhausted { session, last } => {
            RejectionDetail::AcceptancePositionExhausted {
                session_id: wire_uuid(session.into_uuid()),
                last: CanonicalU64::new(last.as_u64()),
            }
        }
        SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        } => RejectionDetail::NoActiveTurn {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        } => RejectionDetail::ActiveTurnMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::InterruptAlreadyApplied {
            session,
            active_turn,
            existing_command,
        } => RejectionDetail::InterruptAlreadyApplied {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
            existing_command_id: wire_uuid(*existing_command.as_uuid()),
        },
        SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
            session,
            active_turn,
        } => RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
            session,
            active_turn,
            existing_command,
        } => RejectionDetail::SafePointUnavailableWhileStopping {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
            existing_command_id: wire_uuid(*existing_command.as_uuid()),
        },
    })
}

pub(super) fn domain_model_selection(selection: WireModelSelection) -> ModelSelectionRequest {
    match selection {
        WireModelSelection::Direct { selection_id } => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(selection_id.into_uuid()))
        }
        WireModelSelection::Alias { alias_id } => {
            ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias_id.into_uuid()))
        }
    }
}

pub(super) fn domain_session_placement(
    placement: WireSessionPlacement,
) -> Result<DomainSessionPlacement, ()> {
    match placement {
        WireSessionPlacement::Pathless {} => Ok(DomainSessionPlacement::pathless()),
        WireSessionPlacement::Scoped { path } => {
            DomainSessionPlacement::scoped(SessionPlacementPath::try_new(path).map_err(|_| ())?)
                .map_err(|_| ())
        }
        WireSessionPlacement::RootGlobalRead { path, .. } => {
            DomainSessionPlacement::root_global_read(
                SessionPlacementPath::try_new(path).map_err(|_| ())?,
                signalbox_domain::RootPlacementGlobalReadIntent::Acknowledged,
            )
            .map_err(|_| ())
        }
    }
}

pub(super) fn wire_session_placement(placement: &DomainSessionPlacement) -> WireSessionPlacement {
    match placement.path() {
        None => WireSessionPlacement::Pathless {},
        Some(path) if placement.records_root_global_read_intent() => {
            WireSessionPlacement::RootGlobalRead {
                path: path.as_str().to_owned(),
                intent: signalbox_process_protocol::RootPlacementGlobalReadIntent::Acknowledged,
            }
        }
        Some(path) => WireSessionPlacement::Scoped {
            path: path.as_str().to_owned(),
        },
    }
}

pub(super) fn wire_model_selection(selection: ProcessModelSelection) -> WireModelSelection {
    match selection {
        ProcessModelSelection::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ProcessModelSelection::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

pub(super) fn wire_domain_model_selection(selection: ModelSelectionRequest) -> WireModelSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ModelSelectionRequest::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

pub(super) fn wire_frozen_model_selection(selection: &FrozenModelSelection) -> WireModelSelection {
    match selection {
        FrozenModelSelection::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        FrozenModelSelection::FrozenAlias { alias, .. } => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

pub(super) fn domain_model_settings_overlay(
    value: WireModelSettingsOverlay,
) -> DomainModelSettingsOverlay {
    DomainModelSettingsOverlay::new(
        domain_setting_overlay(value.reasoning_level, domain_reasoning_level),
        domain_fast_mode_overlay(value.fast_mode),
        domain_setting_overlay(value.service_tier, domain_service_tier),
    )
}

pub(super) fn validate_session_model_settings(
    configuration: &HubModelConfiguration,
    selection: ModelSelectionRequest,
    value: DomainModelSettingsOverlay,
) -> Result<ValidatedModelSettings, ModelSettingsAdmissionError> {
    configuration
        .validate_session_model_settings(selection, value)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?
        .map_err(|error| {
            ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
        })
}

pub(super) fn validate_replacement_model_settings(
    configuration: &HubModelConfiguration,
    selection: ModelSelectionRequest,
    caller: DomainModelSettingsOverlay,
    prior: ValidatedModelSettings,
) -> Result<(ValidatedModelSettings, Box<[DomainModelChangeAdjustment]>), ModelSettingsAdmissionError>
{
    let direct = configuration
        .resolve_direct_selection(selection)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let catalog = configuration.model_capability_catalog();
    let capabilities = catalog
        .resolve(direct)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let (profile, global_default) = configuration
        .model_settings_lower_layers(direct)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let precedence = DomainModelSettingsPrecedence::new(
        DomainModelSettingsOverlay::inherit_all(),
        model_settings_overlay_inheriting_from(caller, prior.precedence().session()),
        profile,
        global_default,
    );
    if prior
        .validated_for()
        .is_some_and(|prior_selection| prior_selection != direct)
    {
        return capabilities
            .validate_model_change(direct, precedence, caller)
            .map(signalbox_domain::AdjustedModelSettings::into_parts)
            .map_err(|error| {
                ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
            });
    }
    capabilities
        .validate_precedence(direct, precedence)
        .map(|settings| {
            (
                settings,
                Vec::<DomainModelChangeAdjustment>::new().into_boxed_slice(),
            )
        })
        .map_err(|error| {
            ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
        })
}

pub(super) const fn model_settings_overlay_inheriting_from(
    caller: DomainModelSettingsOverlay,
    prior: DomainModelSettingsOverlay,
) -> DomainModelSettingsOverlay {
    DomainModelSettingsOverlay::new(
        match caller.reasoning_level() {
            DomainSettingOverlay::Inherit => prior.reasoning_level(),
            value @ (DomainSettingOverlay::ProviderDefault | DomainSettingOverlay::Value(_)) => {
                value
            }
        },
        match caller.fast_mode() {
            DomainFastModeOverlay::Inherit => prior.fast_mode(),
            value @ DomainFastModeOverlay::Value(_) => value,
        },
        match caller.service_tier() {
            DomainSettingOverlay::Inherit => prior.service_tier(),
            value @ (DomainSettingOverlay::ProviderDefault | DomainSettingOverlay::Value(_)) => {
                value
            }
        },
    )
}

pub(super) enum ModelSettingsAdmissionError {
    UnknownModel,
    Unsupported(RejectionDetail),
}

pub(super) fn model_settings_protocol_error(error: ModelSettingsAdmissionError) -> ProtocolError {
    match error {
        ModelSettingsAdmissionError::UnknownModel => {
            ProtocolError::without_detail(ErrorCode::InvalidRequest)
        }
        ModelSettingsAdmissionError::Unsupported(detail) => ProtocolError::rejected(detail),
    }
}

pub(super) fn wire_unsupported_model_setting(value: UnsupportedModelSetting) -> RejectionDetail {
    match value {
        UnsupportedModelSetting::ReasoningLevel {
            selection,
            requested,
        } => RejectionDetail::UnsupportedReasoningLevel {
            selection_id: wire_uuid(selection.into_uuid()),
            requested: wire_reasoning_level(requested),
        },
        UnsupportedModelSetting::FastMode { selection } => RejectionDetail::UnsupportedFastMode {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        UnsupportedModelSetting::ServiceTier {
            selection,
            requested,
        } => RejectionDetail::UnsupportedServiceTier {
            selection_id: wire_uuid(selection.into_uuid()),
            requested: wire_service_tier(requested),
        },
    }
}

pub(super) fn domain_setting_overlay<WireT, DomainT>(
    value: WireSettingOverlay<WireT>,
    map: impl FnOnce(WireT) -> DomainT,
) -> DomainSettingOverlay<DomainT> {
    match value {
        WireSettingOverlay::Inherit => DomainSettingOverlay::Inherit,
        WireSettingOverlay::ProviderDefault => DomainSettingOverlay::ProviderDefault,
        WireSettingOverlay::Value(value) => DomainSettingOverlay::Value(map(value)),
    }
}

pub(super) const fn domain_reasoning_level(value: WireReasoningLevel) -> DomainReasoningLevel {
    match value {
        WireReasoningLevel::None => DomainReasoningLevel::None,
        WireReasoningLevel::Minimal => DomainReasoningLevel::Minimal,
        WireReasoningLevel::Low => DomainReasoningLevel::Low,
        WireReasoningLevel::Medium => DomainReasoningLevel::Medium,
        WireReasoningLevel::High => DomainReasoningLevel::High,
        WireReasoningLevel::XHigh => DomainReasoningLevel::XHigh,
        WireReasoningLevel::Max => DomainReasoningLevel::Max,
        WireReasoningLevel::Ultra => DomainReasoningLevel::Ultra,
    }
}

pub(super) const fn wire_reasoning_level(value: DomainReasoningLevel) -> WireReasoningLevel {
    match value {
        DomainReasoningLevel::None => WireReasoningLevel::None,
        DomainReasoningLevel::Minimal => WireReasoningLevel::Minimal,
        DomainReasoningLevel::Low => WireReasoningLevel::Low,
        DomainReasoningLevel::Medium => WireReasoningLevel::Medium,
        DomainReasoningLevel::High => WireReasoningLevel::High,
        DomainReasoningLevel::XHigh => WireReasoningLevel::XHigh,
        DomainReasoningLevel::Max => WireReasoningLevel::Max,
        DomainReasoningLevel::Ultra => WireReasoningLevel::Ultra,
    }
}

pub(super) const fn domain_fast_mode(value: WireFastMode) -> DomainFastMode {
    match value {
        WireFastMode::Disabled => DomainFastMode::Disabled,
        WireFastMode::Enabled => DomainFastMode::Enabled,
    }
}

pub(super) const fn domain_fast_mode_overlay(value: WireFastModeOverlay) -> DomainFastModeOverlay {
    match value {
        WireFastModeOverlay::Inherit => DomainFastModeOverlay::Inherit,
        WireFastModeOverlay::Value(value) => DomainFastModeOverlay::Value(domain_fast_mode(value)),
    }
}

pub(super) const fn wire_fast_mode(value: DomainFastMode) -> WireFastMode {
    match value {
        DomainFastMode::Disabled => WireFastMode::Disabled,
        DomainFastMode::Enabled => WireFastMode::Enabled,
    }
}

pub(super) const fn domain_service_tier(value: WireServiceTier) -> DomainServiceTier {
    match value {
        WireServiceTier::Anthropic(value) => DomainServiceTier::Anthropic(match value {
            signalbox_process_protocol::AnthropicServiceTier::Auto => {
                signalbox_domain::AnthropicServiceTier::Auto
            }
            signalbox_process_protocol::AnthropicServiceTier::StandardOnly => {
                signalbox_domain::AnthropicServiceTier::StandardOnly
            }
        }),
        WireServiceTier::OpenAi(value) => DomainServiceTier::OpenAi(match value {
            signalbox_process_protocol::OpenAiServiceTier::Auto => {
                signalbox_domain::OpenAiServiceTier::Auto
            }
            signalbox_process_protocol::OpenAiServiceTier::Default => {
                signalbox_domain::OpenAiServiceTier::Default
            }
            signalbox_process_protocol::OpenAiServiceTier::Flex => {
                signalbox_domain::OpenAiServiceTier::Flex
            }
            signalbox_process_protocol::OpenAiServiceTier::Scale => {
                signalbox_domain::OpenAiServiceTier::Scale
            }
            signalbox_process_protocol::OpenAiServiceTier::Priority => {
                signalbox_domain::OpenAiServiceTier::Priority
            }
            signalbox_process_protocol::OpenAiServiceTier::Fast => {
                signalbox_domain::OpenAiServiceTier::Fast
            }
        }),
        WireServiceTier::CodexCli(value) => DomainServiceTier::CodexCli(match value {
            signalbox_process_protocol::CodexCliServiceTier::Default => {
                signalbox_domain::CodexCliServiceTier::Default
            }
            signalbox_process_protocol::CodexCliServiceTier::Priority => {
                signalbox_domain::CodexCliServiceTier::Priority
            }
            signalbox_process_protocol::CodexCliServiceTier::Flex => {
                signalbox_domain::CodexCliServiceTier::Flex
            }
        }),
    }
}

pub(super) const fn wire_service_tier(value: DomainServiceTier) -> WireServiceTier {
    match value {
        DomainServiceTier::Anthropic(value) => WireServiceTier::Anthropic(match value {
            signalbox_domain::AnthropicServiceTier::Auto => {
                signalbox_process_protocol::AnthropicServiceTier::Auto
            }
            signalbox_domain::AnthropicServiceTier::StandardOnly => {
                signalbox_process_protocol::AnthropicServiceTier::StandardOnly
            }
        }),
        DomainServiceTier::OpenAi(value) => WireServiceTier::OpenAi(match value {
            signalbox_domain::OpenAiServiceTier::Auto => {
                signalbox_process_protocol::OpenAiServiceTier::Auto
            }
            signalbox_domain::OpenAiServiceTier::Default => {
                signalbox_process_protocol::OpenAiServiceTier::Default
            }
            signalbox_domain::OpenAiServiceTier::Flex => {
                signalbox_process_protocol::OpenAiServiceTier::Flex
            }
            signalbox_domain::OpenAiServiceTier::Scale => {
                signalbox_process_protocol::OpenAiServiceTier::Scale
            }
            signalbox_domain::OpenAiServiceTier::Priority => {
                signalbox_process_protocol::OpenAiServiceTier::Priority
            }
            signalbox_domain::OpenAiServiceTier::Fast => {
                signalbox_process_protocol::OpenAiServiceTier::Fast
            }
        }),
        DomainServiceTier::CodexCli(value) => WireServiceTier::CodexCli(match value {
            signalbox_domain::CodexCliServiceTier::Default => {
                signalbox_process_protocol::CodexCliServiceTier::Default
            }
            signalbox_domain::CodexCliServiceTier::Priority => {
                signalbox_process_protocol::CodexCliServiceTier::Priority
            }
            signalbox_domain::CodexCliServiceTier::Flex => {
                signalbox_process_protocol::CodexCliServiceTier::Flex
            }
        }),
    }
}

pub(super) const fn wire_model_change_adjustment(
    value: DomainModelChangeAdjustment,
) -> WireModelChangeAdjustment {
    match value {
        DomainModelChangeAdjustment::ReasoningLevelClamped { from, to } => {
            WireModelChangeAdjustment::ReasoningLevelClamped {
                from: wire_reasoning_level(from),
                to: wire_reasoning_level(to),
            }
        }
        DomainModelChangeAdjustment::ReasoningLevelCleared { from } => {
            WireModelChangeAdjustment::ReasoningLevelCleared {
                from: wire_reasoning_level(from),
            }
        }
        DomainModelChangeAdjustment::FastModeDisabled => {
            WireModelChangeAdjustment::FastModeDisabled {}
        }
        DomainModelChangeAdjustment::ServiceTierCleared { from } => {
            WireModelChangeAdjustment::ServiceTierCleared {
                from: wire_service_tier(from),
            }
        }
    }
}

pub(super) fn wire_model_settings(settings: ValidatedModelSettings) -> WireModelSettingsSnapshot {
    let precedence = settings.precedence();
    let resolved = settings.resolved();
    let effective = resolved.effective();
    WireModelSettingsSnapshot {
        precedence: WireModelSettingsPrecedence {
            per_call: wire_model_settings_overlay(precedence.per_call()),
            session: wire_model_settings_overlay(precedence.session()),
            profile: wire_model_settings_overlay(precedence.profile()),
            global_default: wire_model_settings_overlay(precedence.global_default()),
        },
        effective: WireEffectiveModelSettings {
            reasoning_level: effective.reasoning_level().map(wire_reasoning_level),
            fast_mode: wire_fast_mode(effective.fast_mode()),
            service_tier: effective.service_tier().map(wire_service_tier),
        },
        reasoning_source: resolved.reasoning_source().map(wire_model_setting_source),
        fast_mode_source: resolved.fast_mode_source().map(wire_model_setting_source),
        service_tier_source: resolved
            .service_tier_source()
            .map(wire_model_setting_source),
        validated_for_selection_id: settings
            .validated_for()
            .map(|selection| wire_uuid(selection.into_uuid())),
    }
}

pub(super) fn wire_turn_model_settings(
    event: &DomainTurnModelSettingsResolved,
) -> WireTurnModelSettingsSnapshot {
    WireTurnModelSettingsSnapshot {
        turn_id: wire_uuid(event.turn().into_uuid()),
        accepted_input_id: wire_uuid(event.accepted_input().into_uuid()),
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
    }
}

pub(super) fn wire_model_settings_overlay(
    value: DomainModelSettingsOverlay,
) -> WireModelSettingsOverlay {
    WireModelSettingsOverlay {
        reasoning_level: wire_setting_overlay(value.reasoning_level(), wire_reasoning_level),
        fast_mode: wire_fast_mode_overlay(value.fast_mode()),
        service_tier: wire_setting_overlay(value.service_tier(), wire_service_tier),
    }
}

pub(super) const fn wire_fast_mode_overlay(value: DomainFastModeOverlay) -> WireFastModeOverlay {
    match value {
        DomainFastModeOverlay::Inherit => WireFastModeOverlay::Inherit,
        DomainFastModeOverlay::Value(value) => WireFastModeOverlay::Value(wire_fast_mode(value)),
    }
}

pub(super) fn wire_setting_overlay<DomainT, WireT>(
    value: DomainSettingOverlay<DomainT>,
    map: impl FnOnce(DomainT) -> WireT,
) -> WireSettingOverlay<WireT> {
    match value {
        DomainSettingOverlay::Inherit => WireSettingOverlay::Inherit,
        DomainSettingOverlay::ProviderDefault => WireSettingOverlay::ProviderDefault,
        DomainSettingOverlay::Value(value) => WireSettingOverlay::Value(map(value)),
    }
}

pub(super) const fn wire_model_setting_source(
    value: DomainModelSettingSource,
) -> WireModelSettingSource {
    match value {
        DomainModelSettingSource::PerCall => WireModelSettingSource::PerCall,
        DomainModelSettingSource::Session => WireModelSettingSource::Session,
        DomainModelSettingSource::Profile => WireModelSettingSource::Profile,
        DomainModelSettingSource::GlobalDefault => WireModelSettingSource::GlobalDefault,
    }
}

/// Maps the presence-checked wire member into the domain's optional bounded
/// prompt. Frame validation already bounds the text; construction failure is
/// a fail-closed invalid request rather than a panic.
pub(super) fn domain_system_prompt(
    member: SystemPromptMember,
    max_utf8_bytes: Option<usize>,
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ()> {
    match member.value() {
        None | Some(None) => Ok(None),
        Some(Some(text)) if max_utf8_bytes.is_some_and(|maximum| text.as_str().len() > maximum) => {
            Err(())
        }
        Some(Some(text)) => {
            signalbox_domain::SessionSystemPrompt::try_new(text.as_str().to_owned())
                .map(Some)
                .map_err(|_| ())
        }
    }
}

/// Maps the domain's optional bounded prompt onto the wire text type.
///
/// The domain admission is strictly at least as strict as the wire's, so a
/// `None` here is fail-closed encode-invariant evidence.
pub(super) fn wire_system_prompt(
    prompt: Option<&signalbox_domain::SessionSystemPrompt>,
) -> Option<Option<SystemPromptText>> {
    match prompt {
        None => Some(None),
        Some(value) => SystemPromptText::try_new(value.as_str().to_owned())
            .ok()
            .map(Some),
    }
}

pub(super) fn wire_list_metadata(
    item: &SessionMetadataListItem,
) -> Option<(Option<String>, Vec<String>, Option<MetadataLastWriter>)> {
    let last_writer = item.last_writer().map(wire_metadata_last_writer);
    Some((
        item.title().map(str::to_owned),
        item.tags().map(str::to_owned).collect(),
        last_writer,
    ))
}

pub(super) fn wire_metadata_snapshot(
    snapshot: &SessionMetadataSnapshot,
) -> Option<(WireSessionMetadata, Option<MetadataLastWriter>)> {
    let content = snapshot.content();
    let metadata = WireSessionMetadata::try_new(
        content.title().map(str::to_owned),
        content.tags().map(str::to_owned).collect(),
        content
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        content.archived(),
    )
    .ok()?;
    let last_writer = snapshot.last_writer().map(wire_metadata_last_writer);
    Some((metadata, last_writer))
}

pub(super) fn wire_metadata_last_writer(writer: SessionMetadataLastWriter) -> MetadataLastWriter {
    let actor = match writer.actor() {
        Actor::User => MetadataActor::User {},
        Actor::Core => MetadataActor::Core {},
        Actor::Model { turn } => MetadataActor::Model {
            turn_id: wire_uuid(turn.into_uuid()),
        },
        Actor::Recovery => MetadataActor::Recovery {},
        Actor::Tool { request } => MetadataActor::Tool {
            tool_request_id: wire_uuid(request.into_uuid()),
        },
    };
    MetadataLastWriter::new(
        CanonicalU64::new(writer.updated_at().as_unix_micros()),
        actor,
    )
}

pub(super) const fn wire_imported_source_speaker(
    source: ProcessImportedSourceSpeaker,
) -> ImportedSourceSpeaker {
    match source {
        ProcessImportedSourceSpeaker::NotAttested => ImportedSourceSpeaker::NotAttested {},
        ProcessImportedSourceSpeaker::AttestedAbsent => ImportedSourceSpeaker::AttestedAbsent {},
        ProcessImportedSourceSpeaker::User => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        },
        ProcessImportedSourceSpeaker::Assistant => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        },
    }
}

pub(super) const fn wire_imported_content_kind(
    kind: ProcessImportedContentKind,
) -> ImportedContentKind {
    match kind {
        ProcessImportedContentKind::SourceEvent => ImportedContentKind::SourceEvent,
        ProcessImportedContentKind::SourceMessageBlock => ImportedContentKind::SourceMessageBlock,
        ProcessImportedContentKind::Text => ImportedContentKind::Text,
        ProcessImportedContentKind::ToolCall => ImportedContentKind::ToolCall,
        ProcessImportedContentKind::ToolResult => ImportedContentKind::ToolResult,
        ProcessImportedContentKind::Thinking => ImportedContentKind::Thinking,
        ProcessImportedContentKind::RedactedThinking => ImportedContentKind::RedactedThinking,
        ProcessImportedContentKind::Document => ImportedContentKind::Document,
        ProcessImportedContentKind::MessageContentAbsent => {
            ImportedContentKind::MessageContentAbsent
        }
    }
}

pub(super) fn wire_turn_state(state: &ProcessTurnState) -> TurnState {
    match state {
        ProcessTurnState::Queued {
            accepted_input,
            content,
        } => TurnState::Queued {
            accepted_input_id: wire_uuid(accepted_input.into_uuid()),
            content: wire_user_content(content),
        },
        ProcessTurnState::QueuedDelegated {
            spawning_request,
            parent_session,
            parent_turn,
            content,
        } => TurnState::QueuedDelegated {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            parent_session_id: wire_uuid(parent_session.into_uuid()),
            parent_turn_id: wire_uuid(parent_turn.into_uuid()),
            content: InputContent::new(content.clone()),
        },
        ProcessTurnState::QueuedDelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } => TurnState::QueuedDelegationWake {
            first_delivery_sequence: CanonicalU64::new(*first_delivery_sequence),
            through_delivery_sequence: CanonicalU64::new(*through_delivery_sequence),
        },
        ProcessTurnState::DelegationTerminated {
            spawning_request,
            outcome,
            reason,
            provenance,
        } => TurnState::DelegationTerminated {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
        },
        ProcessTurnState::ActiveRunning {
            current_attempt,
            current_model_call,
        } => TurnState::ActiveRunning {
            current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            current_model_call: current_model_call.map(|call| {
                CurrentModelCall::new(
                    wire_uuid(call.call().into_uuid()),
                    match call.state() {
                        ProcessCurrentModelCallState::Prepared => {
                            CurrentModelCallState::Prepared {}
                        }
                        ProcessCurrentModelCallState::InFlight => {
                            CurrentModelCallState::InFlight {}
                        }
                        ProcessCurrentModelCallState::CancellationRequested => {
                            CurrentModelCallState::CancellationRequested {}
                        }
                    },
                )
            }),
        },
        ProcessTurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt,
            recovery_call,
            automatic_reconciliation_attempts,
            operator_action_required,
        } => TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_model_call_id: wire_uuid(recovery_call.into_uuid()),
            automatic_reconciliation_attempts: CanonicalU64::new(u64::from(
                *automatic_reconciliation_attempts,
            )),
            operator_action_required: *operator_action_required,
        },
        ProcessTurnState::ActiveAwaitingToolApproval { request } => {
            TurnState::ActiveAwaitingToolApproval {
                tool_request_id: wire_uuid(request.into_uuid()),
            }
        }
        ProcessTurnState::ActiveAwaitingChild {
            awaiting_request,
            spawning_request,
            child,
        } => TurnState::ActiveAwaitingChild {
            await_request_id: wire_uuid(awaiting_request.into_uuid()),
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
        },
        ProcessTurnState::ActiveAwaitingToolRecovery {
            ended_attempt,
            recovery_attempt,
            automatic_reconciliation_attempts,
            operator_action_required,
        } => TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_tool_attempt_id: wire_uuid(recovery_attempt.into_uuid()),
            automatic_reconciliation_attempts: CanonicalU64::new(u64::from(
                *automatic_reconciliation_attempts,
            )),
            operator_action_required: *operator_action_required,
        },
        ProcessTurnState::ActiveAwaitingRunnerRecovery {
            runner,
            placement_revision,
            interrupted_tool_attempt,
        } => TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: wire_uuid(runner.into_uuid()),
            placement_revision: PositiveCanonicalU64::from(*placement_revision),
            tool_attempt_id: interrupted_tool_attempt.map(|attempt| wire_uuid(attempt.into_uuid())),
        },
        ProcessTurnState::Failed {
            terminal_frontier,
            terminal_attempt,
            terminal_model_call,
        } => TurnState::Failed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: terminal_attempt.map(|attempt| wire_uuid(attempt.into_uuid())),
            terminal_model_call: terminal_model_call.map(|call| {
                let model_call_id = wire_uuid(call.call().into_uuid());
                match call.disposition() {
                    ProcessFailedModelCallDisposition::KnownFailed => {
                        let provider_cause = call.provider_failure_cause();
                        let attachment_cause = call.attachment_preparation_failure_cause();
                        debug_assert!(
                            provider_cause.is_none() || attachment_cause.is_none(),
                            "process-read validation rejects overlapping failure causes"
                        );
                        match (provider_cause, attachment_cause) {
                            (Some(cause), _) => FailedTerminalModelCall::known_failed_with_cause(
                                model_call_id,
                                wire_provider_failure_cause(cause),
                            ),
                            (None, Some(cause)) => {
                                FailedTerminalModelCall::known_failed_with_cause(
                                    model_call_id,
                                    wire_attachment_preparation_failure_cause(cause),
                                )
                            }
                            (None, None) => FailedTerminalModelCall::new(
                                model_call_id,
                                FailedModelCallDisposition::KnownFailed,
                            ),
                        }
                    }
                    ProcessFailedModelCallDisposition::Cancelled => {
                        debug_assert!(
                            call.provider_failure_cause().is_none(),
                            "process-read validation rejects causes on cancelled model calls"
                        );
                        debug_assert!(call.attachment_preparation_failure_cause().is_none());
                        FailedTerminalModelCall::new(
                            model_call_id,
                            FailedModelCallDisposition::Cancelled,
                        )
                    }
                }
            }),
        },
        ProcessTurnState::Completed {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Completed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Refused {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Refused {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Cancelled {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Cancelled {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: terminal_call.map(|call| wire_uuid(call.into_uuid())),
        },
        ProcessTurnState::ReconciliationRequired {
            terminal_frontier,
            terminal_attempt,
            operation,
        } => match operation {
            ProcessReconciliationOperation::ModelCall(call) => TurnState::ReconciliationRequired {
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                terminal_model_call_id: wire_uuid(call.into_uuid()),
            },
            ProcessReconciliationOperation::ToolAttempt(attempt) => {
                TurnState::ToolReconciliationRequired {
                    terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                    terminal_tool_attempt_id: wire_uuid(attempt.into_uuid()),
                }
            }
        },
    }
}
