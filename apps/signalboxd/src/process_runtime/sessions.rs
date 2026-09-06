use super::*;

pub(super) struct WireCreateSessionRequest {
    pub(super) command_uuid: uuid::Uuid,
    pub(super) initial_model_selection: WireModelSelection,
    pub(super) model_settings: WireModelSettingsOverlay,
    pub(super) system_prompt: SystemPromptMember,
    pub(super) placement: WireSessionPlacement,
    pub(super) lifecycle: SessionLifecycleMembers,
}

/// The lifecycle members of one creation, admitted into domain values.
pub(super) struct LifecycleMembers {
    start_gate: DomainStartGate,
    ownership: DomainSessionOwnership,
    finish_condition: Option<FinishCondition>,
}

pub(super) fn domain_finish_condition(wire: WireFinishCondition) -> Result<FinishCondition, ()> {
    match wire {
        WireFinishCondition::ExternalGate => Ok(FinishCondition::ExternalGate),
        WireFinishCondition::Declared { statement } => FinishConditionStatement::try_new(statement)
            .map(FinishCondition::Declared)
            .map_err(|_| ()),
    }
}

impl LifecycleMembers {
    fn admit(wire: SessionLifecycleMembers) -> Result<Self, ()> {
        Ok(Self {
            start_gate: match wire.start_gate {
                WireStartGate::Open => DomainStartGate::Open,
                WireStartGate::Held => DomainStartGate::Held,
            },
            ownership: match wire.ownership {
                WireSessionOwnership::Owned => DomainSessionOwnership::Owned,
                WireSessionOwnership::Unmonitored => DomainSessionOwnership::Unmonitored,
            },
            finish_condition: wire
                .finish_condition
                .map(domain_finish_condition)
                .transpose()
                .map_err(|_| ())?,
        })
    }

    /// Whether a recorded creation carries these members.
    fn matches(&self, command: &signalbox_domain::CreateSession) -> bool {
        command.start_gate() == self.start_gate
            && command.ownership() == self.ownership
            && command.finish_condition() == self.finish_condition.as_ref()
    }
}

/// Answers a creation replay from its recorded result.
pub(super) async fn write_recorded_creation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    recorded: &ReconstitutedSessionCreation,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
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
    .await
}

