use super::*;

pub(super) async fn handle_compact_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: signalbox_process_protocol::CommandId,
    session_id: CanonicalUuid,
    through_position: Option<CanonicalU64>,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let command = DurableCommandId::from_uuid(command_id.into_uuid());
    let session = SessionId::from_uuid(session_id.into_uuid());
    let requested_through_position = through_position.map(CanonicalU64::value);
    let repository = ContextCompactionRepository::new(services.pool.clone());
    match repository
        .lookup_command(command, session, requested_through_position)
        .await
    {
        Ok(ContextCompactionCommandLookup::Unseen) => {}
        Ok(ContextCompactionCommandLookup::Replayed(applied)) => {
            return write_context_compaction_receipt(
                writer, version, request_id, session_id, applied,
            )
            .await;
        }
        Ok(ContextCompactionCommandLookup::ConflictingReuse) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(ContextCompactionCommandLookup::Pending | ContextCompactionCommandLookup::Failed) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(error) => {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                error,
            )
            .await;
        }
    }
    let defaults = match ProcessReadRepository::new(services.pool.clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(defaults)) => defaults,
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session.into_uuid()),
                    InternalDiagnostic::SessionDefaultsVersionMissing,
                ),
            )
            .await;
        }
        Err(error) => {
            return write_context_compaction_read_error(
                writer, version, request_id, session, error,
            )
            .await;
        }
    };
    let selection = match defaults.defaults().model() {
        ModelSelectionRequest::Direct(selection) => selection,
        ModelSelectionRequest::Alias(alias) => {
            let Some(definition) = services.model_configuration.resolve_alias(alias) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            };
            definition.selected()
        }
    };
    let Some(route) = services.model_configuration.resolve_direct_model(selection) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    let input_includes_cache_tokens = route.adapter().reports_cache_inclusive_input();
    let credential_reference =
        match signalbox_persistence::session_credentials::current_session_credential_with_migration_fallback(
            &services.pool,
            session,
            route.model_family(),
            route.migration_credential_family(),
        )
        .await
        {
            Ok(reference) => reference.as_str().to_owned(),
            Err(sqlx::Error::RowNotFound) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    internal_protocol_error(
                        Some(session.into_uuid()),
                        InternalDiagnostic::SessionModelCredentialMissing,
                    ),
                )
                .await;
            }
            Err(_) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            }
        };
    let target = match services
        .model_configuration
        .target_catalog()
        .resolve(FrozenModelSelection::Direct(selection))
    {
        Ok(resolved) => resolved.target(),
        Err(_) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
    };
    let prepared = loop {
        let request = PrepareContextCompactionRequest {
            command,
            session,
            requested_through_position,
            automatic_for_turn: None,
            defaults_version: defaults.version(),
            selection,
            target,
            input_includes_cache_tokens,
            credential_reference: credential_reference.clone(),
            call: ModelCallId::from_uuid(uuid::Uuid::now_v7()),
            compaction: ContextCompactionId::from_uuid(uuid::Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        };
        match repository.prepare(request).await {
            Ok(PrepareContextCompactionOutcome::Prepared(prepared)) => break prepared,
            Ok(PrepareContextCompactionOutcome::Replayed(applied)) => {
                return write_context_compaction_receipt(
                    writer, version, request_id, session_id, applied,
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::ConflictingReuse) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::ConflictingReuse),
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::SessionNotFound) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::NotFound),
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::InvalidBoundary) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
            Ok(
                PrepareContextCompactionOutcome::DefaultsChanged
                | PrepareContextCompactionOutcome::Busy
                | PrepareContextCompactionOutcome::NoBoundary
                | PrepareContextCompactionOutcome::AutomaticAlreadyAttempted
                | PrepareContextCompactionOutcome::FailedReplay,
            ) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            }
            Err(ContextCompactionRepositoryError::IdentityCollision) => continue,
            Err(error) => {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    session,
                    services.recovery_reporter.as_ref(),
                    error,
                )
                .await;
            }
        }
    };
    let rendered_range = match load_context_compaction_range(&services.pool, &prepared).await {
        Ok(rendered) => rendered,
        Err(error) => {
            return fail_context_compaction_before_response(
                writer,
                version,
                request_id,
                services.recovery_reporter.as_ref(),
                &repository,
                &prepared,
                error,
            )
            .await;
        }
    };
    if let Err(error) = authorize_context_compaction_until_resolved(&repository, &prepared).await {
        return write_context_compaction_repository_error(
            writer,
            version,
            request_id,
            session,
            services.recovery_reporter.as_ref(),
            error,
        )
        .await;
    }
    let request = ContextCompactionModelRequest {
        call: prepared.call(),
        session,
        selection: prepared.selection(),
        target: prepared.target(),
        credential_reference: prepared.credential_reference().to_owned(),
        system_prompt: services.model_configuration.compaction_prompt().to_owned(),
        rendered_range,
    };
    let result = match services.context_compaction_model.execute(request).await {
        Ok(result) => result,
        Err(error) => {
            let disposition = context_compaction_failure_disposition(error);
            if let Err(repository_error) =
                fail_context_compaction_until_resolved(&repository, &prepared, disposition).await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    session,
                    services.recovery_reporter.as_ref(),
                    repository_error,
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
    };
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(result.usage.input_tokens)
        .with_output_tokens(result.usage.output_tokens)
        .with_cache_creation_input_tokens(result.usage.cache_creation_input_tokens)
        .with_cache_read_input_tokens(result.usage.cache_read_input_tokens);
    let exceeds_limits = context_compaction_usage_exceeds_configured_limits(
        &services.model_configuration,
        prepared.target(),
        result.usage,
    )
    .unwrap_or_else(|| {
        record_internal_diagnostic(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionUnconfiguredTarget,
        );
        true
    });
    if exceeds_limits {
        if let Err(repository_error) = fail_context_compaction_with_usage_until_resolved(
            &repository,
            &prepared,
            FailedContextCompactionDisposition::KnownFailed,
            usage,
        )
        .await
        {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                repository_error,
            )
            .await;
        }
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    }
    let applied = match complete_context_compaction_until_resolved(
        &repository,
        &prepared,
        &result.summary,
        usage,
    )
    .await
    {
        Ok(applied) => applied,
        Err(error) => {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                error,
            )
            .await;
        }
    };
    write_context_compaction_receipt(writer, version, request_id, session_id, applied).await
}

