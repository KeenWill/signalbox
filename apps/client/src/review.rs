use super::*;
use signalbox_process_protocol::MAX_REVIEW_PRODUCED_FINDINGS;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewConcernsFile {
    pub(crate) concerns: Vec<ReviewOrchestrationConcernInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewFindingsFile {
    pub(crate) findings: Vec<ReviewFindingInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewJudgmentMembersFile {
    members: Vec<ReviewJudgmentPlanMember>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRepairOutcomesFile {
    outcomes: Vec<ReviewRepairOutcome>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewPublicationOutcomesFile {
    outcomes: Vec<ReviewPublicationOutcome>,
}

pub(crate) async fn read_review_json_file<Value: DeserializeOwned>(
    path: &Path,
) -> Result<Value, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::review_input_file)?;
    let read_limit = u64::try_from(MAX_REVIEW_JSON_INPUT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("review JSON read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(ClientError::review_input_file)?;
    if bytes.len() > MAX_REVIEW_JSON_INPUT_BYTES {
        return Err(ClientError::ReviewInputExceedsFrame);
    }
    serde_json::from_slice(&bytes).map_err(ClientError::review_input_json)
}

pub(crate) fn validate_review_finding_count(
    count: usize,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    if count > MAX_REVIEW_PRODUCED_FINDINGS {
        return Err(ClientError::Input(
            "review findings exceed the structural per-pass count limit",
        ));
    }
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_review_findings_per_run
        .is_some_and(|maximum| u64::try_from(count).map_or(true, |count| count > maximum))
    {
        return Err(ClientError::Input(
            "review findings exceed the deployment count limit",
        ));
    }
    Ok(())
}

pub(crate) async fn review(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: ReviewCommand,
    deployment_limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    match command {
        ReviewCommand::CreateTarget {
            command_id,
            target_id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_target_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::CreateReviewTarget {
                    command_id,
                    target_id,
                    provider,
                    repository,
                    subject,
                    head_revision,
                    base_revision,
                    stack_parent_target_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewTargetCreated {
                    target_id: recorded,
                } if recorded == target_id => {
                    output.review_acknowledgement(&format!("target={recorded} created"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review target creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::StartRun {
            command_id,
            target_id,
            run_id,
            pass_id,
            workflow,
            session_id,
            accepted_input_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::StartReviewRun {
                    command_id,
                    target_id,
                    run_id,
                    pass_id,
                    workflow,
                    session_id,
                    accepted_input_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewRunStarted {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} started"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review run creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ActivatePass {
            command_id,
            run_id,
            pass_id,
            turn_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::ActivateReviewPass {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewPassActivated {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} activated"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review pass activation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFinding {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding,
        } => {
            validate_review_finding_count(1, deployment_limits)?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: vec![finding],
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingsRecorded {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    finding_count,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && finding_count.value() == 1 =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} findings=1 recorded"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding admission returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings_file,
        } => {
            let file: ReviewFindingsFile = read_review_json_file(&findings_file).await?;
            let finding_count = file.findings.len();
            validate_review_finding_count(finding_count, deployment_limits)?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: file.findings,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingsRecorded {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    finding_count: recorded_count,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && usize::try_from(recorded_count.value()) == Ok(finding_count) =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} findings={finding_count} recorded"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding inventory admission returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::CompletePass {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::CompleteReviewPass {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewPassCompleted {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    state,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && review_pass_completion_is_coherent(outcome, state) =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} completed"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review pass completion returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFindingEvent {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding_id,
            event_ordinal,
            event,
        } => {
            let expected_status = review_finding_event_status(&event);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindingEvent {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    finding_id,
                    event_ordinal,
                    event,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingEventRecorded {
                    finding_id: recorded,
                    status,
                } if recorded == finding_id && status == expected_status => {
                    output.review_acknowledgement(&format!("finding={recorded} event recorded"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding event returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::StartOrchestration {
            command_id,
            attempt_id,
            target_id,
            concern_set_version,
            import_template_name,
            judgment_template_name,
            repair_template_name,
            publication_template_name,
            concerns_file,
        } => {
            let file: ReviewConcernsFile = read_review_json_file(&concerns_file).await?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::StartReviewOrchestration {
                    command_id,
                    attempt_id,
                    target_id,
                    concern_set_version,
                    import_template_name,
                    judgment_template_name,
                    repair_template_name,
                    publication_template_name,
                    concerns: file.concerns,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationStarted {
                    attempt_id: recorded,
                } if recorded == attempt_id => {
                    output.review_acknowledgement(&format!("attempt={recorded} started"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review orchestration start returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordImportOutcome {
            command_id,
            attempt_id,
            pass_id,
            external_link_id,
            context_digest,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewImportOutcome {
                    command_id,
                    attempt_id,
                    pass_id,
                    external_link_id,
                    context_digest,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id && review_import_state_is_coherent(outcome, state) => {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review import outcome returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordConcernOutcome {
            command_id,
            attempt_id,
            concern,
            pass_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewConcernOutcome {
                    command_id,
                    attempt_id,
                    concern,
                    pass_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id && review_concern_state_is_coherent(outcome, state) => {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review concern outcome returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordJudgmentPlan {
            command_id,
            attempt_id,
            analysis_pass_id,
            members_file,
        } => {
            let file: ReviewJudgmentMembersFile = read_review_json_file(&members_file).await?;
            let plan_is_empty = file.members.is_empty();
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewJudgmentPlan {
                    command_id,
                    attempt_id,
                    analysis_pass_id,
                    members: file.members,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_judgment_plan_state_is_coherent(plan_is_empty, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review judgment plan returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordJudgmentEffect {
            command_id,
            attempt_id,
            finding_id,
            event_pass_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewJudgmentEffect {
                    command_id,
                    attempt_id,
                    finding_id,
                    event_pass_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_judgment_effect_state_is_coherent(outcome, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review judgment effect returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordRepairOutcomes {
            command_id,
            attempt_id,
            outcomes_file,
        } => {
            let file: ReviewRepairOutcomesFile = read_review_json_file(&outcomes_file).await?;
            let has_blocked = file
                .outcomes
                .iter()
                .any(|outcome| outcome.outcome == ReviewRepairTerminalOutcome::Blocked);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewRepairOutcomes {
                    command_id,
                    attempt_id,
                    outcomes: file.outcomes,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_repair_state_is_coherent(has_blocked, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review repair outcomes returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordPublicationOutcomes {
            command_id,
            attempt_id,
            outcomes_file,
        } => {
            let file: ReviewPublicationOutcomesFile = read_review_json_file(&outcomes_file).await?;
            let all_published = file
                .outcomes
                .iter()
                .all(|outcome| outcome.outcome == ReviewPublicationTerminalOutcome::Published);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewPublicationOutcomes {
                    command_id,
                    attempt_id,
                    outcomes: file.outcomes,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_publication_state_is_coherent(all_published, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review publication outcomes returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ReserveExternalLink {
            command_id,
            external_link_id,
            finding_id,
            provider,
            object_kind,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::ReserveReviewExternalLink {
                    command_id,
                    external_link_id,
                    finding_id,
                    provider,
                    object_kind,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewExternalLinkReserved {
                    external_link_id: recorded,
                } if recorded == external_link_id => {
                    output.review_acknowledgement(&format!("external_link={recorded} reserved"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review external-link reservation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::AttachExternalLink {
            command_id,
            external_link_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            external_object,
            event_ordinal,
        } => {
            let expected_external_object = external_object.clone();
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::AttachReviewExternalLink {
                    command_id,
                    external_link_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    external_object,
                    event_ordinal,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewExternalLinkAttached {
                    external_link_id: recorded,
                    external_object: recorded_object,
                } if recorded == external_link_id
                    && recorded_object == expected_external_object =>
                {
                    output.review_acknowledgement(&format!("external_link={recorded} attached"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review external-link attachment returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ReadOrchestration { attempt_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewOrchestration { attempt_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewOrchestration { snapshot }
                    if snapshot.attempt_id == attempt_id =>
                {
                    output.review_orchestration(&snapshot)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review orchestration read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadTarget { target_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewTarget { target_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewTarget { target } if target.target_id == target_id => {
                    output.review_target(&target)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review target read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadRun { run_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewRun { run_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewRun { run, pass }
                    if run.run_id == run_id
                        && review_run_response_is_coherent(&run, pass.as_ref()) =>
                {
                    output.review_run(&run, pass.as_ref())?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review run read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadFinding { finding_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewFinding { finding_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewFinding { finding }
                    if finding.finding.finding_id == finding_id =>
                {
                    output.review_finding(&finding)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review finding read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ListFindings { run_id } => {
            let mut connection = client
                .request(ClientRequest::ListReviewFindings { run_id })
                .await?;
            let start = connection.frame().await?;
            match start.message() {
                ServerMessage::ReviewFindingsStart { run_id: selected } if *selected == run_id => {}
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => {
                    return Err(ClientError::remote(*code, message.clone(), *detail));
                }
                _ => {
                    return Err(ClientError::Protocol(
                        "review finding list did not start correctly",
                    ));
                }
            }
            let mut spool = tempfile::tempfile()?;
            let mut count = 0_u64;
            let mut previous_finding_id: Option<CanonicalUuid> = None;
            loop {
                let frame = connection.frame().await?;
                match frame.message() {
                    ServerMessage::ReviewFindingItem { finding } if finding.run_id == run_id => {
                        let finding_id = finding.finding.finding_id;
                        if previous_finding_id
                            .is_some_and(|previous| finding_id.into_uuid() <= previous.into_uuid())
                        {
                            return Err(ClientError::Protocol(
                                "review finding list identity order was invalid",
                            ));
                        }
                        previous_finding_id = Some(finding_id);
                        count = count.checked_add(1).ok_or(ClientError::Protocol(
                            "review finding list count overflowed",
                        ))?;
                        if count > MAX_REVIEW_PRODUCED_FINDINGS as u64 {
                            return Err(ClientError::Protocol(
                                "review finding list exceeded its structural count limit",
                            ));
                        }
                        spool.write_all(&encode_server_line(&frame)?)?;
                    }
                    ServerMessage::ReviewFindingsEnd { finding_count }
                        if finding_count.value() == count =>
                    {
                        break;
                    }
                    ServerMessage::Error {
                        code,
                        message,
                        detail,
                    } => {
                        return Err(ClientError::remote(*code, message.clone(), *detail));
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding list sequence or count was invalid",
                        ));
                    }
                }
            }
            spool.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new(spool);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? != 0 {
                match decode_server_line(&line)?.message() {
                    ServerMessage::ReviewFindingItem { finding } => {
                        output.review_finding(finding)?;
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding spool contained a non-finding frame",
                        ));
                    }
                }
                line.clear();
            }
            Ok(())
        }
    }
}

pub(crate) const fn review_pass_completion_is_coherent(
    outcome: ReviewPassTerminalOutcome,
    state: ReviewPassLifecycle,
) -> bool {
    matches!(
        (outcome, state),
        (
            ReviewPassTerminalOutcome::Succeeded,
            ReviewPassLifecycle::Succeeded
        ) | (
            ReviewPassTerminalOutcome::Failed,
            ReviewPassLifecycle::Failed
        ) | (
            ReviewPassTerminalOutcome::Blocked,
            ReviewPassLifecycle::Blocked
        ) | (
            ReviewPassTerminalOutcome::Cancelled,
            ReviewPassLifecycle::Cancelled
        )
    )
}

pub(crate) const fn review_finding_event_status(event: &ReviewFindingEvent) -> ReviewFindingStatus {
    match event {
        ReviewFindingEvent::Accepted {} => ReviewFindingStatus::Accepted,
        ReviewFindingEvent::Rejected { .. } => ReviewFindingStatus::Rejected,
        ReviewFindingEvent::Duplicate { .. } => ReviewFindingStatus::Duplicate,
        ReviewFindingEvent::Superseded { .. } => ReviewFindingStatus::Superseded,
        ReviewFindingEvent::Stale {} => ReviewFindingStatus::Stale,
        ReviewFindingEvent::Fixed {} => ReviewFindingStatus::Fixed,
        ReviewFindingEvent::BlockedWithReason { .. } => ReviewFindingStatus::BlockedWithReason,
    }
}

const fn review_import_state_is_coherent(
    outcome: ReviewImportTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewImportTerminalOutcome::Succeeded => {
            matches!(state, ReviewOrchestrationState::AwaitingConcerns)
        }
        ReviewImportTerminalOutcome::Failed
        | ReviewImportTerminalOutcome::Blocked
        | ReviewImportTerminalOutcome::Cancelled => {
            matches!(state, ReviewOrchestrationState::ImportIncomplete)
        }
    }
}

pub(crate) const fn review_concern_state_is_coherent(
    outcome: ReviewConcernTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewConcernTerminalOutcome::Succeeded => matches!(
            state,
            ReviewOrchestrationState::AwaitingConcerns
                | ReviewOrchestrationState::FanoutIncomplete
                | ReviewOrchestrationState::AwaitingJudgment
        ),
        ReviewConcernTerminalOutcome::Failed
        | ReviewConcernTerminalOutcome::Blocked
        | ReviewConcernTerminalOutcome::Cancelled => matches!(
            state,
            ReviewOrchestrationState::AwaitingConcerns | ReviewOrchestrationState::FanoutIncomplete
        ),
    }
}

pub(crate) const fn review_judgment_plan_state_is_coherent(
    plan_is_empty: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (plan_is_empty, state),
        (true, ReviewOrchestrationState::AwaitingRepair)
            | (false, ReviewOrchestrationState::AwaitingJudgmentEffects)
    )
}

pub(crate) const fn review_judgment_effect_state_is_coherent(
    outcome: ReviewJudgmentEffectTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewJudgmentEffectTerminalOutcome::Applied => matches!(
            state,
            ReviewOrchestrationState::AwaitingJudgmentEffects
                | ReviewOrchestrationState::AwaitingRepair
        ),
        ReviewJudgmentEffectTerminalOutcome::Failed
        | ReviewJudgmentEffectTerminalOutcome::Blocked
        | ReviewJudgmentEffectTerminalOutcome::Cancelled => {
            matches!(state, ReviewOrchestrationState::JudgmentIncomplete)
        }
    }
}

pub(crate) const fn review_repair_state_is_coherent(
    has_blocked: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (has_blocked, state),
        (true, ReviewOrchestrationState::RepairIncomplete)
            | (false, ReviewOrchestrationState::AwaitingPublication)
    )
}

pub(crate) const fn review_publication_state_is_coherent(
    all_published: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (all_published, state),
        (true, ReviewOrchestrationState::Complete)
            | (false, ReviewOrchestrationState::PublicationIncomplete)
    )
}

pub(crate) fn review_run_response_is_coherent(
    run: &ReviewRunSnapshot,
    pass: Option<&ReviewPassSnapshot>,
) -> bool {
    match (run.pass_id, pass) {
        (None, None) => true,
        (Some(pass_id), Some(pass)) => {
            pass.pass_id == pass_id && pass.run_id == run.run_id && pass.target_id == run.target_id
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn review_command_identity(
    output: &mut Output<'_>,
    supplied: Option<CommandId>,
) -> Result<CommandId, ClientError> {
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    Ok(command_id)
}