pub(super) async fn handle_create_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    wire_request: WireCreateSessionRequest,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireCreateSessionRequest {
        command_uuid,
        initial_model_selection,
        model_settings,
        system_prompt,
        placement,
        lifecycle,
    } = wire_request;
    let Ok(lifecycle) = LifecycleMembers::admit(lifecycle) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(system_prompt) = domain_system_prompt(
        system_prompt,
        configured_usize(
            &services.model_configuration,
            "max_system_prompt_utf8_bytes",
        ),
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(placement) = domain_session_placement(placement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_selection = domain_model_selection(initial_model_selection);
    let caller_model_settings = domain_model_settings_overlay(model_settings);
    let command_id = DurableCommandId::from_uuid(command_uuid);
    let repository = CreateSessionRepository::new(
        services.pool.clone(),
        services.model_configuration.session_credential_pin(),
    );
    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            let defaults = command.initial_configuration_defaults();
            if defaults.model() == model_selection
                && defaults.dangerous_tool_auto_approval() == DangerousToolAutoApproval::Disabled
                && defaults.system_prompt() == system_prompt.as_ref()
                && defaults.model_settings().precedence().session() == caller_model_settings
                && command.template_provenance().is_none()
                && command.placement() == &placement
                && lifecycle.matches(command)
            {
                return write_recorded_creation(writer, version, request_id, &recorded).await;
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
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }
    let model_settings = match validate_session_model_settings(
        services.model_configuration.as_ref(),
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
        system_prompt,
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
    let request = CreateSessionRequest::try_new(command_id, defaults);
    let Ok(request) = request.map(|request| {
        request.with_placement(placement).with_lifecycle(
            lifecycle.start_gate,
            lifecycle.ownership,
            lifecycle.finish_condition,
        )
    }) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    execute_create_session_request(
        writer,
        version,
        request_id,
        request,
        &services.pool,
        services.model_configuration.as_ref(),
    )
    .await
}

pub(super) struct WireCreateSessionFromTemplateRequest {
    pub(super) command_uuid: uuid::Uuid,
    pub(super) template_name: String,
    pub(super) placement: WireSessionPlacement,
    pub(super) lifecycle: SessionLifecycleMembers,
}

pub(super) async fn handle_create_session_from_template<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireCreateSessionFromTemplateRequest,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireCreateSessionFromTemplateRequest {
        command_uuid: command_id,
        template_name,
        placement,
        lifecycle,
    } = request;
    let Ok(lifecycle) = LifecycleMembers::admit(lifecycle) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(template_name) = SessionTemplateName::try_new(template_name) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(placement) = domain_session_placement(placement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = CreateSessionRepository::new(
        services.pool.clone(),
        services.model_configuration.session_credential_pin(),
    );
    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let recorded_name = recorded
                .command()
                .template_provenance()
                .map(SessionTemplateProvenance::name);
            if recorded_name == Some(&template_name)
                && recorded.command().placement() == &placement
                && lifecycle.matches(recorded.command())
            {
                return write_recorded_creation(writer, version, request_id, &recorded).await;
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
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }

    let Some(template) = services.template_configuration.resolve(&template_name) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = CreateSessionRequest::try_new_from_template(
        command_id,
        template.provenance().clone(),
        template.defaults().clone(),
    );
    let Ok(request) = request.map(|request| {
        request.with_placement(placement).with_lifecycle(
            lifecycle.start_gate,
            lifecycle.ownership,
            lifecycle.finish_condition,
        )
    }) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    execute_create_session_request(
        writer,
        version,
        request_id,
        request,
        &services.pool,
        services.model_configuration.as_ref(),
    )
    .await
}

pub(super) struct WireCommissionSessionRequest {
    pub(super) command_uuid: uuid::Uuid,
    pub(super) template_name: String,
    pub(super) fence: WireCommissionedSessionFence,
    pub(super) statement: String,
    pub(super) content: InputContent,
}

/// Admits one wire fence into its exact domain values.
pub(super) fn domain_commissioned_fence(
    fence: WireCommissionedSessionFence,
) -> Result<ApplicationCommissionedDispatchFence, ()> {
    match fence {
        WireCommissionedSessionFence::PullRequest {
            repository,
            pull_request,
            head_sha,
            head_repository,
            head_branch,
            base_branch,
        } => Ok(ApplicationCommissionedDispatchFence::PullRequest {
            repository: RepositorySlug::try_new(repository).map_err(|_| ())?,
            pull_request: NonZeroU64::new(pull_request.value())
                .map(PullRequestNumber::new)
                .ok_or(())?,
            head_sha: CommitSha::try_new(head_sha).map_err(|_| ())?,
            head_repository: RepositorySlug::try_new(head_repository).map_err(|_| ())?,
            head_branch: BranchName::try_new(head_branch).map_err(|_| ())?,
            base_branch: BranchName::try_new(base_branch).map_err(|_| ())?,
        }),
        WireCommissionedSessionFence::Branch { repository, branch } => {
            Ok(ApplicationCommissionedDispatchFence::Branch {
                repository: RepositorySlug::try_new(repository).map_err(|_| ())?,
                branch: BranchName::try_new(branch).map_err(|_| ())?,
            })
        }
    }
}

pub(super) async fn handle_commission_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireCommissionSessionRequest,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireCommissionSessionRequest {
        command_uuid,
        template_name,
        fence,
        statement,
        content,
    } = request;
    let admitted = (|| {
        let template_name = SessionTemplateName::try_new(template_name).map_err(|_| ())?;
        let statement = GoalStatement::try_new(statement).map_err(|_| ())?;
        let content = UserContent::try_text(content.into_string()).map_err(|_| ())?;
        let fence = domain_commissioned_fence(fence)?;
        CommissionDispatchRequest::try_new(
            DurableCommandId::from_uuid(command_uuid),
            template_name,
            fence,
            statement,
            content,
        )
        .map_err(|_| ())
    })();
    let Ok(request) = admitted else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let store = PostgresCommissionedDispatchStore::new(
        services.pool.clone(),
        services.model_configuration.session_credential_pin(),
    );
    // Replay is resolved from the durable record before the live template
    // catalog is consulted: a committed commission whose response was lost
    // must stay discoverable through a retry of the exact command even after
    // configuration removed or renamed the template it was commissioned from.
    match store.load(request.command_id()).await {
        Ok(Some(recorded)) => {
            return if recorded.matches(&request) {
                // A replay re-arms the queued turn's runnable hint too, so a
                // retry recovers a hint the first response lost.
                let _ = services.eligibility_nudge.nudge(recorded.session());
                write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCommissioned {
                        session_id: wire_uuid(recorded.session().into_uuid()),
                        dispatch_id: wire_uuid(recorded.dispatch().into_uuid()),
                    },
                )
                .await
            } else {
                write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::ConflictingReuse),
                )
                .await
            };
        }
        Ok(None) => {}
        Err(error) => {
            // The lookup is a pre-mutation read: infrastructure failure is the
            // contractually retryable unavailable, and only a durable shape
            // that cannot reconstruct its domain value is corruption.
            let error = match commission_failure_ambiguity(&error) {
                Some(commit_ambiguous) => ProtocolError::mutation_unavailable(commit_ambiguous),
                None => internal_protocol_error(
                    None,
                    InternalDiagnostic::CommissionedDispatchCorruption,
                ),
            };
            return write_error(writer, version, request_id, error).await;
        }
    }
    let Some(template) = services.template_configuration.resolve(request.template()) else {
        // The identity outranks the template. `load` above found no committed
        // commission, so a registry claim on this identity names a conflicting
        // reuse — another command kind, or an ordinary session creation — and
        // `invalid_request` stays reserved for identities that claim nothing.
        let refusal = match store.identity_claimed(request.command_id()).await {
            Ok(true) => ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            Ok(false) => ProtocolError::without_detail(ErrorCode::InvalidRequest),
            Err(error) => match commission_failure_ambiguity(&error) {
                Some(commit_ambiguous) => ProtocolError::mutation_unavailable(commit_ambiguous),
                None => internal_protocol_error(
                    None,
                    InternalDiagnostic::CommissionedDispatchCorruption,
                ),
            },
        };
        return write_error(writer, version, request_id, refusal).await;
    };
    let mut ids = UuidV7CommissionedDispatchIdGenerator;
    let Ok(prepared) = request.prepare(
        &mut ids,
        template.provenance().clone(),
        template.defaults().clone(),
    ) else {
        // The template was resolved by the requested name, so a mismatch or a
        // refused creation command is a daemon defect rather than caller error.
        return write_error(
            writer,
            version,
            request_id,
            internal_protocol_error(None, InternalDiagnostic::CommissionedDispatchCorruption),
        )
        .await;
    };
    let outcome = store
        .commission(prepared, &mut UuidV7SubmitInputIdGenerator, |alias| {
            services.model_configuration.resolve_alias(alias)
        })
        .await;
    match outcome {
        Ok(
            CommissionDispatchOutcome::Dispatched { dispatch, session }
            | CommissionDispatchOutcome::Replayed { dispatch, session },
        ) => {
            // The composite committed a queued turn; hint it runnable now
            // rather than waiting for the periodic reconciliation sweep, as
            // submit-input and repository-watch dispatch do post-commit.
            let _ = services.eligibility_nudge.nudge(session);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionCommissioned {
                    session_id: wire_uuid(session.into_uuid()),
                    dispatch_id: wire_uuid(dispatch.into_uuid()),
                },
            )
            .await
        }
        Ok(CommissionDispatchOutcome::TargetBusy { session })
        | Ok(CommissionDispatchOutcome::TargetCoolingOff { session }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::CommissionTargetBusy {
                    session_id: wire_uuid(session.into_uuid()),
                }),
            )
            .await
        }
        Ok(CommissionDispatchOutcome::ConflictingReuse)
        | Err(CommissionedDispatchRepositoryError::SessionCreation(
            CreateSessionRepositoryError::DifferentCommandKind { .. },
        )) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(error) => {
            let protocol_error = match commission_failure_ambiguity(&error) {
                Some(commit_ambiguous) => ProtocolError::mutation_unavailable(commit_ambiguous),
                None => internal_protocol_error(
                    None,
                    InternalDiagnostic::CommissionedDispatchCorruption,
                ),
            };
            write_error(writer, version, request_id, protocol_error).await
        }
    }
}