#[derive(Debug)]
pub(crate) enum AutomaticContextCompactionError {
    Read(ProcessReadError),
    Credential(ModelCallRepositoryError),
    Repository(ContextCompactionRepositoryError),
    Model,
    Configuration,
    InputDoesNotFit,
    State,
    Integrity,
    AlreadyAttempted,
}

impl fmt::Display for AutomaticContextCompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("automatic context compaction failed")
    }
}

impl Error for AutomaticContextCompactionError {}

impl ClassifyOperatorFailure for AutomaticContextCompactionError {
    fn operator_failure_class(&self) -> signalbox_application::OperatorFailureClass {
        match self {
            Self::Credential(error) => error.operator_failure_class(),
            Self::Repository(error) => error.operator_failure_class(),
            Self::Read(ProcessReadError::Database(_)) | Self::Model => {
                signalbox_application::OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Read(ProcessReadError::Corruption(_)) | Self::Integrity => {
                signalbox_application::OperatorFailureClass::FailClosedCorruption
            }
            Self::Configuration | Self::InputDoesNotFit | Self::State | Self::AlreadyAttempted => {
                signalbox_application::OperatorFailureClass::CallerOrHubBug
            }
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Credential(error) => error.operator_failure_cause_code(),
            Self::Read(ProcessReadError::Database(_)) => "context_compaction_read_database",
            Self::Read(ProcessReadError::Corruption(_)) => "context_compaction_read_corruption",
            Self::Repository(ContextCompactionRepositoryError::Database(_)) => {
                "context_compaction_repository_database"
            }
            Self::Repository(ContextCompactionRepositoryError::CommitAmbiguous(_)) => {
                "context_compaction_repository_commit_ambiguous"
            }
            Self::Repository(ContextCompactionRepositoryError::IdentityCollision) => {
                "context_compaction_repository_identity_collision"
            }
            Self::Repository(ContextCompactionRepositoryError::Corruption(_)) => {
                "context_compaction_repository_corruption"
            }
            Self::Model => "context_compaction_model",
            Self::Configuration => "context_compaction_configuration",
            Self::InputDoesNotFit => "context_compaction_input_does_not_fit",
            Self::State => "context_compaction_state",
            Self::Integrity => "context_compaction_integrity",
            Self::AlreadyAttempted => "context_compaction_already_attempted",
        }
    }
}

pub(super) async fn automatic_context_compaction_boundary(
    members: &[AutomaticContextCompactionPreviewMember],
    entries: &[ProcessTranscriptEntry],
    input_byte_budget: u64,
    catalog: &BlobCatalogRepository,
) -> Result<Option<u64>, AutomaticContextCompactionError> {
    if members.len() != entries.len() {
        return Err(AutomaticContextCompactionError::Integrity);
    }
    let mut encoded_lengths = Vec::with_capacity(entries.len());
    let mut boundaries = Vec::with_capacity(entries.len());
    for (member, entry) in members.iter().zip(entries) {
        if member.reference() != transcript_entry_reference(entry) {
            return Err(AutomaticContextCompactionError::Integrity);
        }
        // Attachment parts resolve through the blob catalog, so rendering one
        // entry is a database read that carries the same closed dispositions
        // the compaction range load already maps.
        let value = context_compaction_entry_value(entry, catalog)
            .await
            .map_err(|error| match error {
                ContextCompactionRangeLoadError::Read(error) => {
                    AutomaticContextCompactionError::Read(error)
                }
                ContextCompactionRangeLoadError::CatalogUnavailable => {
                    AutomaticContextCompactionError::Model
                }
                ContextCompactionRangeLoadError::Integrity => {
                    AutomaticContextCompactionError::Integrity
                }
            })?;
        let encoded =
            serde_json::to_vec(&value).map_err(|_| AutomaticContextCompactionError::Integrity)?;
        encoded_lengths.push(
            u64::try_from(encoded.len()).map_err(|_| AutomaticContextCompactionError::Integrity)?,
        );
        boundaries.push((member.position(), member.is_safe_boundary()));
    }
    let selected =
        bounded_rendered_compaction_boundary(&encoded_lengths, &boundaries, input_byte_budget);
    let selected_only_current_summary = selected
        == boundaries.first().map(|(position, _)| *position)
        && matches!(
            entries.first(),
            Some(ProcessTranscriptEntry::ContextSummary { .. })
        );
    if selected_only_current_summary
        && successor_compaction_cannot_advance(&encoded_lengths, &boundaries, input_byte_budget)
    {
        return Ok(None);
    }
    Ok(selected)
}

pub(super) fn successor_compaction_cannot_advance(
    encoded_lengths: &[u64],
    boundaries: &[(u64, bool)],
    input_byte_budget: u64,
) -> bool {
    if encoded_lengths.len() != boundaries.len() || encoded_lengths.len() < 2 {
        return true;
    }
    let mut minimum_bytes = 2_u64;
    for (encoded_length, (_, safe_boundary)) in encoded_lengths[1..].iter().zip(&boundaries[1..]) {
        minimum_bytes = minimum_bytes
            .saturating_add(1)
            .saturating_add(*encoded_length);
        if *safe_boundary {
            return minimum_bytes > input_byte_budget;
        }
    }
    true
}

pub(super) fn bounded_rendered_compaction_boundary(
    encoded_lengths: &[u64],
    boundaries: &[(u64, bool)],
    input_byte_budget: u64,
) -> Option<u64> {
    if encoded_lengths.len() != boundaries.len() {
        return None;
    }
    let separators = u64::try_from(encoded_lengths.len().saturating_sub(1)).ok()?;
    let total_bytes = encoded_lengths
        .iter()
        .fold(2_u64, |total, length| total.saturating_add(*length));
    let target_bytes = total_bytes
        .saturating_add(separators)
        .div_ceil(2)
        .min(input_byte_budget);
    let mut prefix_bytes = 1_u64;
    let mut latest_safe = None;
    for (index, ((position, safe_boundary), encoded_length)) in
        boundaries.iter().zip(encoded_lengths).enumerate()
    {
        if index > 0 {
            prefix_bytes = prefix_bytes.saturating_add(1);
        }
        prefix_bytes = prefix_bytes.saturating_add(*encoded_length);
        let candidate_bytes = prefix_bytes.saturating_add(1);
        if candidate_bytes > input_byte_budget {
            break;
        }
        if *safe_boundary {
            latest_safe = Some(*position);
            if candidate_bytes >= target_bytes {
                return latest_safe;
            }
        }
    }
    latest_safe
}

pub(crate) async fn compact_automatically(
    model_calls: &PostgresModelCallRepository,
    model_configuration: &HubModelConfiguration,
    model: &Arc<dyn ContextCompactionModel>,
    session: SessionId,
    turn: TurnId,
    observe_prepared: Option<&(dyn Fn(ModelCallId) + Send + Sync)>,
) -> Result<AppliedContextCompaction, AutomaticContextCompactionError> {
    let defaults = match ProcessReadRepository::new(model_calls.pool().clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(defaults)) => defaults,
        Ok(ProcessSessionDefaultsRead::SessionNotFound)
        | Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            return Err(AutomaticContextCompactionError::State);
        }
        Err(error) => return Err(AutomaticContextCompactionError::Read(error)),
    };
    let selection = match defaults.defaults().model() {
        ModelSelectionRequest::Direct(selection) => selection,
        ModelSelectionRequest::Alias(alias) => model_configuration
            .resolve_alias(alias)
            .ok_or(AutomaticContextCompactionError::Configuration)?
            .selected(),
    };
    let target = model_configuration
        .target_catalog()
        .resolve(FrozenModelSelection::Direct(selection))
        .map_err(|_| AutomaticContextCompactionError::Configuration)?
        .target();
    let route = model_configuration
        .resolve_direct_model(selection)
        .ok_or(AutomaticContextCompactionError::Configuration)?;
    let input_includes_cache_tokens = route.adapter().reports_cache_inclusive_input();
    let runtime_models = model_configuration.runtime_model_catalog();
    let definition = runtime_models
        .resolve(target)
        .ok_or(AutomaticContextCompactionError::Configuration)?;
    let compaction_prompt = model_configuration.compaction_prompt();
    let prompt_bytes = u64::try_from(compaction_prompt.len())
        .map_err(|_| AutomaticContextCompactionError::Configuration)?;
    let automatic_input_byte_budget = u64::from(definition.context_window_tokens())
        .checked_sub(u64::from(definition.max_output_tokens()))
        .and_then(|available| available.checked_sub(prompt_bytes))
        .filter(|available| *available > 0)
        .ok_or(AutomaticContextCompactionError::Configuration)?;
    let credential_reference = model_calls
        .resolve_session_credential_reference(session, target)
        .await
        .map_err(AutomaticContextCompactionError::Credential)?;
    let repository = ContextCompactionRepository::new(model_calls.pool().clone());
    let preview = repository
        .preview_automatic_range(session)
        .await
        .map_err(AutomaticContextCompactionError::Repository)?
        .ok_or(AutomaticContextCompactionError::State)?;
    let preview_positions = preview
        .members()
        .iter()
        .map(|member| member.position())
        .collect::<Vec<_>>();
    let preview_entries = preview
        .members()
        .iter()
        .map(|member| member.reference())
        .collect::<Vec<_>>();
    let rendered_entries = ProcessReadRepository::new(model_calls.pool().clone())
        .read_selected_transcript_entries(&preview_positions, &preview_entries)
        .await
        .map_err(AutomaticContextCompactionError::Read)?;
    let requested_through_position = automatic_context_compaction_boundary(
        preview.members(),
        &rendered_entries,
        automatic_input_byte_budget,
        &BlobCatalogRepository::new(model_calls.pool().clone()),
    )
    .await?
    .ok_or(AutomaticContextCompactionError::InputDoesNotFit)?;
    let prepared = loop {
        let call = ModelCallId::from_uuid(uuid::Uuid::now_v7());
        let request = PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            session,
            requested_through_position: Some(requested_through_position),
            automatic_for_turn: Some(turn),
            defaults_version: defaults.version(),
            selection,
            target,
            input_includes_cache_tokens,
            credential_reference: credential_reference.as_str().to_owned(),
            call,
            compaction: ContextCompactionId::from_uuid(uuid::Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        };
        // Named before the await, not after it. A scheduler pass drops this
        // future the moment its occupancy bound expires, and that drop can land
        // while this prepare is waiting for its commit to be acknowledged — the
        // row durable, the answer never delivered. Reporting the identity only
        // on the way out would leave that compaction unnamed, so expiry would
        // read the window as still inside its read-only preflight and hand over
        // no recovery at all: exactly the wedge this handoff exists to close.
        // Early reporting is safe in the other direction, because recovery
        // names this exact call and a prepare that never became durable is
        // simply found absent. A collision retry is rejected before it commits,
        // so overwriting with the next attempt's identity cannot orphan a row.
        if let Some(observe_prepared) = observe_prepared {
            observe_prepared(call);
        }
        match repository.prepare(request).await {
            Ok(PrepareContextCompactionOutcome::Prepared(prepared)) => break prepared,
            Ok(
                PrepareContextCompactionOutcome::Replayed(_)
                | PrepareContextCompactionOutcome::ConflictingReuse
                | PrepareContextCompactionOutcome::SessionNotFound
                | PrepareContextCompactionOutcome::DefaultsChanged
                | PrepareContextCompactionOutcome::Busy
                | PrepareContextCompactionOutcome::NoBoundary
                | PrepareContextCompactionOutcome::InvalidBoundary
                | PrepareContextCompactionOutcome::FailedReplay,
            ) => {
                return Err(AutomaticContextCompactionError::State);
            }
            Ok(PrepareContextCompactionOutcome::AutomaticAlreadyAttempted) => {
                return Err(AutomaticContextCompactionError::AlreadyAttempted);
            }
            Err(ContextCompactionRepositoryError::IdentityCollision) => continue,
            Err(error) => return Err(AutomaticContextCompactionError::Repository(error)),
        }
    };
    let rendered_range = match retry_context_compaction_range_database_reads(|| {
        load_context_compaction_range(model_calls.pool(), &prepared)
    })
    .await
    {
        Ok(rendered) => rendered,
        Err(ContextCompactionRangeLoadError::Read(error)) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Read(error));
        }
        Err(ContextCompactionRangeLoadError::CatalogUnavailable) => {
            return Err(AutomaticContextCompactionError::Model);
        }
        Err(ContextCompactionRangeLoadError::Integrity) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Integrity);
        }
    };
    if u64::try_from(rendered_range.len())
        .ok()
        .is_none_or(|rendered_bytes| rendered_bytes > automatic_input_byte_budget)
    {
        fail_context_compaction_until_resolved(
            &repository,
            &prepared,
            FailedContextCompactionDisposition::KnownFailed,
        )
        .await
        .map_err(AutomaticContextCompactionError::Repository)?;
        return Err(AutomaticContextCompactionError::InputDoesNotFit);
    }
    authorize_context_compaction_until_resolved(&repository, &prepared)
        .await
        .map_err(AutomaticContextCompactionError::Repository)?;
    let request = ContextCompactionModelRequest {
        call: prepared.call(),
        session,
        selection: prepared.selection(),
        target: prepared.target(),
        credential_reference: prepared.credential_reference().to_owned(),
        system_prompt: compaction_prompt.to_owned(),
        rendered_range,
    };
    let result = match model.execute(request).await {
        Ok(result) => result,
        Err(error) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                context_compaction_failure_disposition(error),
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Model);
        }
    };
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(result.usage.input_tokens)
        .with_output_tokens(result.usage.output_tokens)
        .with_cache_creation_input_tokens(result.usage.cache_creation_input_tokens)
        .with_cache_read_input_tokens(result.usage.cache_read_input_tokens);
    complete_context_compaction_until_resolved(&repository, &prepared, &result.summary, usage)
        .await
        .map_err(AutomaticContextCompactionError::Repository)
}

