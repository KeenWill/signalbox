use super::*;

#[allow(
    clippy::too_many_arguments,
    reason = "request execution keeps connection I/O and durable correlation explicit"
)]
pub(super) async fn handle_request<Writer>(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    mut request: ClientRequest,
    resources: ConnectionRequestResources<'_>,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let review_request = is_review_mutation(&request);
    let ConnectionRequestResources {
        import_permit,
        acquired_bulk_ingest_at,
        mut review_command_permit,
        pending_import,
        pending_blob_upload,
    } = resources;
    debug_assert_eq!(review_request, review_command_permit.is_some());
    let review_digest = if review_request {
        canonical_review_request_digest(&mut request)
    } else {
        None
    };
    let Some(snapshot_permit) = admit_snapshot_reader(
        &request,
        Arc::clone(&services.snapshot_reader_budget),
        &mut shutdown,
    )
    .await?
    else {
        return Ok(());
    };
    if let Some(active_kind) = active_bulk_ingest_kind(pending_import, pending_blob_upload)
        && request_is_cross_kind_bulk_ingest(&request, active_kind)
    {
        return write_bulk_ingest_rejection(writer, version, request_id, active_kind).await;
    }
    match request {
        ClientRequest::CreateSession {
            command_id,
            initial_model_selection,
            model_settings,
            system_prompt,
            placement,
            lifecycle,
        } => {
            handle_create_session(
                writer,
                version,
                request_id,
                WireCreateSessionRequest {
                    command_uuid: command_id.into_uuid(),
                    initial_model_selection,
                    model_settings,
                    system_prompt,
                    placement,
                    lifecycle,
                },
                services,
            )
            .await
        }
        ClientRequest::CreateSessionFromTemplate {
            command_id,
            template_name,
            placement,
            lifecycle,
        } => {
            handle_create_session_from_template(
                writer,
                version,
                request_id,
                WireCreateSessionFromTemplateRequest {
                    command_uuid: command_id.into_uuid(),
                    template_name,
                    placement,
                    lifecycle,
                },
                services,
            )
            .await
        }
        ClientRequest::StopSession {
            command_id,
            session_id,
            sticky,
            descendant_scope,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Stop {
                    sticky: if sticky {
                        StopStickiness::Sticky
                    } else {
                        StopStickiness::Redispatchable
                    },
                    descendant_scope: decode_descendant_scope(descendant_scope),
                },
                services,
            )
            .await
        }
        ClientRequest::SupersedeSession {
            command_id,
            session_id,
            successor_session_id,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Supersede {
                    successor: SessionId::from_uuid(successor_session_id.into_uuid()),
                },
                services,
            )
            .await
        }
        ClientRequest::AbandonSession {
            command_id,
            session_id,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Abandon,
                services,
            )
            .await
        }
        ClientRequest::CloseSessionFailed {
            command_id,
            session_id,
            cause,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::CloseFailed {
                    cause: cause.map(domain_session_failure_cause),
                },
                services,
            )
            .await
        }
        ClientRequest::ResumeSession {
            command_id,
            session_id,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Resume,
                services,
            )
            .await
        }
        ClientRequest::AdoptSession {
            command_id,
            session_id,
            finish_condition,
        } => {
            let finish_condition = match finish_condition.map(domain_finish_condition).transpose() {
                Ok(finish_condition) => finish_condition,
                Err(_) => {
                    return write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::InvalidRequest),
                    )
                    .await;
                }
            };
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Adopt { finish_condition },
                services,
            )
            .await
        }
        ClientRequest::ReleaseSession {
            command_id,
            session_id,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::Release,
                services,
            )
            .await
        }
        ClientRequest::ReleaseStart {
            command_id,
            session_id,
        } => {
            handle_session_lifecycle_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                SessionLifecycleOperation::ReleaseStart,
                services,
            )
            .await
        }
        ClientRequest::CommissionSession {
            command_id,
            template_name,
            fence,
            statement,
            content,
        } => {
            handle_commission_session(
                writer,
                version,
                request_id,
                WireCommissionSessionRequest {
                    command_uuid: command_id.into_uuid(),
                    template_name,
                    fence,
                    statement,
                    content,
                },
                services,
            )
            .await
        }
        ClientRequest::CreateSessionFromImportedFrontier {
            command_id,
            imported_conversation_id,
            through_position,
            relationship,
            initial_model_selection,
            model_settings,
        } => {
            handle_create_session_from_imported_frontier(
                writer,
                version,
                request_id,
                WireImportedContinuationRequest {
                    command_uuid: command_id.into_uuid(),
                    conversation: imported_conversation_id,
                    through_position,
                    relationship,
                    initial_model_selection,
                    model_settings,
                },
                &services.pool,
                services.model_configuration.as_ref(),
                &services.imported_conversations,
            )
            .await
        }
        ClientRequest::CompactSession {
            command_id,
            session_id,
            through_position,
        } => {
            handle_compact_session(
                writer,
                version,
                request_id,
                command_id,
                session_id,
                through_position,
                services,
            )
            .await
        }
        ClientRequest::ReadImportedConversation {
            imported_conversation_id,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_imported_conversation(
                writer,
                version,
                request_id,
                imported_conversation_id,
                &services.pool,
                &services.model_configuration,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListSessions {} => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_sessions(writer, version, request_id, &services.pool, snapshot_permit).await
        }
        ClientRequest::ReadOperatorStatus {} => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            Box::pin(handle_operator_status(
                writer,
                version,
                request_id,
                &services.pool,
                snapshot_permit,
            ))
            .await
        }
        ClientRequest::UpdateSessionPlacement {
            command_id,
            session_id,
            expected_placement_version,
            replacement,
        } => {
            handle_update_session_placement(
                writer,
                version,
                request_id,
                WireSessionPlacementUpdateRequest {
                    command_id,
                    session_id,
                    expected_version: expected_placement_version,
                    replacement,
                },
                &services.pool,
            )
            .await
        }
        ClientRequest::AttachGoal {
            command_id,
            session_id,
            statement,
        } => {
            let Ok(statement) = GoalStatement::try_new(statement) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Attach(statement),
                services,
            )
            .await
        }
        ClientRequest::ReadGoal { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_goal(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ResumeGoal {
            command_id,
            session_id,
            guidance,
        } => {
            let Ok(guidance) = guidance.map(GoalGuidance::try_new).transpose() else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Resume(guidance),
                services,
            )
            .await
        }
        ClientRequest::StopGoal {
            command_id,
            session_id,
            descendant_scope,
        } => {
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Stop {
                    descendant_scope: decode_descendant_scope(descendant_scope),
                },
                services,
            )
            .await
        }
        ClientRequest::SupersedeGoal {
            command_id,
            session_id,
            statement,
        } => {
            let Ok(statement) = GoalStatement::try_new(statement) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Supersede(statement),
                services,
            )
            .await
        }
        ClientRequest::ListTemplates {} => {
            handle_list_templates(
                writer,
                version,
                request_id,
                services.template_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadDeploymentLimits {} => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::DeploymentLimits {
                    max_message_utf8_bytes: configured_u64(
                        &services.model_configuration,
                        "max_message_utf8_bytes",
                    )
                    .map(CanonicalU64::new),
                    max_system_prompt_utf8_bytes: configured_u64(
                        &services.model_configuration,
                        "max_system_prompt_utf8_bytes",
                    )
                    .map(CanonicalU64::new),
                    terminal_input_channel_capacity: configured_u64(
                        &services.model_configuration,
                        "terminal_input_channel_capacity",
                    )
                    .map(CanonicalU64::new),
                    min_metadata_page_size: configured_u64(
                        &services.model_configuration,
                        "min_metadata_page_size",
                    )
                    .map(CanonicalU64::new),
                    max_metadata_page_size: configured_u64(
                        &services.model_configuration,
                        "max_metadata_page_size",
                    )
                    .map(CanonicalU64::new),
                    max_review_findings_per_run: configured_u64(
                        &services.model_configuration,
                        "max_review_findings_per_run",
                    )
                    .map(CanonicalU64::new),
                },
            )
            .await
        }
        ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version,
            model_settings,
            delivery,
        } => {
            handle_submit_input(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                content,
                expected_defaults_version,
                model_settings,
                delivery,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
                services.blob_store_registry.as_deref(),
            )
            .await
        }
        ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version,
            model_settings,
        } => {
            handle_reconcile_turn(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_active_turn_id,
                content,
                expected_defaults_version,
                model_settings,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
                services.blob_store_registry.as_deref(),
            )
            .await
        }
        ClientRequest::ReadTranscript { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_transcript(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                &services.model_configuration,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::FollowSession { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_follow_session(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                &services.model_configuration,
                &services.fanouts,
                shutdown,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListSessionMetadata {
            required_tags,
            title_contains,
            include_archived,
            page_size,
            after_session_id,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_session_metadata(
                writer,
                version,
                request_id,
                WireMetadataPageRequest {
                    required_tags,
                    title_contains,
                    include_archived,
                    page_size,
                    after_session_id,
                },
                &services.pool,
                &services.model_configuration,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListConversations {
            title_contains,
            origin,
            include_archived,
            page_size,
            after,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_conversations(
                writer,
                version,
                request_id,
                WireConversationPageRequest {
                    title_contains,
                    origin,
                    include_archived,
                    page_size,
                    after,
                },
                &services.pool,
                &services.model_configuration,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListModelAliases {} => {
            handle_list_model_aliases(
                writer,
                version,
                request_id,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ListModelCapabilities {} => {
            handle_list_model_capabilities(
                writer,
                version,
                request_id,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadSessionMetadata { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_session_metadata(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReplaceSessionMetadata {
            command_id,
            session_id,
            metadata,
        } => {
            handle_replace_session_metadata(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                metadata,
                &services.pool,
                &services.model_configuration,
            )
            .await
        }
        ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version,
            model_selection,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt,
        } => {
            handle_replace_session_defaults(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_defaults_version,
                model_selection,
                model_settings,
                dangerous_tool_auto_approval,
                system_prompt,
                &services.pool,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadSessionDefaults {
            session_id,
            defaults_version,
        } => {
            handle_read_session_defaults(
                writer,
                version,
                request_id,
                session_id,
                defaults_version,
                &services.pool,
            )
            .await
        }
        ClientRequest::ImportConversation { format, source } => {
            if pending_import.is_some() {
                drop(source);
                return write_import_rejection(
                    writer,
                    version,
                    request_id,
                    RejectionDetail::ConversationImportAlreadyInProgress {},
                )
                .await;
            }
            let source = source.into_bytes();
            let source_size =
                u64::try_from(source.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
            let limit = services
                .model_configuration
                .conversation_import_max_source_bytes();
            if source.len() > limit {
                let detail = RejectionDetail::ConversationImportSourceTooLarge {
                    limit_bytes: wire_size(limit)?,
                    declared_size_bytes: CanonicalU64::new(source_size),
                    actual_size_bytes: Some(CanonicalU64::new(source_size)),
                };
                drop(source);
                return write_import_rejection(writer, version, request_id, detail).await;
            }
            let import_permit = import_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
            handle_import_conversation(
                writer,
                version,
                request_id,
                format,
                source,
                services.imported_conversations.clone(),
                import_permit,
            )
            .await
        }
        ClientRequest::BeginConversationImport {
            format,
            declared_size_bytes,
        } => {
            handle_begin_conversation_import(
                writer,
                version,
                request_id,
                format,
                declared_size_bytes,
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                import_permit,
                acquired_bulk_ingest_at,
                pending_import,
            )
            .await
        }
        ClientRequest::AppendConversationImport { chunk } => {
            handle_append_conversation_import(
                writer,
                version,
                request_id,
                chunk.into_bytes(),
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                pending_import,
            )
            .await
        }
        ClientRequest::CommitConversationImport {} => {
            handle_commit_conversation_import(
                writer,
                version,
                request_id,
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                services.imported_conversations.clone(),
                pending_import,
            )
            .await
        }
        ClientRequest::AbortConversationImport {} => {
            handle_abort_conversation_import(writer, version, request_id, pending_import).await
        }
        ClientRequest::BeginBlobUpload {
            expected_digest,
            expected_length_bytes,
        } => {
            handle_begin_blob_upload(
                writer,
                version,
                request_id,
                expected_digest,
                expected_length_bytes,
                import_permit,
                acquired_bulk_ingest_at,
                services,
                pending_blob_upload,
            )
            .await
        }
        ClientRequest::AppendBlobUpload { chunk } => {
            handle_append_blob_upload(
                writer,
                version,
                request_id,
                chunk.into_bytes(),
                pending_blob_upload,
            )
            .await
        }
        ClientRequest::CommitBlobUpload {} => {
            handle_commit_blob_upload(writer, version, request_id, services, pending_blob_upload)
                .await
        }
        ClientRequest::AbortBlobUpload {} => {
            handle_abort_blob_upload(writer, version, request_id, pending_blob_upload).await
        }
        ClientRequest::ReadBlobMetadata { digest } => {
            handle_read_blob_metadata(
                reader, writer, version, request_id, digest, services, shutdown,
            )
            .await
        }
        ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes,
            length_bytes,
        } => {
            handle_read_blob_chunk(
                reader,
                writer,
                version,
                request_id,
                digest,
                offset_bytes,
                length_bytes,
                services,
                shutdown,
            )
            .await
        }
        ClientRequest::CreateReviewTarget {
            command_id,
            target_id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_target_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_create_review_target(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                target_id,
                provider,
                repository,
                subject,
                head_revision,
                base_revision,
                stack_parent_target_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::StartReviewRun {
            command_id,
            target_id,
            run_id,
            pass_id,
            workflow,
            session_id,
            accepted_input_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_start_review_run(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                target_id,
                run_id,
                pass_id,
                workflow,
                session_id,
                accepted_input_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::ActivateReviewPass {
            command_id,
            run_id,
            pass_id,
            turn_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_activate_review_pass(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::CompleteReviewPass {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            outcome,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_complete_review_pass(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                outcome,
                &services.pool,
            )
            .await
        }
        ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_record_review_findings(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                findings,
                &services.pool,
            )
            .await
        }
        ClientRequest::RecordReviewFindingEvent {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding_id,
            event_ordinal,
            event,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_record_review_disposition(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                finding_id,
                event_ordinal,
                event,
                &services.pool,
            )
            .await
        }
        ClientRequest::ReserveReviewExternalLink {
            command_id,
            external_link_id,
            finding_id,
            provider,
            object_kind,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_reserve_review_external_link(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                external_link_id,
                finding_id,
                provider,
                object_kind,
                &services.pool,
            )
            .await
        }
        ClientRequest::AttachReviewExternalLink {
            command_id,
            external_link_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            external_object,
            event_ordinal,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_attach_review_external_link(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                external_link_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                external_object,
                event_ordinal,
                &services.pool,
            )
            .await
        }
        request @ (ClientRequest::StartReviewOrchestration { .. }
        | ClientRequest::RecordReviewImportOutcome { .. }
        | ClientRequest::RecordReviewConcernOutcome { .. }
        | ClientRequest::RecordReviewJudgmentPlan { .. }
        | ClientRequest::RecordReviewJudgmentEffect { .. }
        | ClientRequest::RecordReviewRepairOutcomes { .. }
        | ClientRequest::RecordReviewPublicationOutcomes { .. }) => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_review_orchestration_mutation(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                request,
                &services.pool,
                services.template_configuration.as_ref(),
            )
            .await
        }
        request @ ClientRequest::ReadReviewOrchestration { .. } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            run_until_shutdown(
                &mut shutdown,
                handle_read_review_orchestration(
                    writer,
                    version,
                    request_id,
                    request,
                    &services.pool,
                    services.template_configuration.as_ref(),
                    snapshot_permit,
                ),
            )
            .await
            .unwrap_or(Ok(()))
        }
        ClientRequest::ReadReviewTarget { target_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_target(
                writer,
                version,
                request_id,
                target_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReadReviewRun { run_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_run(
                writer,
                version,
                request_id,
                run_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReadReviewFinding { finding_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_finding(
                writer,
                version,
                request_id,
                finding_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListReviewFindings { run_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_review_findings(
                writer,
                version,
                request_id,
                run_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version,
            descendant_scope,
            model_settings,
        } => {
            handle_stop_turn(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_active_turn_id,
                content,
                expected_defaults_version,
                decode_descendant_scope(descendant_scope),
                model_settings,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
                services.blob_store_registry.as_deref(),
            )
            .await
        }
        ClientRequest::SpawnSession { .. } => {
            reject_uncomposed_spawn(writer, version, request_id).await
        }
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id,
            child_session_id,
            mode,
        } => {
            handle_await_session(
                reader,
                writer,
                version,
                request_id,
                session_id,
                turn_id,
                tool_request_id,
                child_session_id,
                mode,
                services,
                shutdown,
            )
            .await
        }
        ClientRequest::SendSessionMessage {
            session_id,
            turn_id,
            tool_request_id,
            peer_session_id,
            content,
        } => {
            handle_send_session_message(
                writer,
                version,
                request_id,
                session_id,
                turn_id,
                tool_request_id,
                peer_session_id,
                content,
                &services.pool,
                &services.eligibility_nudge,
            )
            .await
        }
        ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision,
        } => {
            handle_decide_tool_request(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                tool_request_id,
                decision,
                &services.pool,
                &services.eligibility_nudge,
            )
            .await
        }
        ClientRequest::OverrideDeniedToolRequest {
            command_id,
            session_id,
            tool_request_id,
        } => {
            handle_override_denied_tool_request(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                tool_request_id,
                &services.pool,
            )
            .await
        }
    }
}