/// Classifies one commission failure as a database outage, or none for the
/// fail-closed remainder.
pub(super) fn commission_failure_ambiguity(
    error: &CommissionedDispatchRepositoryError,
) -> Option<bool> {
    match error {
        CommissionedDispatchRepositoryError::Database {
            commit_ambiguous, ..
        } => Some(*commit_ambiguous),
        CommissionedDispatchRepositoryError::SessionCreation(error) => match error {
            CreateSessionRepositoryError::Database(_) => Some(false),
            CreateSessionRepositoryError::CommitAmbiguous(_) => Some(true),
            CreateSessionRepositoryError::DifferentCommandKind { .. }
            | CreateSessionRepositoryError::Corruption(_) => None,
        },
        CommissionedDispatchRepositoryError::InitialInput(error) => match error {
            SubmitInputRepositoryError::Database(_) => Some(false),
            SubmitInputRepositoryError::CommitAmbiguous(_) => Some(true),
            SubmitInputRepositoryError::DifferentCommandKind { .. }
            | SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. }
            | SubmitInputRepositoryError::UnsupportedModelSetting(_)
            | SubmitInputRepositoryError::Corruption(_)
            | SubmitInputRepositoryError::ModelExecution(_) => None,
        },
        CommissionedDispatchRepositoryError::GoalCommission(error) => match error {
            GoalRepositoryError::Database(_) => Some(false),
            GoalRepositoryError::CommitAmbiguous(_) => Some(true),
            GoalRepositoryError::DifferentCommandKind { .. }
            | GoalRepositoryError::Corruption(_) => None,
        },
        CommissionedDispatchRepositoryError::Corruption(_) => None,
    }
}

pub(super) struct WireSessionPlacementUpdateRequest {
    pub(super) command_id: signalbox_process_protocol::CommandId,
    pub(super) session_id: CanonicalUuid,
    pub(super) expected_version: CanonicalU64,
    pub(super) replacement: WireSessionPlacement,
}

pub(super) async fn handle_update_session_placement<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireSessionPlacementUpdateRequest,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireSessionPlacementUpdateRequest {
        command_id,
        session_id,
        expected_version,
        replacement,
    } = request;
    let Some(expected_version) = SessionPlacementVersion::try_from_u64(expected_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(replacement) = domain_session_placement(replacement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let session = SessionId::from_uuid(session_id.into_uuid());
    let Ok(request) = UpdateSessionPlacementRequest::try_new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        session,
        expected_version,
        replacement,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service =
        UpdateSessionPlacementService::new(SessionPlacementRepository::new(pool.clone()));
    match service.execute(request).await {
        Ok(UpdateSessionPlacementOutcome::Recorded(UpdateSessionPlacementResult::Applied(
            applied,
        ))) => {
            let recorded = applied.event().placement();
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionPlacementUpdated {
                    session_id,
                    placement_version: CanonicalU64::new(recorded.version().as_u64()),
                    placement: wire_session_placement(recorded.placement()),
                },
            )
            .await
        }
        Ok(UpdateSessionPlacementOutcome::Recorded(UpdateSessionPlacementResult::Rejected(
            rejected,
        ))) => {
            // Both version-bearing kinds carry their current version by
            // construction, so an absent one is placement-state corruption
            // rather than a rejection this connection can state on the wire.
            let error = match (rejected.kind(), rejected.current_version()) {
                (UpdateSessionPlacementRejectionKind::SessionNotFound, _) => {
                    ProtocolError::rejected(RejectionDetail::SessionNotFound {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                    })
                }
                (UpdateSessionPlacementRejectionKind::CurrentVersionMismatch, Some(current)) => {
                    ProtocolError::rejected(
                        RejectionDetail::SessionPlacementCurrentVersionMismatch {
                            session_id: wire_uuid(rejected.session().into_uuid()),
                            expected_placement_version: CanonicalU64::new(
                                rejected.expected_version().as_u64(),
                            ),
                            current_placement_version: CanonicalU64::new(current.as_u64()),
                        },
                    )
                }
                (UpdateSessionPlacementRejectionKind::VersionExhausted, Some(current)) => {
                    ProtocolError::rejected(RejectionDetail::SessionPlacementVersionExhausted {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        current_placement_version: CanonicalU64::new(current.as_u64()),
                    })
                }
                (
                    UpdateSessionPlacementRejectionKind::CurrentVersionMismatch
                    | UpdateSessionPlacementRejectionKind::VersionExhausted,
                    None,
                ) => internal_protocol_error(
                    Some(rejected.session().into_uuid()),
                    InternalDiagnostic::ProcessReadCorruption,
                ),
            };
            write_error(writer, version, request_id, error).await
        }
        Ok(UpdateSessionPlacementOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::InvalidCommandId) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::Corruption(_)) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, InternalDiagnostic::ProcessReadCorruption),
            )
            .await
        }
    }
}

pub(super) async fn handle_list_templates<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    templates: &SessionTemplateConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TemplatesStart {},
    )
    .await?;
    for (name, template_version) in templates.summaries() {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::TemplateSummary {
                name: name.as_str().to_owned(),
                version: CanonicalU64::new(template_version.as_u64()),
            },
        )
        .await?;
    }
    let template_count = u64::try_from(templates.summaries().len())
        .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TemplatesEnd {
            template_count: CanonicalU64::new(template_count),
        },
    )
    .await
}