pub(super) async fn load_context_compaction_range(
    pool: &PgPool,
    prepared: &PreparedContextCompaction,
) -> Result<String, ContextCompactionRangeLoadError> {
    let entries = ProcessReadRepository::new(pool.clone())
        .read_selected_transcript_entries(
            prepared.summarized_positions(),
            prepared.summarized_entries(),
        )
        .await?;
    let Some(first) = entries.first() else {
        return Err(ContextCompactionRangeLoadError::Integrity);
    };
    let Some(through) = entries.last() else {
        return Err(ContextCompactionRangeLoadError::Integrity);
    };
    if transcript_entry_reference(first) != prepared.first()
        || transcript_entry_reference(through) != prepared.through()
    {
        return Err(ContextCompactionRangeLoadError::Integrity);
    }
    let catalog = BlobCatalogRepository::new(pool.clone());
    let mut values = Vec::with_capacity(entries.len());
    for entry in &entries {
        values.push(context_compaction_entry_value(entry, &catalog).await?);
    }
    serde_json::to_string(&values).map_err(|_| ContextCompactionRangeLoadError::Integrity)
}

pub(super) async fn retry_context_compaction_range_database_reads<Load, LoadFuture>(
    mut load: Load,
) -> Result<String, ContextCompactionRangeLoadError>
where
    Load: FnMut() -> LoadFuture,
    LoadFuture: Future<Output = Result<String, ContextCompactionRangeLoadError>>,
{
    loop {
        match load().await {
            Err(ContextCompactionRangeLoadError::Read(ProcessReadError::Database(_))) => {
                sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await;
            }
            Err(ContextCompactionRangeLoadError::CatalogUnavailable) => {
                sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await;
            }
            result => return result,
        }
    }
}

