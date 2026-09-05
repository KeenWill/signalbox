use super::*;

pub(super) async fn handle_review_orchestration_mutation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match execute_review_orchestration_request(request, digest, pool.clone(), templates).await {
        Ok(message) => write_message(writer, version, request_id, message).await,
        Err(error) => write_review_orchestration_error(writer, version, request_id, error).await,
    }
}

pub(super) async fn handle_read_review_orchestration<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let result = read_review_orchestration_request(request, [0; 32], pool.clone(), templates).await;
    drop(snapshot_permit);
    match result {
        Ok(message) => write_message(writer, version, request_id, message).await,
        Err(error) => write_review_orchestration_error(writer, version, request_id, error).await,
    }
}

pub(super) async fn write_review_orchestration_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ReviewOrchestrationRuntimeError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        ReviewOrchestrationRuntimeError::InvalidRequest
        | ReviewOrchestrationRuntimeError::Rejected => {
            ProtocolError::without_detail(ErrorCode::InvalidRequest)
        }
        ReviewOrchestrationRuntimeError::NotFound => {
            ProtocolError::without_detail(ErrorCode::NotFound)
        }
        ReviewOrchestrationRuntimeError::ConflictingReuse => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        ReviewOrchestrationRuntimeError::Unavailable { commit_ambiguous } => {
            review_orchestration_unavailable_error(commit_ambiguous)
        }
        ReviewOrchestrationRuntimeError::Internal { session_id, cause } => {
            let diagnostic = match cause {
                ReviewOrchestrationInternalCause::StoreCorruption => {
                    InternalDiagnostic::ReviewOrchestrationStoreCorruption
                }
                ReviewOrchestrationInternalCause::WorkflowCorruption => {
                    InternalDiagnostic::ReviewOrchestrationWorkflowCorruption
                }
                ReviewOrchestrationInternalCause::SessionCorruption => {
                    InternalDiagnostic::ReviewOrchestrationSessionCorruption
                }
                ReviewOrchestrationInternalCause::ServiceContract => {
                    InternalDiagnostic::ReviewOrchestrationServiceContract
                }
            };
            internal_protocol_error(session_id, diagnostic)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

pub(super) fn review_orchestration_unavailable_error(commit_ambiguous: bool) -> ProtocolError {
    let failure_class = OperatorFailureClass::Infrastructure { commit_ambiguous };
    let cause_code = if commit_ambiguous {
        "review_orchestration_commit_ambiguous"
    } else {
        "review_orchestration_database_unavailable"
    };
    tracing::error!(
        ?failure_class,
        cause_code,
        session_id = tracing::field::Empty,
        "review orchestration request failed"
    );
    ProtocolError::mutation_unavailable(commit_ambiguous)
}

pub(super) fn required_review_digest(
    digest: Option<[u8; 32]>,
) -> Result<[u8; 32], ProcessConnectionError> {
    digest.ok_or(ProcessConnectionError::EncodeInvariant)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_create_review_target<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    target_id: CanonicalUuid,
    provider: String,
    repository: String,
    subject: WireReviewTargetSubject,
    head_revision: String,
    base_revision: Option<String>,
    stack_parent_target_id: Option<CanonicalUuid>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::CreateTarget,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    let parent = match stack_parent_target_id {
        Some(parent) => match store
            .load_target(ReviewTargetId::from_uuid(parent.into_uuid()))
            .await
        {
            Ok(Some(parent)) => Some(parent),
            Ok(None) => return write_review_invalid(writer, version, request_id).await,
            Err(error) => {
                return write_review_store_error(writer, version, request_id, error).await;
            }
        },
        None => None,
    };
    let subject = match subject {
        WireReviewTargetSubject::ChangeRequest { number } => {
            let Ok(number) = ReviewChangeRequestNumber::try_new(number.value()) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            ReviewTargetSubject::ChangeRequest(number)
        }
        WireReviewTargetSubject::Commit {} => ReviewTargetSubject::Commit,
    };
    let values = (
        ReviewKey::try_new(provider),
        ReviewKey::try_new(repository),
        ReviewKey::try_new(head_revision),
        base_revision.map(ReviewKey::try_new).transpose(),
    );
    let (Ok(provider), Ok(repository), Ok(head), Ok(base)) = values else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(target) = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(target_id.into_uuid()),
        provider,
        repository,
        subject,
        head,
        base,
        parent.as_ref(),
    ) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::CreateTarget(target),
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_start_review_run<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    target_id: CanonicalUuid,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    workflow: WireReviewWorkflow,
    session_id: CanonicalUuid,
    accepted_input_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::StartRun,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    match store
        .load_target(ReviewTargetId::from_uuid(target_id.into_uuid()))
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    }
    let origin = match store
        .load_accepted_input_origin(AcceptedInputId::from_uuid(accepted_input_id.into_uuid()))
        .await
    {
        Ok(Some(origin)) => origin,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if origin.session() != SessionId::from_uuid(session_id.into_uuid())
        || origin.origin_turn().is_none()
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let reference = ReviewRunRef::new(
        ReviewTargetId::from_uuid(target_id.into_uuid()),
        ReviewRunId::from_uuid(run_id.into_uuid()),
    );
    let (workflow_kind, pass_kind) = review_workflow_kind(workflow);
    let mut run = ReviewRun::new(reference, workflow_kind, ReviewPolicy::version_one());
    let pass = ReviewPass::try_new(
        ReviewPassRef::new(reference, ReviewPassId::from_uuid(pass_id.into_uuid())),
        pass_kind,
        &mut run,
        SessionId::from_uuid(session_id.into_uuid()),
        ReviewPassAcceptedInputEvidence::new(
            AcceptedInputId::from_uuid(accepted_input_id.into_uuid()),
            origin.session(),
            origin.origin_turn(),
        ),
    );
    let Ok(pass) = pass else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::StartRun { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

const fn review_workflow_kind(
    workflow: WireReviewWorkflow,
) -> (ReviewWorkflowKind, ReviewPassKind) {
    match workflow {
        WireReviewWorkflow::ImportExternalContext => (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext,
        ),
        WireReviewWorkflow::ReadOnlyReview => (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview,
        ),
        WireReviewWorkflow::JudgeFindings => {
            (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
        }
        WireReviewWorkflow::DedupeFindings => {
            (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
        }
        WireReviewWorkflow::PublishReview => {
            (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
        }
        WireReviewWorkflow::FixFindings => (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix),
        WireReviewWorkflow::PropagateStack => (
            ReviewWorkflowKind::PropagateStack,
            ReviewPassKind::PropagateStack,
        ),
    }
}

pub(super) async fn replay_review_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    command_id: signalbox_process_protocol::CommandId,
    digest: [u8; 32],
    operation_kind: ReviewWorkflowOperationKind,
) -> Result<bool, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    match store
        .load_command_outcome(
            DurableCommandId::from_uuid(command_id.into_uuid()),
            digest,
            operation_kind,
        )
        .await
    {
        Ok(Some(outcome)) => {
            write_review_command_outcome(writer, version, request_id, outcome).await?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            write_review_store_error(writer, version, request_id, error).await?;
            Ok(true)
        }
    }
}

pub(super) async fn execute_review_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    command: ReviewWorkflowCommand,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut service = ReviewWorkflowCommandService::new(ReviewWorkflowStore::new(pool.clone()));
    match service.execute(command).await {
        Ok(outcome) => write_review_command_outcome(writer, version, request_id, outcome).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

pub(super) async fn write_review_command_outcome<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    outcome: ReviewWorkflowCommandOutcome,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match outcome {
        ReviewWorkflowCommandOutcome::Recorded(result) => {
            write_review_command_result(writer, version, request_id, result).await
        }
        ReviewWorkflowCommandOutcome::ConflictingReuse { .. } => {
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

pub(super) async fn write_review_command_result<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    result: ReviewWorkflowCommandResult,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let message = match result {
        ReviewWorkflowCommandResult::TargetCreated { target } => {
            ServerMessage::ReviewTargetCreated {
                target_id: wire_uuid(target.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::RunStarted { run, pass } => ServerMessage::ReviewRunStarted {
            run_id: wire_uuid(run.into_uuid()),
            pass_id: wire_uuid(pass.into_uuid()),
        },
        ReviewWorkflowCommandResult::PassActivated { run, pass } => {
            ServerMessage::ReviewPassActivated {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::PassCompleted { run, pass, status } => {
            let state = match status {
                ReviewPassCompletionStatus::Succeeded => ReviewPassLifecycle::Succeeded,
                ReviewPassCompletionStatus::Failed => ReviewPassLifecycle::Failed,
                ReviewPassCompletionStatus::Blocked => ReviewPassLifecycle::Blocked,
                ReviewPassCompletionStatus::Cancelled => ReviewPassLifecycle::Cancelled,
            };
            ServerMessage::ReviewPassCompleted {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
                state,
            }
        }
        ReviewWorkflowCommandResult::FindingsRecorded {
            run,
            pass,
            finding_count,
        } => {
            let count = u64::try_from(finding_count)
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
            ServerMessage::ReviewFindingsRecorded {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
                finding_count: CanonicalU64::new(count),
            }
        }
        ReviewWorkflowCommandResult::FindingEventRecorded { finding, status } => {
            ServerMessage::ReviewFindingEventRecorded {
                finding_id: wire_uuid(finding.into_uuid()),
                status: wire_review_finding_status(status),
            }
        }
        ReviewWorkflowCommandResult::ExternalLinkReserved { link } => {
            ServerMessage::ReviewExternalLinkReserved {
                external_link_id: wire_uuid(link.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::ExternalLinkAttached {
            link,
            external_object,
        } => ServerMessage::ReviewExternalLinkAttached {
            external_link_id: wire_uuid(link.into_uuid()),
            external_object: external_object.into_string(),
        },
    };
    write_message(writer, version, request_id, message).await
}

pub(super) fn review_activation_was_applied(
    run: &ReviewRun,
    pass: &ReviewPass,
    turn: TurnId,
) -> bool {
    let reference = pass.reference();
    let run_retains_pass = match run.state() {
        ReviewRunState::Running { active_pass }
        | ReviewRunState::Succeeded {
            concluding_pass: active_pass,
        }
        | ReviewRunState::Failed {
            failed_pass: active_pass,
        }
        | ReviewRunState::Blocked {
            blocking_pass: active_pass,
        }
        | ReviewRunState::Cancelled {
            last_pass: Some(active_pass),
        } => active_pass == reference,
        ReviewRunState::Queued | ReviewRunState::Cancelled { last_pass: None } => false,
    };
    let pass_retains_turn = match pass.state() {
        ReviewPassState::Running { turn: retained }
        | ReviewPassState::Succeeded { turn: retained, .. }
        | ReviewPassState::Failed { turn: retained }
        | ReviewPassState::Blocked { turn: retained, .. }
        | ReviewPassState::Cancelled {
            turn: Some(retained),
        } => *retained == turn,
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => false,
    };
    run_retains_pass && pass_retains_turn
}

pub(super) fn historical_review_activation(
    current_run: &ReviewRun,
    current_pass: &ReviewPass,
    turn: TurnId,
) -> Option<(ReviewRun, ReviewPass)> {
    let mut run = ReviewRun::new(
        current_run.reference(),
        current_run.workflow(),
        current_run.policy(),
    );
    let pass = ReviewPass::try_new(
        current_pass.reference(),
        current_pass.kind(),
        &mut run,
        current_pass.session(),
        ReviewPassAcceptedInputEvidence::new(
            current_pass.accepted_input(),
            current_pass.session(),
            Some(current_pass.origin_turn()),
        ),
    )
    .ok()?;
    let pass = pass
        .transition(
            ReviewPassState::Running { turn },
            Some(ReviewPassTurnEvidence::new(
                turn,
                current_pass.session(),
                current_pass.accepted_input(),
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .ok()?;
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, current_run.policy());
    let run = run
        .transition(
            ReviewRunState::Running {
                active_pass: current_pass.reference(),
            },
            Some(pass_evidence),
        )
        .ok()?;
    Some((run, pass))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_activate_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::ActivatePass,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let turn_id = TurnId::from_uuid(turn_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id {
        return write_review_invalid(writer, version, request_id).await;
    }
    let lifecycle = match store.load_turn_lifecycle(turn_id).await {
        Ok(Some(lifecycle)) => lifecycle,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let Some(canonical_input) = lifecycle.accepted_input() else {
        return write_review_internal(writer, version, request_id).await;
    };
    if lifecycle.session() != current_pass.session()
        || canonical_input != current_pass.accepted_input()
        || matches!(lifecycle.state(), ReviewTurnLifecycleState::Queued)
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let evidence = ReviewPassTurnEvidence::new(
        turn_id,
        current_pass.session(),
        current_pass.accepted_input(),
        ReviewPassTurnOutcome::Active,
        None,
    );
    let policy = current_run.policy();
    let active_pass_state = ReviewPassState::Running { turn: turn_id };
    let active_run_state = ReviewRunState::Running {
        active_pass: current_pass.reference(),
    };
    let (run, pass) = if lifecycle.state() == ReviewTurnLifecycleState::Active
        && current_run.state() == ReviewRunState::Queued
        && current_pass.state() == &ReviewPassState::Queued
    {
        let Ok(pass) = current_pass.transition(active_pass_state, Some(evidence)) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        let pass_evidence = ReviewPassEvidence::from_pass(&pass, policy);
        let Ok(run) = current_run.transition(active_run_state, Some(pass_evidence)) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        (run, pass)
    } else if review_activation_was_applied(&current_run, &current_pass, turn_id) {
        let Some(activation) = historical_review_activation(&current_run, &current_pass, turn_id)
        else {
            return write_review_internal(writer, version, request_id).await;
        };
        activation
    } else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::ActivatePass { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_complete_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: Option<CanonicalUuid>,
    output_frontier_id: Option<CanonicalUuid>,
    outcome: ReviewPassTerminalOutcome,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::CompletePass,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id {
        return write_review_invalid(writer, version, request_id).await;
    }
    if matches!(outcome, ReviewPassTerminalOutcome::Succeeded)
        && current_pass.kind() == ReviewPassKind::ReadOnlyReview
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let completed = match (outcome, turn_id, output_frontier_id) {
        (ReviewPassTerminalOutcome::Cancelled, None, None) => {
            complete_queued_review_pass(current_run, current_pass)
        }
        (ReviewPassTerminalOutcome::Succeeded, Some(turn), Some(frontier)) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Succeeded {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                    output_frontier: ContextFrontierId::from_uuid(frontier.into_uuid()),
                    result: None,
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Failed, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Failed {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Blocked, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Blocked {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                    result: None,
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Cancelled, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Cancelled {
                    turn: Some(TurnId::from_uuid(turn.into_uuid())),
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        _ => return write_review_invalid(writer, version, request_id).await,
    };
    let Some((run, pass)) = completed.map(|(pass, run)| (run, pass)) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::CompletePass { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

pub(super) fn complete_queued_review_pass(
    current_run: ReviewRun,
    current_pass: ReviewPass,
) -> Option<(ReviewPass, ReviewRun)> {
    let next_pass = ReviewPassState::Cancelled { turn: None };
    let pass = if current_pass.state() == &next_pass {
        current_pass
    } else {
        current_pass.transition(next_pass, None).ok()?
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, current_run.policy());
    let next_run = ReviewRunState::Cancelled {
        last_pass: Some(pass.reference()),
    };
    let run = if current_run.state() == next_run {
        current_run
    } else {
        current_run.transition(next_run, Some(pass_evidence)).ok()?
    };
    Some((pass, run))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_record_review_findings<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: CanonicalUuid,
    inputs: Vec<ReviewFindingInput>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::RecordFindings,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_pass.kind() != ReviewPassKind::ReadOnlyReview
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let references = inputs
        .iter()
        .map(|input| {
            ReviewFindingRef::new(
                current_pass.reference(),
                ReviewFindingId::from_uuid(input.finding_id.into_uuid()),
            )
        })
        .collect::<Vec<_>>();
    let Ok(inventory) = ReviewProducedFindings::try_new(references) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let next = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(turn_id.into_uuid()),
        output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
        result: Some(ReviewPassResult::ProducedFindings(inventory)),
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let target = match store.load_target(pass_evidence.reference().target()).await {
        Ok(Some(target)) => target,
        Ok(None) => return write_review_internal(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let mut findings = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Some((finding_id, content)) = review_finding_content(input) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        let reference = ReviewFindingRef::new(
            pass_evidence.reference(),
            ReviewFindingId::from_uuid(finding_id.into_uuid()),
        );
        let Ok(proposal) = ReviewFindingProposal::try_new(
            reference,
            pass_evidence.clone(),
            run_evidence,
            &target,
            content,
        ) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        findings.push(ReviewFinding::new(proposal));
    }
    findings.sort_unstable_by_key(|finding| finding.proposal().reference());
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::RecordFindings {
            pass: pass_evidence,
            findings,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_record_review_disposition<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: Option<CanonicalUuid>,
    finding_id: CanonicalUuid,
    event_ordinal: CanonicalU64,
    event: WireReviewFindingEvent,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::RecordFindingEvent,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let finding_id = ReviewFindingId::from_uuid(finding_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_finding = match store.load_finding(finding_id).await {
        Ok(Some(finding)) => finding,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_finding.proposal().reference().target() != current_pass.reference().target()
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let Ok(ordinal_value) = u32::try_from(event_ordinal.value()) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(ordinal) = ReviewEventOrdinal::try_new(ordinal_value) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let (result_kind, event_kind, blocked) = match event {
        WireReviewFindingEvent::Accepted {} => (
            ReviewFindingEventResultKind::Accepted,
            ReviewFindingEventKind::Accepted,
            false,
        ),
        WireReviewFindingEvent::Rejected { reason } => {
            let Ok(reason) = ReviewText::try_new(reason) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Rejected {
                    reason: reason.clone(),
                },
                ReviewFindingEventKind::Rejected { reason },
                false,
            )
        }
        WireReviewFindingEvent::Duplicate {
            canonical_finding_id,
        } => {
            let referenced = match store
                .load_finding(ReviewFindingId::from_uuid(canonical_finding_id.into_uuid()))
                .await
            {
                Ok(Some(finding)) => finding,
                Ok(None) => return write_review_invalid(writer, version, request_id).await,
                Err(error) => {
                    return write_review_store_error(writer, version, request_id, error).await;
                }
            };
            let Some(canonical) = ReviewReferencedFindingEvidence::try_from_finding(&referenced)
            else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Duplicate { canonical },
                ReviewFindingEventKind::Duplicate { canonical },
                false,
            )
        }
        WireReviewFindingEvent::Superseded {
            successor_finding_id,
        } => {
            let referenced = match store
                .load_finding(ReviewFindingId::from_uuid(successor_finding_id.into_uuid()))
                .await
            {
                Ok(Some(finding)) => finding,
                Ok(None) => return write_review_invalid(writer, version, request_id).await,
                Err(error) => {
                    return write_review_store_error(writer, version, request_id, error).await;
                }
            };
            let Some(successor) = ReviewReferencedFindingEvidence::try_from_finding(&referenced)
            else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Superseded { successor },
                ReviewFindingEventKind::Superseded { successor },
                false,
            )
        }
        WireReviewFindingEvent::Stale {} => (
            ReviewFindingEventResultKind::Stale,
            ReviewFindingEventKind::Stale,
            false,
        ),
        WireReviewFindingEvent::Fixed {} => (
            ReviewFindingEventResultKind::Fixed,
            ReviewFindingEventKind::Fixed,
            false,
        ),
        WireReviewFindingEvent::BlockedWithReason {
            reason,
            external_link_id,
        } => {
            let Ok(reason) = ReviewText::try_new(reason) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            let link = match external_link_id {
                Some(link_id) => {
                    let link_id = ReviewExternalLinkId::from_uuid(link_id.into_uuid());
                    let link = match store.load_external_link(link_id).await {
                        Ok(Some(link)) => link,
                        Ok(None) => {
                            return write_review_invalid(writer, version, request_id).await;
                        }
                        Err(error) => {
                            return write_review_store_error(writer, version, request_id, error)
                                .await;
                        }
                    };
                    let Ok(reference) = ReviewFindingPendingExternalLinkRef::try_new(
                        current_finding.proposal().reference(),
                        &link,
                    ) else {
                        return write_review_invalid(writer, version, request_id).await;
                    };
                    Some(reference)
                }
                None => None,
            };
            (
                ReviewFindingEventResultKind::BlockedWithReason {
                    reason: reason.clone(),
                    link: link.as_ref().map(ReviewFindingPendingExternalLinkRef::link),
                },
                ReviewFindingEventKind::BlockedWithReason {
                    reason,
                    link: link.map(Box::new),
                },
                true,
            )
        }
    };
    let finding_reference = current_finding.proposal().reference();
    let result = ReviewFindingEventResult::new(finding_reference, ordinal, result_kind);
    let next = if blocked {
        if output_frontier_id.is_some() {
            return write_review_invalid(writer, version, request_id).await;
        }
        ReviewPassState::Blocked {
            turn: TurnId::from_uuid(turn_id.into_uuid()),
            result: Some(ReviewPassResult::FindingEvent(result)),
        }
    } else {
        let Some(output_frontier_id) = output_frontier_id else {
            return write_review_invalid(writer, version, request_id).await;
        };
        ReviewPassState::Succeeded {
            turn: TurnId::from_uuid(turn_id.into_uuid()),
            output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
            result: Some(ReviewPassResult::FindingEvent(result)),
        }
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let event = ReviewFindingEvent::new(
        finding_reference,
        ordinal,
        pass_evidence.reference(),
        pass_evidence.clone(),
        run_evidence,
        event_kind,
    );
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::RecordFindingEvent {
            pass: pass_evidence,
            event,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_reserve_review_external_link<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    external_link_id: CanonicalUuid,
    finding_id: CanonicalUuid,
    provider: String,
    object_kind: WireReviewExternalObjectKind,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::ReserveExternalLink,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    let finding = match store
        .load_finding(ReviewFindingId::from_uuid(finding_id.into_uuid()))
        .await
    {
        Ok(Some(finding)) => finding,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let association = ReviewExternalLinkAssociation::Finding(finding.proposal().reference());
    let target = match store.load_target(association.target()).await {
        Ok(Some(target)) => target,
        Ok(None) => return write_review_internal(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let object_kind = match object_kind {
        WireReviewExternalObjectKind::Review => ReviewExternalObjectKind::Review,
        WireReviewExternalObjectKind::ReviewThread => ReviewExternalObjectKind::ReviewThread,
        WireReviewExternalObjectKind::ReviewComment => ReviewExternalObjectKind::ReviewComment,
        WireReviewExternalObjectKind::ChangeRequestComment => {
            ReviewExternalObjectKind::ChangeRequestComment
        }
    };
    let Ok(provider) = ReviewKey::try_new(provider) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let link_id = ReviewExternalLinkId::from_uuid(external_link_id.into_uuid());
    let Ok(link) =
        ReviewExternalLink::try_reserve(link_id, association, provider, object_kind, &target)
    else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::ReserveExternalLink(link),
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_attach_review_external_link<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    external_link_id: CanonicalUuid,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: CanonicalUuid,
    external_object: String,
    event_ordinal: CanonicalU64,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::AttachExternalLink,
    )
    .await?
    {
        return Ok(());
    }
    let link_id = ReviewExternalLinkId::from_uuid(external_link_id.into_uuid());
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let link = match store.load_external_link(link_id).await {
        Ok(Some(link)) => link,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let ReviewExternalLinkAssociation::Finding(finding_reference) = link.association() else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_pass.reference().target() != finding_reference.target()
        || current_pass.kind() != ReviewPassKind::Publish
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let Ok(ordinal_value) = u32::try_from(event_ordinal.value()) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(ordinal) = ReviewEventOrdinal::try_new(ordinal_value) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(external_object) = ReviewKey::try_new(external_object) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let result = ReviewExternalLinkAttachmentResult::new(
        link_id,
        external_object.clone(),
        Some(ReviewFindingEventResult::new(
            finding_reference,
            ordinal,
            ReviewFindingEventResultKind::Posted { link: link_id },
        )),
    );
    let next = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(turn_id.into_uuid()),
        output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
        result: Some(ReviewPassResult::ExternalLinkAttachment(result)),
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let attachment = ReviewExternalLinkAttachment::new(
        link_id,
        pass_evidence.reference(),
        pass_evidence,
        run_evidence,
        external_object.clone(),
    );
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::AttachExternalLink {
            link: link_id,
            attachment,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

pub(super) async fn handle_read_review_target<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    target_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_target(ReviewTargetId::from_uuid(target_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(target)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ReviewTarget {
                    target: review_target_snapshot(&target),
                },
            )
            .await
        }
        Ok(None) => write_review_not_found(writer, version, request_id).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

pub(super) async fn handle_read_review_run<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_run_with_pass(ReviewRunId::from_uuid(run_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    let (run, pass) = match loaded {
        Ok(Some(aggregate)) => aggregate,
        Ok(None) => return write_review_not_found(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let pass = pass.as_ref().map(review_pass_snapshot);
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewRun {
            run: review_run_snapshot(&run),
            pass,
        },
    )
    .await
}

pub(super) async fn handle_read_review_finding<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    finding_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_finding(ReviewFindingId::from_uuid(finding_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(finding)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ReviewFinding {
                    finding: review_finding_snapshot(&finding),
                },
            )
            .await
        }
        Ok(None) => write_review_not_found(writer, version, request_id).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

/// Loads one run's complete finding page, or `None` when the run is absent.
///
/// The existence check and the graph walk are separate database phases, so they
/// belong to the same reader admission: splitting them would let a listing hold
/// pool capacity it never reserved.
pub(super) async fn load_review_findings_page(
    store: &ReviewWorkflowStore,
    run_id: ReviewRunId,
) -> Result<Option<Vec<ReviewFinding>>, ReviewWorkflowStoreError> {
    if store.load_run(run_id).await?.is_none() {
        return Ok(None);
    }
    store.list_findings(run_id).await.map(Some)
}

pub(super) async fn handle_list_review_findings<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = load_review_findings_page(&store, run_id).await;
    drop(snapshot_permit);
    let findings = match loaded {
        Ok(Some(findings)) => findings,
        Ok(None) => return write_review_not_found(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewFindingsStart {
            run_id: wire_uuid(run_id.into_uuid()),
        },
    )
    .await?;
    for finding in &findings {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ReviewFindingItem {
                finding: review_finding_snapshot(finding),
            },
        )
        .await?;
    }
    let Ok(finding_count) = u64::try_from(findings.len()) else {
        return Err(ProcessConnectionError::EncodeInvariant);
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewFindingsEnd {
            finding_count: CanonicalU64::new(finding_count),
        },
    )
    .await
}

pub(super) fn review_target_snapshot(target: &ReviewTarget) -> ReviewTargetSnapshot {
    let subject = match target.subject() {
        ReviewTargetSubject::ChangeRequest(number) => WireReviewTargetSubject::ChangeRequest {
            number: CanonicalU64::new(number.get()),
        },
        ReviewTargetSubject::Commit => WireReviewTargetSubject::Commit {},
    };
    ReviewTargetSnapshot {
        target_id: wire_uuid(target.id().into_uuid()),
        provider: target.provider().as_str().to_owned(),
        repository: target.repository().as_str().to_owned(),
        subject,
        head_revision: target.head_revision().as_str().to_owned(),
        base_revision: target
            .base_revision()
            .map(|revision| revision.as_str().to_owned()),
        stack_parent_target_id: target
            .stack_parent()
            .map(|parent| wire_uuid(parent.target().into_uuid())),
    }
}

const fn wire_review_workflow(workflow: ReviewWorkflowKind) -> WireReviewWorkflow {
    match workflow {
        ReviewWorkflowKind::ImportExternalContext => WireReviewWorkflow::ImportExternalContext,
        ReviewWorkflowKind::ReadOnlyReview => WireReviewWorkflow::ReadOnlyReview,
        ReviewWorkflowKind::JudgeFindings => WireReviewWorkflow::JudgeFindings,
        ReviewWorkflowKind::DedupeFindings => WireReviewWorkflow::DedupeFindings,
        ReviewWorkflowKind::PublishReview => WireReviewWorkflow::PublishReview,
        ReviewWorkflowKind::FixFindings => WireReviewWorkflow::FixFindings,
        ReviewWorkflowKind::PropagateStack => WireReviewWorkflow::PropagateStack,
    }
}

const fn wire_review_pass_kind(kind: ReviewPassKind) -> signalbox_process_protocol::ReviewPassKind {
    match kind {
        ReviewPassKind::ImportExternalContext => {
            signalbox_process_protocol::ReviewPassKind::ImportExternalContext
        }
        ReviewPassKind::ReadOnlyReview => {
            signalbox_process_protocol::ReviewPassKind::ReadOnlyReview
        }
        ReviewPassKind::Judge => signalbox_process_protocol::ReviewPassKind::Judge,
        ReviewPassKind::Dedupe => signalbox_process_protocol::ReviewPassKind::Dedupe,
        ReviewPassKind::Publish => signalbox_process_protocol::ReviewPassKind::Publish,
        ReviewPassKind::Fix => signalbox_process_protocol::ReviewPassKind::Fix,
        ReviewPassKind::PropagateStack => {
            signalbox_process_protocol::ReviewPassKind::PropagateStack
        }
    }
}

pub(super) fn review_run_snapshot(run: &ReviewRun) -> ReviewRunSnapshot {
    let state = match run.state() {
        ReviewRunState::Queued => ReviewRunLifecycle::Queued,
        ReviewRunState::Running { .. } => ReviewRunLifecycle::Running,
        ReviewRunState::Succeeded { .. } => ReviewRunLifecycle::Succeeded,
        ReviewRunState::Failed { .. } => ReviewRunLifecycle::Failed,
        ReviewRunState::Blocked { .. } => ReviewRunLifecycle::Blocked,
        ReviewRunState::Cancelled { .. } => ReviewRunLifecycle::Cancelled,
    };
    let policy = run.policy();
    ReviewRunSnapshot {
        target_id: wire_uuid(run.reference().target().into_uuid()),
        run_id: wire_uuid(run.reference().run().into_uuid()),
        workflow: wire_review_workflow(run.workflow()),
        policy_version: CanonicalU64::new(u64::from(policy.version().get())),
        minimum_judge_confidence: CanonicalU64::new(u64::from(
            policy.minimum_judge_confidence().basis_points(),
        )),
        minimum_publication_confidence: CanonicalU64::new(u64::from(
            policy.minimum_publication_confidence().basis_points(),
        )),
        state,
        pass_id: run
            .recorded_pass()
            .map(|reference| wire_uuid(reference.pass().into_uuid())),
    }
}

pub(super) fn review_pass_snapshot(pass: &ReviewPass) -> ReviewPassSnapshot {
    let (state, turn, output_frontier) = match pass.state() {
        ReviewPassState::Queued => (ReviewPassLifecycle::Queued, None, None),
        ReviewPassState::Running { turn } => (
            ReviewPassLifecycle::Running,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => (
            ReviewPassLifecycle::Succeeded,
            Some(wire_uuid(turn.into_uuid())),
            Some(wire_uuid(output_frontier.into_uuid())),
        ),
        ReviewPassState::Failed { turn } => (
            ReviewPassLifecycle::Failed,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Blocked { turn, .. } => (
            ReviewPassLifecycle::Blocked,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Cancelled { turn } => (
            ReviewPassLifecycle::Cancelled,
            turn.map(|turn| wire_uuid(turn.into_uuid())),
            None,
        ),
    };
    ReviewPassSnapshot {
        pass_id: wire_uuid(pass.reference().pass().into_uuid()),
        run_id: wire_uuid(pass.reference().run().run().into_uuid()),
        target_id: wire_uuid(pass.reference().target().into_uuid()),
        kind: wire_review_pass_kind(pass.kind()),
        session_id: wire_uuid(pass.session().into_uuid()),
        accepted_input_id: wire_uuid(pass.accepted_input().into_uuid()),
        origin_turn_id: wire_uuid(pass.origin_turn().into_uuid()),
        state,
        turn_id: turn,
        output_frontier_id: output_frontier,
    }
}

const fn wire_review_finding_status(
    status: signalbox_domain::ReviewFindingStatus,
) -> WireReviewFindingStatus {
    match status {
        signalbox_domain::ReviewFindingStatus::Open => WireReviewFindingStatus::Open,
        signalbox_domain::ReviewFindingStatus::Accepted => WireReviewFindingStatus::Accepted,
        signalbox_domain::ReviewFindingStatus::Rejected => WireReviewFindingStatus::Rejected,
        signalbox_domain::ReviewFindingStatus::Duplicate => WireReviewFindingStatus::Duplicate,
        signalbox_domain::ReviewFindingStatus::Superseded => WireReviewFindingStatus::Superseded,
        signalbox_domain::ReviewFindingStatus::Stale => WireReviewFindingStatus::Stale,
        signalbox_domain::ReviewFindingStatus::Posted => WireReviewFindingStatus::Posted,
        signalbox_domain::ReviewFindingStatus::Fixed => WireReviewFindingStatus::Fixed,
        signalbox_domain::ReviewFindingStatus::BlockedWithReason => {
            WireReviewFindingStatus::BlockedWithReason
        }
    }
}

pub(super) fn review_finding_snapshot(finding: &ReviewFinding) -> ReviewFindingSnapshot {
    let reference = finding.proposal().reference();
    let content = finding.proposal().content();
    let location = content.location();
    let line_range = location.line_range();
    let diff_side = location.diff_side().map(|side| match side {
        ReviewFindingDiffSide::Left => WireReviewDiffSide::Left,
        ReviewFindingDiffSide::Right => WireReviewDiffSide::Right,
    });
    let severity = match content.severity() {
        ReviewFindingSeverity::Info => WireReviewSeverity::Info,
        ReviewFindingSeverity::Low => WireReviewSeverity::Low,
        ReviewFindingSeverity::Medium => WireReviewSeverity::Medium,
        ReviewFindingSeverity::High => WireReviewSeverity::High,
        ReviewFindingSeverity::Critical => WireReviewSeverity::Critical,
    };
    let event_count = u64::try_from(finding.events().len()).unwrap_or(u64::MAX);
    ReviewFindingSnapshot {
        target_id: wire_uuid(reference.target().into_uuid()),
        run_id: wire_uuid(reference.run().run().into_uuid()),
        producing_pass_id: wire_uuid(reference.pass().pass().into_uuid()),
        finding: ReviewFindingInput {
            finding_id: wire_uuid(reference.finding().into_uuid()),
            file_path: location.file_path().as_str().to_owned(),
            line_start: line_range.map(|range| CanonicalU64::new(u64::from(range.start()))),
            line_end: line_range.map(|range| CanonicalU64::new(u64::from(range.end()))),
            diff_side,
            title: content.title().as_str().to_owned(),
            body: content.body().as_str().to_owned(),
            severity,
            is_real_confidence: CanonicalU64::new(u64::from(
                content.is_real_confidence().basis_points(),
            )),
            severity_label_confidence: CanonicalU64::new(u64::from(
                content.severity_label_confidence().basis_points(),
            )),
            category: content.category().as_str().to_owned(),
            recommended_fix: content
                .recommended_fix()
                .map(|text| text.as_str().to_owned()),
        },
        status: wire_review_finding_status(finding.status()),
        event_count: CanonicalU64::new(event_count),
    }
}

pub(super) async fn complete_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    current_run: signalbox_domain::ReviewRun,
    current_pass: ReviewPass,
    next: ReviewPassState,
) -> Result<Option<(ReviewPass, ReviewRun)>, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let turn = match &next {
        ReviewPassState::Succeeded { turn, .. }
        | ReviewPassState::Failed { turn }
        | ReviewPassState::Blocked { turn, .. }
        | ReviewPassState::Cancelled { turn: Some(turn) } => *turn,
        ReviewPassState::Queued
        | ReviewPassState::Running { .. }
        | ReviewPassState::Cancelled { turn: None } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let store = ReviewWorkflowStore::new(pool.clone());
    let lifecycle = match store.load_turn_lifecycle(turn).await {
        Ok(Some(lifecycle)) => lifecycle,
        Ok(None) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
        Err(error) => {
            return write_review_store_error(writer, version, request_id, error)
                .await
                .map(|()| None);
        }
    };
    let Some(canonical_input) = lifecycle.accepted_input() else {
        return write_review_internal(writer, version, request_id)
            .await
            .map(|()| None);
    };
    let frontier = lifecycle.terminal_frontier();
    let expected_frontier = match &next {
        ReviewPassState::Succeeded {
            output_frontier, ..
        } => Some(*output_frontier),
        _ => frontier,
    };
    let ReviewTurnLifecycleState::Terminal(turn_outcome) = lifecycle.state() else {
        return write_review_invalid(writer, version, request_id)
            .await
            .map(|()| None);
    };
    if lifecycle.session() != current_pass.session()
        || canonical_input != current_pass.accepted_input()
        || frontier != expected_frontier
    {
        return write_review_invalid(writer, version, request_id)
            .await
            .map(|()| None);
    }
    let evidence = ReviewPassTurnEvidence::new(
        turn,
        current_pass.session(),
        current_pass.accepted_input(),
        turn_outcome,
        frontier,
    );
    let policy = current_run.policy();
    let pass_reference = current_pass.reference();
    let next_run = match &next {
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: pass_reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: pass_reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: pass_reference,
        },
        ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
            last_pass: Some(pass_reference),
        },
        ReviewPassState::Queued | ReviewPassState::Running { .. } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    if current_pass.state() == &next {
        if current_run.state() != next_run {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
        return Ok(Some((current_pass, current_run)));
    }
    let pass = match current_pass.transition(next, Some(evidence)) {
        Ok(pass) => pass,
        Err(_) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, policy);
    let run = match current_run.transition(next_run, Some(pass_evidence.clone())) {
        Ok(run) => run,
        Err(_) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    Ok(Some((pass, run)))
}

pub(super) fn review_finding_content(
    input: ReviewFindingInput,
) -> Option<(CanonicalUuid, ReviewFindingContent)> {
    let line_range = match (input.line_start, input.line_end) {
        (None, None) => None,
        (Some(start), Some(end)) => Some(
            ReviewLineRange::try_new(
                u32::try_from(start.value()).ok()?,
                u32::try_from(end.value()).ok()?,
            )
            .ok()?,
        ),
        (Some(_), None) | (None, Some(_)) => return None,
    };
    let diff_side = input.diff_side.map(|side| match side {
        WireReviewDiffSide::Left => ReviewFindingDiffSide::Left,
        WireReviewDiffSide::Right => ReviewFindingDiffSide::Right,
    });
    let location = ReviewFindingLocation::new(
        ReviewKey::try_new(input.file_path).ok()?,
        line_range,
        diff_side,
    );
    let severity = match input.severity {
        WireReviewSeverity::Info => ReviewFindingSeverity::Info,
        WireReviewSeverity::Low => ReviewFindingSeverity::Low,
        WireReviewSeverity::Medium => ReviewFindingSeverity::Medium,
        WireReviewSeverity::High => ReviewFindingSeverity::High,
        WireReviewSeverity::Critical => ReviewFindingSeverity::Critical,
    };
    let is_real_confidence = ReviewConfidence::try_from_basis_points(
        u16::try_from(input.is_real_confidence.value()).ok()?,
    )
    .ok()?;
    let severity_label_confidence = ReviewConfidence::try_from_basis_points(
        u16::try_from(input.severity_label_confidence.value()).ok()?,
    )
    .ok()?;
    Some((
        input.finding_id,
        ReviewFindingContent::new(
            location,
            ReviewText::try_new(input.title).ok()?,
            ReviewText::try_new(input.body).ok()?,
            severity,
            ReviewFindingConfidenceAxes::new(is_real_confidence, severity_label_confidence),
            ReviewKey::try_new(input.category).ok()?,
            input
                .recommended_fix
                .map(ReviewText::try_new)
                .transpose()
                .ok()?,
        ),
    ))
}

pub(super) async fn write_review_invalid<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::InvalidRequest),
    )
    .await
}

pub(super) async fn write_review_internal<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        internal_protocol_error(None, InternalDiagnostic::ReviewWorkflowProjectionCorruption),
    )
    .await
}

pub(super) async fn write_review_unavailable<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    commit_ambiguous: bool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::mutation_unavailable(commit_ambiguous),
    )
    .await
}

pub(super) async fn write_review_not_found<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::NotFound),
    )
    .await
}

pub(super) async fn write_review_store_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ReviewWorkflowStoreError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        ReviewWorkflowStoreError::Database(_) => {
            write_review_unavailable(writer, version, request_id, false).await
        }
        ReviewWorkflowStoreError::CommitAmbiguous(_) => {
            write_review_unavailable(writer, version, request_id, true).await
        }
        ReviewWorkflowStoreError::Corruption(_) => {
            write_review_internal(writer, version, request_id).await
        }
        ReviewWorkflowStoreError::InvalidInsertion(_)
        | ReviewWorkflowStoreError::InvalidTransition(_)
        | ReviewWorkflowStoreError::NonAtomicPassResult
        | ReviewWorkflowStoreError::IncompleteFindingInventory
        | ReviewWorkflowStoreError::IncompletePublicationReconciliation
        | ReviewWorkflowStoreError::ReservationConflict(_) => {
            write_review_invalid(writer, version, request_id).await
        }
    }
}