pub(super) async fn execute_create_session_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: CreateSessionRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let repository =
        CreateSessionRepository::new(pool.clone(), model_configuration.session_credential_pin());
    match repository.load(request.command_id()).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command.initial_configuration_defaults() == request.initial_configuration_defaults()
                && command.template_provenance() == request.template_provenance()
                && command.placement() == request.placement()
                && command.start_gate() == request.start_gate()
                && command.ownership() == request.ownership()
                && command.finish_condition() == request.finish_condition()
            {
                return write_recorded_creation(writer, version, request_id, &recorded).await;
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
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }

    if model_configuration
        .resolve_session_model(request.initial_configuration_defaults().model())
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

    let model_settings = request.initial_configuration_defaults().model_settings();
    let mut service = CreateSessionService::new(UuidV7SessionIdGenerator, repository);
    match service.execute(request).await {
        Ok(CreateSessionOutcome::Applied(result)) => {
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
        Ok(CreateSessionOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::CommitAmbiguous(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (CreateSessionError::Preparation(_)
            | CreateSessionError::Transaction(
                CreateSessionRepositoryError::DifferentCommandKind { .. }
                | CreateSessionRepositoryError::Corruption(_),
            )),
        ) => {
            let diagnostic = create_session_internal_diagnostic(&error);
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

pub(super) async fn handle_list_sessions<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_session_summaries(
        ProcessReadRepository::new(pool.clone()),
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(SessionListSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, None, error).await;
        }
        Err(SessionListSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

pub(super) async fn handle_operator_status<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_operator_status(
        ProcessOperatorStatusRepository::new(pool.clone()),
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(OperatorStatusSpoolError::Read(ProcessOperatorStatusError::Database(_))) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(OperatorStatusSpoolError::Read(ProcessOperatorStatusError::Corruption(_))) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, InternalDiagnostic::OperatorStatusCorruption),
            )
            .await;
        }
        Err(OperatorStatusSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

pub(super) async fn handle_list_model_aliases<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut aliases = configuration.model_aliases().collect::<Vec<_>>();
    aliases.sort_unstable_by_key(|(alias, _)| alias.into_uuid());
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelAliasesStart {},
    )
    .await?;
    for (alias, selection) in &aliases {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ModelAliasSummary {
                alias_id: wire_uuid(alias.into_uuid()),
                selection_id: wire_uuid(selection.into_uuid()),
            },
        )
        .await?;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelAliasesEnd {
            alias_count: CanonicalU64::new(
                u64::try_from(aliases.len())
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
            ),
        },
    )
    .await
}

pub(super) async fn handle_list_model_capabilities<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let catalog = configuration.model_capability_catalog();
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelCapabilitiesStart {},
    )
    .await?;
    let mut capability_count = 0_u64;
    for (selection, capabilities) in catalog.iter() {
        capability_count = capability_count
            .checked_add(1)
            .ok_or(ProcessConnectionError::EncodeInvariant)?;
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ModelCapabilityItem {
                selection_id: wire_uuid(selection.into_uuid()),
                capabilities: WireModelCapabilities {
                    reasoning_levels: capabilities
                        .reasoning_levels()
                        .iter()
                        .copied()
                        .map(wire_reasoning_level)
                        .collect(),
                    fast_mode_supported: !matches!(
                        capabilities.fast_mode(),
                        signalbox_domain::FastModeSupport::Unsupported
                    ),
                    service_tiers: capabilities
                        .service_tiers()
                        .iter()
                        .copied()
                        .map(wire_service_tier)
                        .collect(),
                },
            },
        )
        .await?;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelCapabilitiesEnd {
            capability_count: CanonicalU64::new(capability_count),
        },
    )
    .await
}

pub(super) struct SessionListSpool {
    file: tokio::fs::File,
}

pub(super) enum SessionListSpoolError {
    Read(ProcessReadError),
    Spool(SnapshotSpoolError),
}

pub(super) enum OperatorStatusSpoolError {
    Read(ProcessOperatorStatusError),
    Spool(SnapshotSpoolError),
}

pub(super) enum SnapshotSpoolError {
    Io(io::Error),
    Encode(FrameEncodeError),
    EncodeInvariant,
}

impl SnapshotSpoolError {
    pub(super) fn from_connection(error: ProcessConnectionError) -> Self {
        match error {
            ProcessConnectionError::PeerIo(error) | ProcessConnectionError::SpoolIo(error) => {
                Self::Io(error)
            }
            ProcessConnectionError::Encode(error) => Self::Encode(error),
            ProcessConnectionError::EncodeInvariant
            | ProcessConnectionError::InboundFrameBudgetClosed
            | ProcessConnectionError::ImportBudgetClosed
            | ProcessConnectionError::ReviewCommandBudgetClosed
            | ProcessConnectionError::SnapshotReaderBudgetClosed => Self::EncodeInvariant,
        }
    }
}

pub(super) async fn write_snapshot_spool_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: SnapshotSpoolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        SnapshotSpoolError::Io(error) => {
            tracing::warn!(error = %error, "process snapshot spooling failed before response");
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await
        }
        SnapshotSpoolError::Encode(error) => Err(ProcessConnectionError::Encode(error)),
        SnapshotSpoolError::EncodeInvariant => Err(ProcessConnectionError::EncodeInvariant),
    }
}