pub(super) fn transcript_entry_reference(
    entry: &ProcessTranscriptEntry,
) -> signalbox_domain::SemanticTranscriptEntryRef {
    let (source_session, entry) = match entry {
        ProcessTranscriptEntry::DelegatedTask {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::DelegationMessage {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::DelegationResult {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ModelIdentityChanged {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ContextSummary {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::User {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::Assistant {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::AssistantToolUse {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolExecutionResult {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolDenied {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolClosed {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnFailed {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnCompleted {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnCancelled {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ImportedText {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::Imported {
            source_session,
            entry,
            ..
        } => (*source_session, *entry),
    };
    signalbox_domain::SemanticTranscriptEntryRef::from_source(source_session, entry)
}

pub(super) async fn context_compaction_entry_value(
    entry: &ProcessTranscriptEntry,
    catalog: &BlobCatalogRepository,
) -> Result<serde_json::Value, ContextCompactionRangeLoadError> {
    let reference = transcript_entry_reference(entry);
    let source_session_id = reference
        .source_session()
        .into_uuid()
        .hyphenated()
        .to_string();
    let entry_id = reference.entry().into_uuid().hyphenated().to_string();
    let value = match entry {
        ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            spawning_request,
            parent_session,
            parent_turn,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegated_task",
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "parent_session_id": parent_session.into_uuid().hyphenated().to_string(),
            "parent_turn_id": parent_turn.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            spawning_request,
            message,
            sender,
            recipient,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegation_message",
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "message_id": message.into_uuid().hyphenated().to_string(),
            "sender_session_id": sender.into_uuid().hyphenated().to_string(),
            "recipient_session_id": recipient.into_uuid().hyphenated().to_string(),
            "ordinal": ordinal,
            "delivery_sequence": delivery_sequence,
            "content": content,
        }),
        ProcessTranscriptEntry::DelegationResult {
            entry_index,
            awaiting_request,
            spawning_request,
            child,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegation_result",
            "await_request_id": awaiting_request.into_uuid().hyphenated().to_string(),
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "child_session_id": child.into_uuid().hyphenated().to_string(),
            "mode": match mode {
                DispatchedDelegationWaitMode::Foreground => WireDelegationWaitMode::Foreground,
                DispatchedDelegationWaitMode::Background => WireDelegationWaitMode::Background,
            },
            "delivery_sequence": delivery_sequence,
            "outcome": wire_delegation_outcome(*outcome),
            "content": content,
            "reason": wire_delegation_reason(*reason),
            "provenance": wire_delegation_provenance(*provenance),
        }),
        ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            turn,
            defaults_version,
            selected,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "model_identity_changed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "defaults_version": defaults_version,
            "selected_model_id": selected.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::ContextSummary {
            entry_index,
            model_call,
            first,
            through,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "context_summary",
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "first_source_session_id": first.source_session().into_uuid().hyphenated().to_string(),
            "first_entry_id": first.entry().into_uuid().hyphenated().to_string(),
            "through_source_session_id": through.source_session().into_uuid().hyphenated().to_string(),
            "through_entry_id": through.entry().into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::User {
            entry_index,
            accepted_input,
            turn,
            content,
            ..
        } => {
            let mut lengths = std::collections::BTreeMap::new();
            for part in content.parts() {
                let signalbox_domain::UserContentPart::Attachment { digest, .. } = part else {
                    continue;
                };
                if lengths.contains_key(digest) {
                    continue;
                }
                let catalog_entry = catalog
                    .find(*digest)
                    .await
                    .map_err(map_context_compaction_catalog_error)?
                    .ok_or(ContextCompactionRangeLoadError::Integrity)?;
                let length = NonZeroU64::new(catalog_entry.expected().byte_length())
                    .ok_or(ContextCompactionRangeLoadError::Integrity)?;
                lengths.insert(*digest, length);
            }
            let rendered =
                render_model_user_content(content.clone(), |digest| lengths.get(&digest).copied())
                    .map_err(|_| ContextCompactionRangeLoadError::Integrity)?;
            let rendered_parts = rendered
                .parts()
                .iter()
                .map(|part| part.as_str())
                .collect::<Vec<_>>();
            serde_json::json!({
                "position": entry_index + 1,
                "source_session_id": source_session_id,
                "entry_id": entry_id,
                "type": "user",
                "accepted_input_id": accepted_input.into_uuid().hyphenated().to_string(),
                "turn_id": turn.into_uuid().hyphenated().to_string(),
                "content": rendered_parts,
            })
        }
        ProcessTranscriptEntry::Assistant {
            entry_index,
            turn,
            model_call,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "assistant",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            turn,
            model_call,
            request,
            name,
            arguments,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "assistant_tool_use",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "name": name,
            "arguments": arguments,
        }),
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            request,
            attempt,
            disposition: _,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_execution_result",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "tool_attempt_id": attempt.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::ToolDenied {
            entry_index,
            request,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_denied",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::ToolClosed {
            entry_index,
            request,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_closed_by_turn_end",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::TurnFailed {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_failed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::TurnCompleted {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_completed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::TurnCancelled {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_cancelled",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "imported_text",
            "imported_conversation_id": imported_conversation.into_uuid().hyphenated().to_string(),
            "imported_entry_id": imported_entry.into_uuid().hyphenated().to_string(),
            "source_speaker": imported_source_speaker_label(*source_speaker),
            "content": content,
        }),
        ProcessTranscriptEntry::Imported {
            entry_index,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "imported",
            "imported_conversation_id": imported_conversation.into_uuid().hyphenated().to_string(),
            "imported_entry_id": imported_entry.into_uuid().hyphenated().to_string(),
            "source_speaker": imported_source_speaker_label(*source_speaker),
            "content_kind": imported_content_kind_label(*content_kind),
        }),
    };
    Ok(value)
}

pub(super) fn map_context_compaction_catalog_error(
    error: signalbox_persistence::blob::BlobCatalogRepositoryError,
) -> ContextCompactionRangeLoadError {
    match error {
        signalbox_persistence::blob::BlobCatalogRepositoryError::Database(_)
        | signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(_) => {
            ContextCompactionRangeLoadError::CatalogUnavailable
        }
        signalbox_persistence::blob::BlobCatalogRepositoryError::Corruption(_) => {
            ContextCompactionRangeLoadError::Integrity
        }
    }
}

pub(super) const fn imported_source_speaker_label(
    speaker: ProcessImportedSourceSpeaker,
) -> &'static str {
    match speaker {
        ProcessImportedSourceSpeaker::NotAttested => "not_attested",
        ProcessImportedSourceSpeaker::AttestedAbsent => "attested_absent",
        ProcessImportedSourceSpeaker::User => "user",
        ProcessImportedSourceSpeaker::Assistant => "assistant",
    }
}

pub(super) const fn imported_content_kind_label(kind: ProcessImportedContentKind) -> &'static str {
    match kind {
        ProcessImportedContentKind::SourceEvent => "source_event",
        ProcessImportedContentKind::SourceMessageBlock => "source_message_block",
        ProcessImportedContentKind::Text => "text",
        ProcessImportedContentKind::ToolCall => "tool_call",
        ProcessImportedContentKind::ToolResult => "tool_result",
        ProcessImportedContentKind::Thinking => "thinking",
        ProcessImportedContentKind::RedactedThinking => "redacted_thinking",
        ProcessImportedContentKind::Document => "document",
        ProcessImportedContentKind::MessageContentAbsent => "message_content_absent",
    }
}

pub(super) async fn authorize_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository.authorize(prepared).await {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

/// Applies the completion, retrying only the outcomes an identical retry can
/// still change.
///
/// A transient database failure may succeed next time, and an unproven commit
/// is resolved by `complete` rereading its own terminal facts under the session
/// lock and returning the applied result. Every other class is a decided fact —
/// including a uniqueness violation on a result identity, which repeating the
/// same statements can never clear — so it returns rather than blocking the
/// session forever.
pub(super) async fn complete_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    summary: &str,
    usage: ContextCompactionTokenUsage,
) -> Result<AppliedContextCompaction, ContextCompactionRepositoryError> {
    loop {
        match repository.complete(prepared, summary, usage).await {
            Ok(applied) => return Ok(applied),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn fail_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    disposition: FailedContextCompactionDisposition,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository.fail(prepared, disposition).await {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn fail_context_compaction_with_usage_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    disposition: FailedContextCompactionDisposition,
    usage: ContextCompactionTokenUsage,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository
            .fail_with_usage(prepared, disposition, usage)
            .await
        {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

pub(super) const fn context_compaction_failure_disposition(
    error: ContextCompactionModelError,
) -> FailedContextCompactionDisposition {
    match error {
        ContextCompactionModelError::CancelledBeforeSend
        | ContextCompactionModelError::CancellationConfirmed => {
            FailedContextCompactionDisposition::Cancelled
        }
        ContextCompactionModelError::BoundaryLoss
        | ContextCompactionModelError::CorrelationMismatch => {
            FailedContextCompactionDisposition::Ambiguous
        }
        ContextCompactionModelError::Refused => FailedContextCompactionDisposition::Refused,
        ContextCompactionModelError::UnconfiguredTarget
        | ContextCompactionModelError::PreparationFailed
        | ContextCompactionModelError::PreparationDefect
        | ContextCompactionModelError::ProviderError
        | ContextCompactionModelError::ProvenUnsent
        | ContextCompactionModelError::ProviderTargetSubstituted
        | ContextCompactionModelError::IncompleteSummary
        | ContextCompactionModelError::NonTextSummary
        | ContextCompactionModelError::InvalidSummary => {
            FailedContextCompactionDisposition::KnownFailed
        }
    }
}

pub(super) async fn fail_context_compaction_before_response<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    recovery_reporter: Option<&FatalRecoveryReporter>,
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    error: ContextCompactionRangeLoadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        ContextCompactionRangeLoadError::CatalogUnavailable => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await
        }
        ContextCompactionRangeLoadError::Read(error) => {
            if let Err(repository_error) = fail_context_compaction_until_resolved(
                repository,
                prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    prepared.session(),
                    recovery_reporter,
                    repository_error,
                )
                .await;
            }
            write_context_compaction_read_error(
                writer,
                version,
                request_id,
                prepared.session(),
                error,
            )
            .await
        }
        ContextCompactionRangeLoadError::Integrity => {
            if let Err(repository_error) = fail_context_compaction_until_resolved(
                repository,
                prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    prepared.session(),
                    recovery_reporter,
                    repository_error,
                )
                .await;
            }
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(prepared.session().into_uuid()),
                    InternalDiagnostic::ContextCompactionRangeCorruption,
                ),
            )
            .await
        }
    }
}

pub(super) async fn write_context_compaction_receipt<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    applied: AppliedContextCompaction,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_mutation_receipt_via_spool(
        writer,
        version,
        request_id,
        ServerMessage::SessionCompacted {
            session_id,
            context_compaction_id: wire_uuid(applied.compaction.into_uuid()),
            model_call_id: wire_uuid(applied.call.into_uuid()),
            through_position: CanonicalU64::new(applied.through_position),
            summary_entry_id: wire_uuid(applied.summary_entry.into_uuid()),
            result_frontier_id: wire_uuid(applied.result_frontier.into_uuid()),
        },
    )
    .await
}

/// Answers one explicit compaction repository failure, reporting first when it
/// left a durable outcome this process cannot decide.
///
/// The automatic sibling reaches the same signal through the scheduler pass's
/// execution role. A connection handler has none, and it cannot terminalize the
/// record either: `prepare` returned no `PreparedContextCompaction`, so `fail`
/// has nothing to name, replay of the same command finds it `Pending`, and a
/// fresh command finds the nonterminal call. Startup recovery does reconcile
/// exactly this state — `active_sessions` includes sessions holding a
/// nonterminal compaction call — but only the next incarnation runs it, so
/// without this report the session's compaction boundary stays owned by a call
/// nothing terminalizes for the life of the process, with nothing telling an
/// operator to restart.
pub(super) async fn write_context_compaction_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session: SessionId,
    recovery_reporter: Option<&FatalRecoveryReporter>,
    error: ContextCompactionRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if crate::commit_outcome_is_unknown(&error)
        && let Some(reporter) = recovery_reporter
    {
        reporter.report_recovery_required();
    }
    let response = match error {
        ContextCompactionRepositoryError::Database(_) => ProtocolError::mutation_unavailable(false),
        ContextCompactionRepositoryError::CommitAmbiguous(_) => {
            ProtocolError::mutation_unavailable(true)
        }
        ContextCompactionRepositoryError::IdentityCollision => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionIdentityCollision,
        ),
        ContextCompactionRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionRepositoryCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

pub(super) async fn write_context_compaction_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session: SessionId,
    error: ProcessReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ProcessReadError::Database(_) => ProtocolError::mutation_unavailable(false),
        ProcessReadError::Corruption(_) => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionReadCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

#[derive(Debug)]
pub(super) enum ContextCompactionRangeLoadError {
    Read(ProcessReadError),
    CatalogUnavailable,
    Integrity,
}

impl From<ProcessReadError> for ContextCompactionRangeLoadError {
    fn from(error: ProcessReadError) -> Self {
        Self::Read(error)
    }
}
