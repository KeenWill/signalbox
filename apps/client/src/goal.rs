use super::*;

async fn read_goal_text_argument(argument: GoalTextArgument) -> Result<String, ClientError> {
    match argument {
        GoalTextArgument::Inline(text) => validate_goal_text_input(text),
        GoalTextArgument::File(path) => read_goal_text_file(&path).await,
    }
}

pub(crate) async fn read_goal_text_file(path: &Path) -> Result<String, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ClientError::goal_text_file(path, error))?;
    let read_limit = u64::try_from(MAX_CONTENT_FRAGMENT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("goal text read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ClientError::goal_text_file(path, error))?;
    if bytes.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "goal text exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("goal text must be valid UTF-8"))?;
    validate_goal_text_input(text)
}

fn validate_goal_text_input(text: String) -> Result<String, ClientError> {
    if text.is_empty() {
        return Err(ClientError::Input("goal text must not be empty"));
    }
    if text.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "goal text exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    if text.contains('\0') {
        return Err(ClientError::Input("goal text must not contain U+0000"));
    }
    Ok(text)
}

pub(crate) async fn goal(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: GoalCommand,
    stdin: &mut dyn Read,
) -> Result<(), ClientError> {
    match command {
        GoalCommand::Attach {
            session_id,
            statement,
            command_id,
        } => {
            let statement = read_goal_text_argument(statement).await?;
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::AttachGoal {
                    command_id,
                    session_id,
                    statement,
                }
            })
            .await
        }
        GoalCommand::Show { session_id } => goal_show(client, output, session_id).await,
        GoalCommand::Resume {
            session_id,
            guidance,
            command_id,
        } => {
            let guidance = match guidance {
                Some(guidance) => Some(read_goal_text_argument(guidance).await?),
                None => read_optional_resume_guidance(stdin)?,
            };
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::ResumeGoal {
                    command_id,
                    session_id,
                    guidance,
                }
            })
            .await
        }
        GoalCommand::Stop {
            session_id,
            command_id,
            descendants,
        } => {
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::StopGoal {
                    command_id,
                    session_id,
                    descendant_scope: descendant_scope(descendants),
                }
            })
            .await
        }
        GoalCommand::Supersede {
            session_id,
            statement,
            command_id,
        } => {
            let statement = read_goal_text_argument(statement).await?;
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::SupersedeGoal {
                    command_id,
                    session_id,
                    statement,
                }
            })
            .await
        }
    }
}