pub(super) async fn spool_session_summaries(
    repository: ProcessReadRepository,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, SessionListSpoolError> {
    let mut reader = repository
        .open_session_summaries()
        .await
        .map_err(SessionListSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsStart {},
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    while let Some(summary) = reader
        .next_summary()
        .await
        .map_err(SessionListSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionSummary {
                session_id: wire_uuid(summary.session().into_uuid()),
                defaults_version: CanonicalU64::new(summary.defaults_version()),
                model_selection: wire_model_selection(summary.model_selection()),
                placement_version: CanonicalU64::new(summary.placement().version().as_u64()),
                placement: wire_session_placement(summary.placement().placement()),
                runner: summary
                    .runner()
                    .map(wire_runner_projection)
                    .transpose()
                    .map_err(SessionListSpoolError::Spool)?,
            },
        )
        .await
        .map_err(SessionListSpoolError::Spool)?;
    }
    let session_count = reader
        .summary_count()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(SessionListSpoolError::Spool)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(session_count),
        },
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

pub(super) async fn spool_operator_status(
    repository: ProcessOperatorStatusRepository,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, OperatorStatusSpoolError> {
    let mut reader = repository
        .open()
        .await
        .map_err(OperatorStatusSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(OperatorStatusSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::Start {})),
    )
    .await
    .map_err(OperatorStatusSpoolError::Spool)?;
    while let Some(item) = reader
        .next_item()
        .await
        .map_err(OperatorStatusSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            wire_operator_status_item(item),
        )
        .await
        .map_err(OperatorStatusSpoolError::Spool)?;
    }
    let counts = reader
        .counts()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(OperatorStatusSpoolError::Spool)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::End(Box::new(
            OperatorStatusEndMessage {
                lifecycle_week_count: CanonicalU64::new(counts.lifecycle_weeks()),
                lifecycle_deadline_violation_count: CanonicalU64::new(
                    counts.lifecycle_deadline_violations(),
                ),
            },
        )))),
    )
    .await
    .map_err(OperatorStatusSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(OperatorStatusSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(OperatorStatusSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

pub(super) const fn wire_lifecycle_state(
    state: LifecycleNonTerminalState,
) -> OperatorStatusLifecycleState {
    match state {
        LifecycleNonTerminalState::Created => OperatorStatusLifecycleState::Created,
        LifecycleNonTerminalState::Dispatched => OperatorStatusLifecycleState::Dispatched,
        LifecycleNonTerminalState::Active => OperatorStatusLifecycleState::Active,
        LifecycleNonTerminalState::Waiting => OperatorStatusLifecycleState::Waiting,
        LifecycleNonTerminalState::Recovering => OperatorStatusLifecycleState::Recovering,
        LifecycleNonTerminalState::Blocked => OperatorStatusLifecycleState::Blocked,
        LifecycleNonTerminalState::Parked => OperatorStatusLifecycleState::Parked,
    }
}

pub(super) fn wire_operator_status_item(item: ProcessOperatorStatusItem) -> ServerMessage {
    match item {
        ProcessOperatorStatusItem::LifecycleWeek(item) => ServerMessage::OperatorStatus(Box::new(
            OperatorStatusMessage::LifecycleWeek(Box::new(OperatorStatusLifecycleWeekMessage {
                week_start_date: item.week_start_date(),
                completion_failure_numerator: CanonicalU64::new(
                    item.completion_failure().numerator(),
                ),
                completion_failure_denominator: CanonicalU64::new(
                    item.completion_failure().denominator(),
                ),
                failed_unknown_count: CanonicalU64::new(item.failed_unknown_share().numerator()),
                overflow_numerator: CanonicalU64::new(item.overflow_incidence().numerator()),
                overflow_denominator: CanonicalU64::new(item.overflow_incidence().denominator()),
                finish_given_overflow_numerator: CanonicalU64::new(
                    item.finish_given_overflow().numerator(),
                ),
                wall_numerator: CanonicalU64::new(item.wall_rate().numerator()),
                wall_denominator: CanonicalU64::new(item.wall_rate().denominator()),
                wall_occurrence_count: CanonicalU64::new(item.wall_occurrences()),
                classified_terminal_turn_count: CanonicalU64::new(
                    item.turn_cause_completeness().numerator(),
                ),
                terminal_turn_count: CanonicalU64::new(
                    item.turn_cause_completeness().denominator(),
                ),
                classified_known_failed_call_count: CanonicalU64::new(
                    item.model_call_cause_completeness().numerator(),
                ),
                known_failed_call_count: CanonicalU64::new(
                    item.model_call_cause_completeness().denominator(),
                ),
            })),
        )),
        ProcessOperatorStatusItem::LifecycleDeadlineViolation(item) => {
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: wire_uuid(item.session().into_uuid()),
                        state: wire_lifecycle_state(item.state()),
                        deadline_missing: item.expired_for_seconds().is_none(),
                        expired_for_seconds: item.expired_for_seconds().map(CanonicalU64::new),
                    },
                )),
            ))
        }
    }
}

pub(super) struct WireMetadataPageRequest {
    pub(super) required_tags: Vec<String>,
    pub(super) title_contains: Option<String>,
    pub(super) include_archived: bool,
    pub(super) page_size: CanonicalU64,
    pub(super) after_session_id: Option<CanonicalUuid>,
}

