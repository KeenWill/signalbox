use super::*;

pub(crate) async fn read_system_prompt_file(path: &Path) -> Result<SystemPromptText, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::system_prompt_file)?;
    let read_limit = u64::try_from(MAX_SYSTEM_PROMPT_FRAME_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("system prompt read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(ClientError::system_prompt_file)?;
    if bytes.is_empty() {
        return Err(ClientError::Input(
            "the system prompt file must not be empty",
        ));
    }
    if bytes.len() > MAX_SYSTEM_PROMPT_FRAME_BYTES {
        return Err(ClientError::Input(
            "the system prompt exceeds the wire-frame byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("the system prompt must be valid UTF-8"))?;
    SystemPromptText::try_new(text)
        .map_err(|_| ClientError::Input("the system prompt must not contain U+0000"))
}

pub(crate) async fn create(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    selection: ModelSelection,
    command_id: Option<CommandId>,
    system_prompt: Option<SystemPromptText>,
    placement: SessionPlacement,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSession {
            command_id,
            initial_model_selection: selection,
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(system_prompt),
            placement,
            lifecycle: SessionLifecycleMembers::default(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } if model_settings.matches_model(&selection) => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("create returned an unexpected response").mutation()),
    }
}

pub(crate) async fn create_from_template(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    template_name: String,
    command_id: Option<CommandId>,
    placement: SessionPlacement,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSessionFromTemplate {
            command_id,
            template_name,
            placement,
            lifecycle: SessionLifecycleMembers::default(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id, .. } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("template creation returned an unexpected response").mutation(),
        ),
    }
}

pub(crate) async fn update_session_placement(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    expected_placement_version: CanonicalU64,
    replacement: SessionPlacement,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    let requested_session = session_id;
    let requested_replacement = replacement.clone();
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::UpdateSessionPlacement {
            command_id,
            session_id,
            expected_placement_version,
            replacement,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionPlacementUpdated {
            session_id,
            placement_version,
            placement,
        } => {
            if !placement_update_receipt_matches(
                session_id,
                placement_version,
                &placement,
                requested_session,
                expected_placement_version,
                &requested_replacement,
            ) {
                return Err(
                    ClientError::Protocol("place returned an incoherent receipt").mutation(),
                );
            }
            output.session_placement_updated(
                session_id,
                placement_version.value(),
                &placement_display(&placement),
            )?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => {
            if code == ErrorCode::Rejected
                && !placement_update_rejection_matches(
                    detail.value(),
                    requested_session,
                    expected_placement_version,
                )
            {
                return Err(ClientError::Protocol("place returned incoherent rejection").mutation());
            }
            Err(ClientError::remote(code, message, detail).mutation())
        }
        _ => Err(ClientError::Protocol("place returned an unexpected response").mutation()),
    }
}

pub(crate) fn placement_update_receipt_matches(
    actual_session: CanonicalUuid,
    actual_version: CanonicalU64,
    actual_placement: &SessionPlacement,
    requested_session: CanonicalUuid,
    expected_version: CanonicalU64,
    requested_placement: &SessionPlacement,
) -> bool {
    actual_session == requested_session
        && expected_version.value().checked_add(1) == Some(actual_version.value())
        && actual_placement == requested_placement
}

pub(crate) fn placement_update_rejection_matches(
    detail: Option<RejectionDetail>,
    requested_session: CanonicalUuid,
    expected_version: CanonicalU64,
) -> bool {
    match detail {
        Some(RejectionDetail::SessionNotFound { session_id }) => session_id == requested_session,
        Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
            session_id,
            expected_placement_version,
            ..
        }) => session_id == requested_session && expected_placement_version == expected_version,
        Some(RejectionDetail::SessionPlacementVersionExhausted {
            session_id,
            current_placement_version,
        }) => session_id == requested_session && current_placement_version == expected_version,
        _ => false,
    }
}

pub(crate) async fn continue_imported(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    imported_conversation_id: CanonicalUuid,
    through_position: ThroughPositionArgument,
    relationship: signalbox_process_protocol::ImportedSessionRelationship,
    selection: ModelSelection,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    // An imported conversation is immutable, so its final position is stable:
    // resolving the sentinel here and sending the concrete ordinal keeps the
    // durable command byte-exact under replay.
    let through_position = match through_position {
        ThroughPositionArgument::Exact(position) => position,
        ThroughPositionArgument::Latest => {
            // The reader already rejects an empty inventory, so the resolved
            // count is a selectable position.
            let entry_count =
                read_imported_conversation(client, imported_conversation_id, |_| Ok(())).await?;
            output.resolved_through_position(entry_count)?;
            CanonicalU64::new(entry_count)
        }
    };
    let mut connection = client
        .mutation_request(ClientRequest::CreateSessionFromImportedFrontier {
            command_id,
            imported_conversation_id,
            through_position,
            relationship,
            initial_model_selection: selection,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } if model_settings.matches_model(&selection) => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("continue returned an unexpected response").mutation()),
    }
}

