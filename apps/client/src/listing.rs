use super::*;

pub(crate) async fn list(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    read_session_summaries(client, |_, frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                model_selection,
                placement_version,
                placement,
                runner,
            } => output.session_summary(
                *session_id,
                defaults_version.value(),
                &selection_display(*model_selection),
                placement_version.value(),
                &placement_display(placement),
                runner.as_ref(),
            )?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

pub(crate) async fn list_templates(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ListTemplates {}).await?;
    match connection.message().await? {
        ServerMessage::TemplatesStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "template list did not begin with its start frame",
            ));
        }
    }
    let mut spool = tempfile::tempfile()?;
    let mut prior_name: Option<String> = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::TemplateSummary { name, .. } => {
                if prior_name
                    .as_ref()
                    .is_some_and(|prior| prior.as_str() >= name.as_str())
                {
                    return Err(ClientError::Protocol(
                        "template summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("template summary count overflowed"))?;
                prior_name = Some(name.clone());
                spool.write_all(&encode_server_line(&frame)?)?;
            }
            ServerMessage::TemplatesEnd { template_count }
                if template_count.value() == summary_count =>
            {
                break;
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "template list sequence or count was invalid",
                ));
            }
        }
    }
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::TemplateSummary { name, version } => {
                output.template_summary(name, version.value())?;
            }
            _ => {
                return Err(ClientError::Protocol(
                    "template-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

pub(crate) async fn search(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page: SessionMetadataPageRequest,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let next_after_session_id = read_session_metadata_page(client, &page, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionMetadataSummary {
                session_id,
                defaults_version,
                model_selection,
                dangerous_tool_auto_approval,
                title,
                tags,
                archived,
                last_writer,
            } => output.session_metadata_summary(&SessionMetadataRow {
                session_id: *session_id,
                defaults_version: defaults_version.value(),
                selection: &selection_display(*model_selection),
                dangerous_tool_auto_approval: *dangerous_tool_auto_approval,
                archived: *archived,
                last_writer: *last_writer,
                tags,
                title: title.as_deref(),
            })?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-metadata spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    if let Some(next_after_session_id) = next_after_session_id {
        output.next_page_cursor(next_after_session_id)?;
    }
    Ok(())
}

/// Orders unified cursors exactly as the daemon lists rows: by identity UUID
/// value, native before imported for a theoretical equal identity.
fn conversation_cursor_key(cursor: ConversationCursor) -> (Uuid, u8) {
    let origin_rank = match cursor.origin() {
        ConversationOrigin::NativeSession => 0,
        ConversationOrigin::ImportedConversation => 1,
    };
    (cursor.conversation_id().into_uuid(), origin_rank)
}

async fn read_conversation_page(
    client: &mut ProcessClient,
    page: &ConversationsPageRequest,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<Option<ConversationCursor>, ClientError> {
    let mut connection = client.request(page.request()).await?;
    match connection.message().await? {
        ServerMessage::ConversationPageStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "conversation page did not begin with its start frame",
            ));
        }
    }
    let mut prior_cursor = page.after;
    let mut last_in_page = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::ConversationSummary { conversation } => {
                let cursor = conversation.cursor();
                if prior_cursor.is_some_and(|prior| {
                    conversation_cursor_key(prior) >= conversation_cursor_key(cursor)
                }) {
                    return Err(ClientError::Protocol(
                        "conversation summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("conversation count overflowed"))?;
                if summary_count > page.page_size.value() {
                    return Err(ClientError::Protocol(
                        "conversation page exceeded its requested bound",
                    ));
                }
                prior_cursor = Some(cursor);
                last_in_page = Some(cursor);
                consume(&frame)?;
            }
            ServerMessage::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if conversation_count.value() != summary_count
                    || next_after.is_some() && *next_after != last_in_page
                {
                    return Err(ClientError::Protocol(
                        "conversation page count or cursor was invalid",
                    ));
                }
                return Ok(*next_after);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "conversation page sequence or count was invalid",
                ));
            }
        }
    }
}

pub(crate) async fn conversations(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page: ConversationsPageRequest,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let next_after = read_conversation_page(client, &page, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::ConversationSummary { conversation } => match conversation {
                ConversationSummary::NativeSession {
                    session_id,
                    title,
                    archived,
                    defaults_version,
                } => output.conversation_summary(&ConversationRow::Native {
                    session_id: *session_id,
                    archived: *archived,
                    defaults_version: defaults_version.value(),
                    title: title.as_deref(),
                })?,
                ConversationSummary::ImportedConversation {
                    imported_conversation_id,
                    title,
                    entry_count,
                    source_format,
                } => output.conversation_summary(&ConversationRow::Imported {
                    imported_conversation_id: *imported_conversation_id,
                    format: imported_source_format_label(*source_format),
                    entry_count: entry_count.value(),
                    title: title.as_deref(),
                })?,
            },
            _ => {
                return Err(ClientError::Protocol(
                    "conversation spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    if let Some(next_after) = next_after {
        output.next_conversation_cursor(
            conversation_origin_label(next_after.origin()),
            next_after.conversation_id(),
        )?;
    }
    Ok(())
}

/// The exact origin spelling the `--after` cursor argument accepts back.
const fn conversation_origin_label(origin: ConversationOrigin) -> &'static str {
    match origin {
        ConversationOrigin::NativeSession => "native",
        ConversationOrigin::ImportedConversation => "imported",
    }
}

const fn imported_source_format_label(
    format: signalbox_process_protocol::ImportedConversationSourceFormat,
) -> &'static str {
    match format {
        signalbox_process_protocol::ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1 => {
            "claude-code-session-jsonl-v1"
        }
        signalbox_process_protocol::ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2 => {
            "claude-code-session-jsonl-v2"
        }
        signalbox_process_protocol::ImportedConversationSourceFormat::CodexRolloutJsonlV1 => {
            "codex-rollout-jsonl-v1"
        }
    }
}