pub(super) async fn handle_list_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireMetadataPageRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let query = SessionMetadataListQuery::try_new_with_limits(
        request.required_tags,
        request.title_contains,
        request.include_archived,
        request.page_size.value(),
        request
            .after_session_id
            .map(|value| SessionId::from_uuid(value.into_uuid())),
        lower_optional_usize(
            configured_usize(model_configuration, "max_required_tags"),
            configured_usize(model_configuration, "max_session_metadata_required_tags"),
        ),
        configured_u64(model_configuration, "min_metadata_page_size"),
        configured_u64(model_configuration, "max_metadata_page_size"),
    );
    let Ok(query) = query else {
        drop(snapshot_permit);
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let spool_result = spool_session_metadata_page(
        SessionMetadataRepository::new(pool.clone()),
        query,
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(MetadataPageSpoolError::Read(error)) => {
            return write_session_metadata_read_error(writer, version, request_id, None, error)
                .await;
        }
        Err(MetadataPageSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

pub(super) enum MetadataPageSpoolError {
    Read(SessionMetadataRepositoryError),
    Spool(SnapshotSpoolError),
}

pub(super) async fn spool_session_metadata_page(
    repository: SessionMetadataRepository,
    query: SessionMetadataListQuery,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, MetadataPageSpoolError> {
    let mut page = ListSessionMetadataService::new(repository)
        .execute(query)
        .await
        .map_err(MetadataPageSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageStart {},
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    let mut session_count = 0_u64;
    while let Some(item) = page
        .next_item()
        .await
        .map_err(MetadataPageSpoolError::Read)?
    {
        let (title, tags, last_writer) = wire_list_metadata(&item)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionMetadataSummary {
                session_id: wire_uuid(item.session().into_uuid()),
                defaults_version: CanonicalU64::new(item.defaults_version().as_u64()),
                model_selection: wire_domain_model_selection(item.model_selection()),
                dangerous_tool_auto_approval: matches!(
                    item.dangerous_tool_auto_approval(),
                    DangerousToolAutoApproval::ApproveAll
                ),
                title,
                tags,
                archived: item.archived(),
                last_writer,
            },
        )
        .await
        .map_err(MetadataPageSpoolError::Spool)?;
        session_count = session_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
    }
    let next_after_session_id = page
        .next_after_session()
        .map(|session| wire_uuid(session.into_uuid()));
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(session_count),
            next_after_session_id,
        },
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

pub(super) struct WireConversationPageRequest {
    pub(super) title_contains: Option<String>,
    pub(super) origin: WireConversationOriginFilter,
    pub(super) include_archived: bool,
    pub(super) page_size: CanonicalU64,
    pub(super) after: Option<WireConversationCursor>,
}

pub(super) async fn handle_list_conversations<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireConversationPageRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let query = ConversationListQuery::try_new_with_page_limits(
        request.title_contains,
        application_origin_filter(request.origin),
        request.include_archived,
        request.page_size.value(),
        request.after.map(application_cursor),
        configured_u64(model_configuration, "min_metadata_page_size"),
        configured_u64(model_configuration, "max_metadata_page_size"),
    );
    let Ok(query) = query else {
        drop(snapshot_permit);
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let spool_result = spool_conversation_page(
        ConversationListingRepository::new(pool.clone()),
        query,
        version,
        request_id,
        configured_usize(
            model_configuration,
            "max_imported_conversation_display_title_scalars",
        ),
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(ConversationPageSpoolError::Read(error)) => {
            return write_conversation_listing_read_error(writer, version, request_id, error).await;
        }
        Err(ConversationPageSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

pub(super) enum ConversationPageSpoolError {
    Read(ConversationListingRepositoryError),
    Spool(SnapshotSpoolError),
}

pub(super) async fn spool_conversation_page(
    repository: ConversationListingRepository,
    query: ConversationListQuery,
    version: ProtocolVersion,
    request_id: RequestId,
    max_imported_title_scalars: Option<usize>,
) -> Result<SessionListSpool, ConversationPageSpoolError> {
    let mut page = ListConversationsService::new(repository)
        .execute(query)
        .await
        .map_err(ConversationPageSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ConversationPageStart {},
    )
    .await
    .map_err(ConversationPageSpoolError::Spool)?;
    let mut conversation_count = 0_u64;
    while let Some(item) = page
        .next_item()
        .await
        .map_err(ConversationPageSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::ConversationSummary {
                conversation: wire_conversation_summary(item, max_imported_title_scalars),
            },
        )
        .await
        .map_err(ConversationPageSpoolError::Spool)?;
        conversation_count = conversation_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(ConversationPageSpoolError::Spool)?;
    }
    let next_after = page.next_after().map(wire_cursor);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ConversationPageEnd {
            conversation_count: CanonicalU64::new(conversation_count),
            next_after,
        },
    )
    .await
    .map_err(ConversationPageSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

pub(super) const fn application_origin_filter(
    origin: WireConversationOriginFilter,
) -> ConversationOriginFilter {
    match origin {
        WireConversationOriginFilter::Native => ConversationOriginFilter::Native,
        WireConversationOriginFilter::Imported => ConversationOriginFilter::Imported,
        WireConversationOriginFilter::All => ConversationOriginFilter::All,
    }
}

pub(super) fn application_cursor(cursor: WireConversationCursor) -> ConversationListCursor {
    match cursor.origin() {
        WireConversationOrigin::NativeSession => ConversationListCursor::NativeSession(
            SessionId::from_uuid(cursor.conversation_id().into_uuid()),
        ),
        WireConversationOrigin::ImportedConversation => {
            ConversationListCursor::ImportedConversation(ImportedConversationId::from_uuid(
                cursor.conversation_id().into_uuid(),
            ))
        }
    }
}

pub(super) fn wire_cursor(cursor: ConversationListCursor) -> WireConversationCursor {
    match cursor {
        ConversationListCursor::NativeSession(session) => WireConversationCursor::new(
            WireConversationOrigin::NativeSession,
            wire_uuid(session.into_uuid()),
        ),
        ConversationListCursor::ImportedConversation(conversation) => WireConversationCursor::new(
            WireConversationOrigin::ImportedConversation,
            wire_uuid(conversation.into_uuid()),
        ),
    }
}

pub(super) fn wire_conversation_summary(
    item: ConversationListItem,
    max_imported_title_scalars: Option<usize>,
) -> WireConversationSummary {
    match item {
        ConversationListItem::NativeSession {
            session,
            title,
            archived,
            defaults_version,
        } => WireConversationSummary::NativeSession {
            session_id: wire_uuid(session.into_uuid()),
            title,
            archived,
            defaults_version: CanonicalU64::new(defaults_version.as_u64()),
        },
        ConversationListItem::ImportedConversation {
            conversation,
            title,
            entry_count,
            format,
        } => WireConversationSummary::ImportedConversation {
            imported_conversation_id: wire_uuid(conversation.into_uuid()),
            title: configured_imported_title(title, max_imported_title_scalars),
            entry_count: CanonicalU64::new(entry_count),
            source_format: wire_imported_source_format(format),
        },
    }
}

pub(super) fn configured_imported_title(
    title: Option<String>,
    maximum: Option<usize>,
) -> Option<String> {
    let title = title?;
    let Some(maximum) = maximum else {
        return Some(title);
    };
    let truncated = title.chars().take(maximum).collect::<String>();
    let truncated = truncated.trim_end_matches([' ', '\t']);
    (!truncated.is_empty()).then(|| truncated.to_owned())
}

pub(super) const fn wire_imported_source_format(
    format: ImportedConversationFormat,
) -> WireImportedConversationSourceFormat {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            WireImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
            WireImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2
        }
        ImportedConversationFormat::CodexRolloutJsonlV1 => {
            WireImportedConversationSourceFormat::CodexRolloutJsonlV1
        }
    }
}

pub(super) async fn write_conversation_listing_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ConversationListingRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ConversationListingRepositoryError::Database(_) => {
            ProtocolError::without_detail(ErrorCode::Unavailable)
        }
        ConversationListingRepositoryError::Corruption(_) => {
            internal_protocol_error(None, InternalDiagnostic::ConversationListingCorruption)
        }
    };
    write_error(writer, version, request_id, response).await
}

pub(super) async fn handle_read_session_metadata<Writer>(
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
    let service = LoadSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    let loaded = service
        .execute(SessionId::from_uuid(session_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(snapshot)) => {
            let (metadata, last_writer) =
                wire_metadata_snapshot(&snapshot).ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadata {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(None) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Err(error) => {
            write_session_metadata_read_error(writer, version, request_id, Some(session_id), error)
                .await
        }
    }
}

pub(super) async fn handle_read_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let named_version = match defaults_version {
        None => None,
        Some(value) => match SessionConfigurationDefaultsVersion::try_from_u64(value.value()) {
            Some(version) => Some(version),
            None => {
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
    let repository = ProcessReadRepository::new(pool.clone());
    match repository
        .read_session_defaults(SessionId::from_uuid(session_id.into_uuid()), named_version)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(read)) => {
            let system_prompt = match wire_system_prompt(read.defaults().system_prompt()) {
                Some(system_prompt) => system_prompt,
                None => return Err(ProcessConnectionError::EncodeInvariant),
            };
            write_message_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaults {
                    session_id,
                    defaults_version: CanonicalU64::new(read.version().as_u64()),
                    model_selection: wire_domain_model_selection(read.defaults().model()),
                    model_settings: wire_model_settings(read.defaults().model_settings()),
                    dangerous_tool_auto_approval: matches!(
                        read.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::defaults_epoch_not_found(),
            )
            .await
        }
        Err(error) => {
            write_process_read_error(writer, version, request_id, Some(session_id), error).await
        }
    }
}

// The protocol handler keeps envelope identity beside the decoded payload and
// deployment policy; grouping them would obscure the runtime module's ownership fence.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_replace_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    metadata: WireSessionMetadata,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if configured_usize(model_configuration, "max_session_metadata_tags")
        .is_some_and(|maximum| metadata.tags().len() > maximum)
        || configured_usize(model_configuration, "max_session_metadata_attributes")
            .is_some_and(|maximum| metadata.attributes().len() > maximum)
    {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    }
    let replacement = SessionMetadataContent::try_new(
        metadata.title().map(str::to_owned),
        metadata.tags().map(str::to_owned).collect(),
        metadata
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        metadata.archived(),
    );
    let Ok(replacement) = replacement else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = ReplaceSessionMetadataRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        SessionId::from_uuid(session_id.into_uuid()),
        replacement,
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
    let mut service =
        ReplaceSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    match service.execute(request).await {
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
            applied,
        ))) => {
            let (metadata, last_writer) = wire_metadata_snapshot(applied.snapshot())
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
            let last_writer = last_writer.ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadataReplaced {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(rejected),
        ))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::SessionNotFound {
                    session_id: wire_uuid(rejected.session().into_uuid()),
                }),
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (SessionMetadataRepositoryError::DifferentCommandKind { .. }
            | SessionMetadataRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = session_metadata_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
            )
            .await
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the complete defaults replacement remains explicit at the wire adapter"
)]
pub(super) async fn handle_replace_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_defaults_version: CanonicalU64,
    model_selection: WireModelSelection,
    model_settings: WireModelSettingsOverlay,
    dangerous_tool_auto_approval: bool,
    system_prompt: SystemPromptMember,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
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
    let prompt_member_is_absent = system_prompt.value().is_none();
    let Ok(system_prompt) = domain_system_prompt(
        system_prompt,
        configured_usize(model_configuration, "max_system_prompt_utf8_bytes"),
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let replacement_model = domain_model_selection(model_selection);
    let session = SessionId::from_uuid(session_id.into_uuid());
    let caller_model_settings = domain_model_settings_overlay(model_settings);
    let dangerous_tool_auto_approval = if dangerous_tool_auto_approval {
        DangerousToolAutoApproval::ApproveAll
    } else {
        DangerousToolAutoApproval::Disabled
    };
    let durable_command_id = DurableCommandId::from_uuid(command_id);
    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    match repository.load(durable_command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            let replacement = command.replacement();
            if command.session() == session
                && command.expected_current_version() == expected_version
                && !prompt_member_is_absent
                && replacement.model() == replacement_model
                && replacement.dangerous_tool_auto_approval() == dangerous_tool_auto_approval
                && replacement.system_prompt() == system_prompt.as_ref()
                && command.caller_model_settings() == caller_model_settings
            {
                return write_replace_session_defaults_result(
                    writer,
                    version,
                    request_id,
                    session_id,
                    recorded.result(),
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
        Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(ReplaceSessionDefaultsRepositoryError::Database { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDefaultsCorruption,
                ),
            )
            .await;
        }
    }
    let prompt_member = if prompt_member_is_absent {
        PromptMemberStatement::Unstated
    } else {
        PromptMemberStatement::Stated
    };
    // The immutable catalog decides an unknown direct selection or alias before
    // any application construction below, so that read-only fact can never be
    // recorded under this command identity as a defaults-version mismatch by
    // the rejection-only boundary. Unlike a capability rejection, it depends on
    // no defaults snapshot and therefore cannot lose a race worth replaying.
    if model_configuration
        .resolve_direct_selection(replacement_model)
        .is_none()
    {
        return write_error(
            writer,
            version,
            request_id,
            model_settings_protocol_error(ModelSettingsAdmissionError::UnknownModel),
        )
        .await;
    }
    let prior_model_settings = match ProcessReadRepository::new(pool.clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(read)) if read.version() == expected_version => {
            Some(read.defaults().model_settings())
        }
        Ok(ProcessSessionDefaultsRead::Read(read)) if read.version() > expected_version => None,
        Ok(ProcessSessionDefaultsRead::Read(_)) => {
            let placeholder = SessionConfigurationDefaults::complete(
                replacement_model,
                dangerous_tool_auto_approval,
                system_prompt.clone(),
            );
            let command = DomainReplaceSessionDefaults::with_model_settings_adjustments(
                durable_command_id,
                session,
                expected_version,
                placeholder,
                caller_model_settings,
                Vec::new(),
            );
            match handle_defaults_rejection_only(&repository, command, prompt_member).await {
                Ok(Some(outcome)) => {
                    return write_replace_session_defaults_outcome(
                        writer, version, request_id, session_id, outcome,
                    )
                    .await;
                }
                Ok(None) => {
                    match ProcessReadRepository::new(pool.clone())
                        .read_session_defaults(session, Some(expected_version))
                        .await
                    {
                        Ok(ProcessSessionDefaultsRead::Read(read)) => {
                            Some(read.defaults().model_settings())
                        }
                        Ok(
                            ProcessSessionDefaultsRead::SessionNotFound
                            | ProcessSessionDefaultsRead::VersionNotFound,
                        ) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                internal_protocol_error(
                                    Some(session_id.into_uuid()),
                                    InternalDiagnostic::SessionDefaultsCorruption,
                                ),
                            )
                            .await;
                        }
                        Err(ProcessReadError::Database(_)) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                ProtocolError::mutation_unavailable(false),
                            )
                            .await;
                        }
                        Err(ProcessReadError::Corruption(_)) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                internal_protocol_error(
                                    Some(session_id.into_uuid()),
                                    InternalDiagnostic::SessionDefaultsCorruption,
                                ),
                            )
                            .await;
                        }
                    }
                }
                Err(ReplaceSessionDefaultsRepositoryError::Database {
                    commit_ambiguous, ..
                }) => {
                    return write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::mutation_unavailable(commit_ambiguous),
                    )
                    .await;
                }
                Err(error) => {
                    let diagnostic = session_defaults_internal_diagnostic(&error);
                    return write_error(
                        writer,
                        version,
                        request_id,
                        internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
                    )
                    .await;
                }
            }
        }
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => None,
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            Some(ValidatedModelSettings::provider_defaults())
        }
        Err(ProcessReadError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ProcessReadError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDefaultsCorruption,
                ),
            )
            .await;
        }
    };
    let (model_settings, model_settings_adjustments) = match prior_model_settings {
        Some(prior_model_settings) => match validate_replacement_model_settings(
            model_configuration,
            replacement_model,
            caller_model_settings,
            prior_model_settings,
        ) {
            Ok(settings) => settings,
            Err(error) => {
                // Validation used an unlocked defaults snapshot. Re-enter the
                // pointer-locked rejection boundary before surfacing the
                // caller error, so a racing advance records and replays its
                // authoritative mismatch instead.
                let placeholder = SessionConfigurationDefaults::complete(
                    replacement_model,
                    dangerous_tool_auto_approval,
                    system_prompt.clone(),
                );
                let command = DomainReplaceSessionDefaults::with_model_settings_adjustments(
                    durable_command_id,
                    session,
                    expected_version,
                    placeholder,
                    caller_model_settings,
                    Vec::new(),
                );
                match handle_defaults_rejection_only(&repository, command, prompt_member).await {
                    Ok(Some(outcome)) => {
                        return write_replace_session_defaults_outcome(
                            writer, version, request_id, session_id, outcome,
                        )
                        .await;
                    }
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            model_settings_protocol_error(error),
                        )
                        .await;
                    }
                    Err(ReplaceSessionDefaultsRepositoryError::Database {
                        commit_ambiguous,
                        ..
                    }) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::mutation_unavailable(commit_ambiguous),
                        )
                        .await;
                    }
                    Err(error) => {
                        let diagnostic = session_defaults_internal_diagnostic(&error);
                        return write_error(
                            writer,
                            version,
                            request_id,
                            internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
                        )
                        .await;
                    }
                }
            }
        },
        // A stale epoch can never move backward, and an absent session must be
        // classified by the durable command boundary. The catalog identity was
        // already decided above, so preserve the canonical caller overlay while
        // supplying an inert replacement snapshot and let the transaction
        // record and replay its authoritative rejection first.
        None => (
            ValidatedModelSettings::provider_defaults(),
            Vec::<DomainModelChangeAdjustment>::new().into_boxed_slice(),
        ),
    };
    let Some(replacement) = SessionConfigurationDefaults::complete_with_model_settings(
        replacement_model,
        dangerous_tool_auto_approval,
        system_prompt,
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
    // A member the frame could not state must not silently clear a prompt
    // the current epoch carries; the transaction refuses that atomically
    // under the compare-and-set lock, recording nothing.
    let request = ReplaceSessionDefaultsRequest::try_new_with_model_settings_adjustments(
        durable_command_id,
        session,
        expected_version,
        replacement,
        caller_model_settings,
        model_settings_adjustments.into_vec(),
        prompt_member,
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
    let mut service = ReplaceSessionDefaultsService::new(repository);
    match service.execute(request).await {
        Ok(outcome) => {
            write_replace_session_defaults_outcome(writer, version, request_id, session_id, outcome)
                .await
        }
        Err(ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous, ..
        }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(commit_ambiguous),
            )
            .await
        }
        Err(
            error @ (ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
            | ReplaceSessionDefaultsRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = session_defaults_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
            )
            .await
        }
    }
}