pub(crate) async fn compact(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    through_position: Option<CanonicalU64>,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CompactSession {
            command_id,
            session_id,
            through_position,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCompacted {
            session_id: compacted_session,
            context_compaction_id,
            model_call_id,
            through_position,
            summary_entry_id,
            result_frontier_id,
        } if compacted_session == session_id => Ok(output.session_compacted(
            session_id,
            context_compaction_id,
            model_call_id,
            through_position.value(),
            summary_entry_id,
            result_frontier_id,
        )?),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("compact returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObservedSessionDefaults {
    pub(crate) version: CanonicalU64,
    pub(crate) model_settings: ModelSettingsOverlay,
    pub(crate) dangerous_tool_auto_approval: bool,
    pub(crate) system_prompt: Option<SystemPromptText>,
}

/// The model verb's resolved replacement choice for the session system
/// prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelSystemPromptChoice {
    /// Copy the observed epoch's exact prompt forward unchanged.
    Keep,
    /// Replace the prompt with exact user-supplied file content.
    Replace(SystemPromptText),
    /// Install the replacement epoch without a prompt.
    Clear,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn replace_session_model(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    selection: ModelSelection,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
    system_prompt: ModelSystemPromptChoice,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let observed = match (defaults_version, dangerous_tool_auto_approval) {
        (Some(version), Some(posture)) => {
            // Recovery pins version and posture from the printed facts. A
            // copied-forward prompt is re-read from the immutable epoch the
            // printed version names, so the retried payload is byte-exact
            // regardless of later concurrent replacements.
            let named_defaults = read_session_defaults(client, session_id, Some(version)).await?;
            let system_prompt = match &system_prompt {
                ModelSystemPromptChoice::Keep => named_defaults.system_prompt,
                ModelSystemPromptChoice::Replace(_) | ModelSystemPromptChoice::Clear => None,
            };
            ObservedSessionDefaults {
                version,
                model_settings: named_defaults.model_settings,
                dangerous_tool_auto_approval: matches!(
                    posture,
                    DangerousToolAutoApprovalArgument::ApproveAll
                ),
                system_prompt,
            }
        }
        (None, None) => read_session_defaults(client, session_id, None).await?,
        (Some(_), None) | (None, Some(_)) => {
            return Err(ClientError::Input(
                "model recovery requires the complete printed defaults facts",
            ));
        }
    };
    output.recovery_value("defaults_version", &observed.version.value().to_string())?;
    output.recovery_value(
        "dangerous_tool_auto_approval",
        if observed.dangerous_tool_auto_approval {
            "approve-all"
        } else {
            "disabled"
        },
    )?;
    let replacement_system_prompt = match system_prompt {
        ModelSystemPromptChoice::Keep => observed.system_prompt.clone(),
        ModelSystemPromptChoice::Replace(text) => Some(text),
        ModelSystemPromptChoice::Clear => None,
    };

    let mut connection = client
        .mutation_request(ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version: observed.version,
            model_selection: selection,
            model_settings: observed.model_settings,
            dangerous_tool_auto_approval: observed.dangerous_tool_auto_approval,
            system_prompt: SystemPromptMember::present(replacement_system_prompt.clone()),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionDefaultsReplaced {
            session_id: replaced_session,
            defaults_version: installed_version,
            model_selection,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt: receipt_system_prompt,
            ..
        } if replaced_session == session_id
            && model_selection == selection
            && replacement_receipt_settings_match(observed.model_settings, &model_settings)
            && dangerous_tool_auto_approval == observed.dangerous_tool_auto_approval
            && receipt_system_prompt.value() == Some(&replacement_system_prompt)
            && observed
                .version
                .value()
                .checked_add(1)
                .is_some_and(|expected| installed_version.value() == expected) =>
        {
            output.session_defaults_replaced(
                replaced_session,
                installed_version.value(),
                &selection_display(model_selection),
            )?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("model replacement returned an unexpected response").mutation(),
        ),
    }
}

pub(crate) fn replacement_receipt_settings_match(
    requested: ModelSettingsOverlay,
    returned: &signalbox_process_protocol::ModelSettingsSnapshot,
) -> bool {
    returned.precedence.session == requested
}

pub(crate) async fn read_session_defaults(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
) -> Result<ObservedSessionDefaults, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadSessionDefaults {
            session_id,
            defaults_version,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::SessionDefaults {
            session_id: read_session,
            defaults_version: read_version,
            model_selection: _,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt,
            ..
        } if read_session == session_id
            && defaults_version.is_none_or(|named| named == read_version) =>
        {
            Ok(ObservedSessionDefaults {
                version: read_version,
                model_settings: model_settings.precedence.session,
                dangerous_tool_auto_approval,
                system_prompt,
            })
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "session defaults read returned an unexpected response",
        )),
    }
}

pub(crate) async fn read_session_metadata_page(
    client: &mut ProcessClient,
    page: &SessionMetadataPageRequest,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<Option<CanonicalUuid>, ClientError> {
    let mut connection = client.request(page.request()).await?;
    match connection.message().await? {
        ServerMessage::SessionMetadataPageStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session metadata page did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = page.after_session_id;
    let mut last_in_page = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionMetadataSummary { session_id, .. } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session metadata summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count.checked_add(1).ok_or(ClientError::Protocol(
                    "session metadata summary count overflowed",
                ))?;
                if summary_count > page.page_size.value() {
                    return Err(ClientError::Protocol(
                        "session metadata page exceeded its requested bound",
                    ));
                }
                prior_session = Some(*session_id);
                last_in_page = Some(*session_id);
                consume(&frame)?;
            }
            ServerMessage::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if session_count.value() != summary_count
                    || next_after_session_id.is_some() && *next_after_session_id != last_in_page
                {
                    return Err(ClientError::Protocol(
                        "session metadata page count or cursor was invalid",
                    ));
                }
                return Ok(*next_after_session_id);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session metadata page sequence or count was invalid",
                ));
            }
        }
    }
}