async fn goal_mutation<BuildRequest>(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    expected_session: CanonicalUuid,
    command_id: Option<CommandId>,
    build_request: BuildRequest,
) -> Result<(), ClientError>
where
    BuildRequest: FnOnce(CommandId) -> ClientRequest,
{
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let request = build_request(command_id);
    let expected_termination = match &request {
        ClientRequest::StopGoal {
            descendant_scope, ..
        } => Some(*descendant_scope),
        _ => None,
    };
    let mut connection = client.mutation_request(request).await?;
    let receipt = decode_goal_mutation_receipt(
        expected_session,
        connection.message().await.map_err(ClientError::mutation)?,
    )?;
    if receipt.termination.map(|value| value.descendant_scope) != expected_termination {
        return Err(
            ClientError::Protocol("goal receipt has mismatched termination scope").mutation(),
        );
    }
    if let Some(termination) = receipt.termination {
        output.termination_receipt(termination)?;
    }
    output
        .goal_transition_applied(
            receipt.session_id,
            receipt.event_ordinal,
            receipt.generation,
        )
        .map_err(ClientError::from)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GoalMutationReceipt {
    session_id: CanonicalUuid,
    event_ordinal: u64,
    generation: u64,
    termination: Option<signalbox_process_protocol::TerminationReceipt>,
}

pub(crate) fn decode_goal_mutation_receipt(
    expected_session: CanonicalUuid,
    message: ServerMessage,
) -> Result<GoalMutationReceipt, ClientError> {
    match message {
        ServerMessage::GoalTransitionApplied {
            termination,
            session_id,
            event_ordinal,
            generation,
        } if session_id == expected_session => Ok(GoalMutationReceipt {
            session_id,
            event_ordinal: event_ordinal.value(),
            generation: generation.value(),
            termination,
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("goal mutation returned an unexpected response").mutation()),
    }
}

#[derive(Debug)]
struct GoalHistoryProjection {
    generation: u64,
    statement: String,
    state: GoalLifecycleState,
}

#[derive(Debug, Default)]
pub(crate) struct GoalHistoryReplay {
    current: Option<GoalHistoryProjection>,
}

impl GoalHistoryReplay {
    pub(crate) fn apply(
        &mut self,
        generation: u64,
        event: &GoalHistoryEvent,
    ) -> Result<(), ClientError> {
        let current = self.current.take();
        let next = match (current, event) {
            (None, GoalHistoryEvent::Commissioned { statement, .. }) if generation == 1 => {
                GoalHistoryProjection {
                    generation,
                    statement: statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(current), GoalHistoryEvent::Commissioned { statement, .. })
                if goal_state_admits_commission(&current.state)
                    && current.generation.checked_add(1) == Some(generation) =>
            {
                GoalHistoryProjection {
                    generation,
                    statement: statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(mut current), GoalHistoryEvent::Blocked { reason, need, .. })
                if generation == current.generation && goal_state_is_pursuing(&current.state) =>
            {
                current.state = GoalLifecycleState::Blocked {
                    reason: *reason,
                    need: need.clone(),
                };
                current
            }
            (Some(mut current), GoalHistoryEvent::Resumed { .. })
                if generation == current.generation && goal_state_is_blocked(&current.state) =>
            {
                current.state = GoalLifecycleState::Pursuing {};
                current
            }
            (
                Some(mut current),
                GoalHistoryEvent::Achieved {
                    turn_id,
                    tool_request_id,
                    ..
                },
            ) if generation == current.generation && goal_state_is_pursuing(&current.state) => {
                current.state = GoalLifecycleState::Achieved {
                    turn_id: *turn_id,
                    tool_request_id: *tool_request_id,
                };
                current
            }
            (Some(mut current), GoalHistoryEvent::UserStopped { .. })
                if generation == current.generation && goal_state_is_open(&current.state) =>
            {
                current.state = GoalLifecycleState::UserStopped {};
                current
            }
            (
                Some(current),
                GoalHistoryEvent::Superseded {
                    replacement_statement,
                    ..
                },
            ) if generation == current.generation && goal_state_is_open(&current.state) => {
                let successor = current
                    .generation
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("goal history generation overflowed"))?;
                GoalHistoryProjection {
                    generation: successor,
                    statement: replacement_statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(mut current), GoalHistoryEvent::SessionClosed { outcome, .. })
                if generation == current.generation && goal_state_is_open(&current.state) =>
            {
                current.state = GoalLifecycleState::SessionClosed { outcome: *outcome };
                current
            }
            _ => {
                return Err(ClientError::Protocol(
                    "goal history contained an invalid lifecycle transition",
                ));
            }
        };
        self.current = Some(next);
        Ok(())
    }

    pub(crate) fn validate_projection(
        self,
        generation: u64,
        statement: &str,
        state: &GoalLifecycleState,
    ) -> Result<(), ClientError> {
        match self.current {
            Some(current)
                if current.generation == generation
                    && current.statement == statement
                    && current.state == *state =>
            {
                Ok(())
            }
            Some(_) | None => Err(ClientError::Protocol(
                "goal history did not derive its declared current projection",
            )),
        }
    }
}

const fn goal_state_is_pursuing(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Pursuing {} => true,
        GoalLifecycleState::Blocked { .. }
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_is_blocked(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Blocked { .. } => true,
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_is_open(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Pursuing {} | GoalLifecycleState::Blocked { .. } => true,
        GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_admits_commission(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Achieved { .. } | GoalLifecycleState::UserStopped {} => true,
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Blocked { .. }
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

async fn goal_show(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadGoal { session_id })
        .await?;
    let first = connection.frame().await?;
    let (current_generation, current_statement) = match first.message() {
        ServerMessage::GoalHistoryStart {
            session_id: observed,
            current_generation,
            current_statement,
        } if *observed == session_id => (current_generation.value(), current_statement.clone()),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(*code, message.clone(), *detail)),
        _ => {
            return Err(ClientError::Protocol(
                "goal history did not begin with its selected session",
            ));
        }
    };
    let state_frame = connection.frame().await?;
    let current_state = match state_frame.message() {
        ServerMessage::GoalHistoryState { current_state } => current_state.clone(),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(*code, message.clone(), *detail)),
        _ => {
            return Err(ClientError::Protocol(
                "goal history did not carry its current state after its projection",
            ));
        }
    };
    let mut spool = tempfile::tempfile()?;
    let mut replay = GoalHistoryReplay::default();
    let mut event_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::GoalHistoryItem {
                event_ordinal,
                generation,
                event,
            } if event_ordinal.value() == event_count.saturating_add(1) => {
                event_count = event_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("goal event count overflowed"))?;
                replay.apply(generation.value(), event)?;
                spool.write_all(&encode_server_line(&frame)?)?;
            }
            ServerMessage::GoalHistoryEnd {
                event_count: declared,
            } if declared.value() == event_count => break,
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "goal history sequence or count was invalid",
                ));
            }
        }
    }
    replay.validate_projection(current_generation, &current_statement, &current_state)?;
    output.goal_current(
        session_id,
        current_generation,
        &current_statement,
        &current_state,
    )?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::GoalHistoryItem {
                event_ordinal,
                generation,
                event,
            } => output.goal_history_event(event_ordinal.value(), generation.value(), event)?,
            _ => {
                return Err(ClientError::Protocol(
                    "goal-history spool contained an unexpected frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

fn read_optional_resume_guidance(stdin: &mut dyn Read) -> Result<Option<String>, ClientError> {
    let mut bytes = Vec::new();
    stdin
        .take((MAX_CONTENT_FRAGMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("goal text must be valid UTF-8"))?;
    validate_goal_text_input(text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_resume_guidance_is_preserved() {
        let guidance = "Keep the requested scope.\n";
        assert_eq!(
            read_optional_resume_guidance(&mut guidance.as_bytes()).unwrap(),
            Some(guidance.to_owned())
        );
    }

    #[test]
    fn empty_resume_input_preserves_absent_guidance() {
        assert_eq!(
            read_optional_resume_guidance(&mut std::io::empty()).unwrap(),
            None
        );
    }

    #[test]
    fn piped_resume_guidance_keeps_the_goal_byte_bound() {
        let oversized = vec![b'x'; MAX_CONTENT_FRAGMENT_BYTES + 1];
        let error = read_optional_resume_guidance(&mut oversized.as_slice()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "goal text exceeds the 1 MiB UTF-8 byte limit"
        );
    }
}