pub(super) async fn handle_defaults_rejection_only(
    repository: &ReplaceSessionDefaultsRepository,
    command: DomainReplaceSessionDefaults,
    prompt_member: PromptMemberStatement,
) -> Result<Option<ReplaceSessionDefaultsOutcome>, ReplaceSessionDefaultsRepositoryError> {
    let outcome = repository
        .handle_rejection_only_where_prompt_member(command, prompt_member)
        .await?;
    Ok(match outcome {
        ReplaceSessionDefaultsRejectionOnlyOutcome::CurrentVersionMatched => None,
        ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome) => Some(match outcome {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id } => {
                ReplaceSessionDefaultsOutcome::ConflictingReuse { command_id }
            }
            ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember => {
                ReplaceSessionDefaultsOutcome::PromptRequiresStatedMember
            }
        }),
    })
}

pub(super) async fn write_replace_session_defaults_outcome<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    outcome: ReplaceSessionDefaultsOutcome,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match outcome {
        ReplaceSessionDefaultsOutcome::Recorded(result) => {
            write_replace_session_defaults_result(writer, version, request_id, session_id, &result)
                .await
        }
        // Frame validation rejects an absent system-prompt member, so this
        // repository outcome cannot be client-triggered.
        ReplaceSessionDefaultsOutcome::PromptRequiresStatedMember => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SystemPromptMemberMissing,
                ),
            )
            .await
        }
        ReplaceSessionDefaultsOutcome::ConflictingReuse { .. } => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
    }
}

pub(super) async fn write_replace_session_defaults_result<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    result: &ReplaceSessionDefaultsResult,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match result {
        ReplaceSessionDefaultsResult::Applied(applied) => {
            let installed = applied.installed();
            let system_prompt = SystemPromptMember::present(
                wire_system_prompt(installed.defaults().system_prompt())
                    .ok_or(ProcessConnectionError::EncodeInvariant)?,
            );
            write_mutation_receipt_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaultsReplaced {
                    session_id,
                    defaults_version: CanonicalU64::new(installed.version().as_u64()),
                    model_selection: wire_domain_model_selection(installed.defaults().model()),
                    model_settings: wire_model_settings(installed.defaults().model_settings()),
                    dangerous_tool_auto_approval: matches!(
                        installed.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        ReplaceSessionDefaultsResult::Rejected(rejected) => {
            let detail = match rejected {
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(rejected) => {
                    RejectionDetail::SessionNotFound {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(rejected) => {
                    RejectionDetail::DefaultsVersionMismatch {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        expected: CanonicalU64::new(rejected.expected().as_u64()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(rejected) => {
                    RejectionDetail::DefaultsVersionExhausted {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
            };
            write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
        }
    }
}

pub(super) async fn write_session_metadata_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<CanonicalUuid>,
    error: SessionMetadataRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        SessionMetadataRepositoryError::Database(_)
        | SessionMetadataRepositoryError::CommitAmbiguous(_) => {
            ProtocolError::without_detail(ErrorCode::Unavailable)
        }
        SessionMetadataRepositoryError::DifferentCommandKind { .. } => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::SessionMetadataCommandKindMismatch,
        ),
        SessionMetadataRepositoryError::Corruption(_) => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::SessionMetadataCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}
