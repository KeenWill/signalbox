use super::attachment::{
    load_runner_recovery_yielded_attempt, persist_runner_recovery_interrupt_effect,
    prepare_attachment_authority_rejection, prospective_attachment_frontier_exceeds_bound,
    session_has_attachment_parts, supersede_automatic_reconciliation,
    terminalize_retryable_runner_recovery_attempt,
};
use super::decode::map_tool_loop_error;
use super::prepare::{existing_outcome, prepare_against_locked_state, require_recorded};
use super::write::{insert_prepared_command, insert_prepared_effects, settle_injection_receipt};
use super::{
    PreparedAgainstLockedState, STORAGE_VERSION, SubmitInputCorruption, SubmitInputHandlingOutcome,
    SubmitInputRepositoryError, TransactionDecision, inspect_registry,
    parent_termination_kind_to_str,
};
use crate::command_registry::{CommandKind, SUBMIT_INPUT_KIND};
use crate::mapping::{durable_command_id_to_uuid, session_id_to_uuid};
use crate::model_execution::{
    attach_interrupt_reclassification_candidates,
    attach_interrupt_reclassification_candidates_for_activated,
    attach_interrupt_reclassification_candidates_for_active,
    attach_recovery_interrupt_reclassification_candidates,
    attach_recovery_interrupt_reclassification_candidates_for_activated,
    load_delegated_runner_recovery_for_interrupt, lock_delegated_child_endpoint_sessions,
    persist_stop_requested, persist_terminal_outcome, persist_tool_reconciliation_required,
    require_live_execution_for_restart,
};
use crate::tool_loop::{
    load_active_batch_from_connection, load_optional_foreground_delegation_outcome,
    load_recovery_batch_by_attempt, load_runner_recovery_batch_without_attempt,
    load_runner_recovery_cancellation_batch, load_runner_recovery_source_snapshot,
    persist_ended_attempt,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputSchedulingProjection, AcceptedInputTurnSchedulingStatus,
    CancelledModelCallTurnIdentities, CommandPrincipal, DeliveryRequest,
    DescendantTerminationScope, DurableCommandId, FrozenAliasDefinition, IssuedOperationRef,
    ModelAlias, ModelCallInterruptOutcome, ModelCallTerminalOutcome, ModelCapabilityCatalog,
    ParentTerminationKind, SubmitInput, SubmitInputAppliedResult, SubmitInputResult, TurnAttemptId,
    TurnId,
};
use sqlx::PgConnection;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_in_transaction<
    NextTurn,
    NextToolCancellation,
    NextClosureDecision,
    NextClosureAttempt,
>(
    connection: &mut PgConnection,
    command: SubmitInput,
    principal: CommandPrincipal,
    cascade_root_kind: ParentTerminationKind,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
    cancellation_identities: CancelledModelCallTurnIdentities,
    mut next_reclassified_turn: NextTurn,
    mut next_tool_cancellation: NextToolCancellation,
    mut next_closure_decision: NextClosureDecision,
    mut next_closure_attempt: NextClosureAttempt,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    model_capabilities: Option<&ModelCapabilityCatalog>,
    attachment_maximum_bytes: Option<u64>,
) -> Result<TransactionDecision, SubmitInputRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    NextToolCancellation: FnMut(
            &[signalbox_domain::ToolRequestId],
        ) -> (
            Vec<signalbox_domain::SemanticTranscriptEntryId>,
            signalbox_domain::ContextFrontierId,
        ) + Send,
    NextClosureDecision: FnMut() -> DurableCommandId + Send,
    NextClosureAttempt: FnMut() -> TurnAttemptId + Send,
{
    let command_id = command.command_id();
    match inspect_registry(connection, command_id).await? {
        Some(CommandKind::SubmitInput) => {
            return Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            )));
        }
        Some(
            CommandKind::CreateSession
            | CommandKind::CreateSessionFromImportedFrontier
            | CommandKind::ReplaceSessionDefaults
            | CommandKind::ReplaceSessionMetadata
            | CommandKind::DecideToolRequest
            | CommandKind::OverrideDeniedToolRequest
            | CommandKind::ReviewWorkflow
            | CommandKind::ReviewOrchestration
            | CommandKind::CompactSession
            | CommandKind::Goal
            | CommandKind::UpdateSessionPlacement
            | CommandKind::RegisterWorkspace
            | CommandKind::MintGitRemote
            | CommandKind::WithdrawGitRemote
            | CommandKind::ProvisionOauthCredential
            | CommandKind::ReprovisionOauthCredential
            | CommandKind::DeleteOauthCredential
            | CommandKind::ReplaceLostRunner
            | CommandKind::AbandonLostRunner
            | CommandKind::PromotePendingRunner
            | CommandKind::SessionLifecycle,
        ) => {
            return Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            ));
        }
        None => {}
    }

    let issuer = crate::command_registry::issuer_columns(principal);
    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;

    if !claimed {
        return match inspect_registry(connection, command_id).await? {
            Some(CommandKind::SubmitInput) => Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            ))),
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::ProvisionOauthCredential
                | CommandKind::ReprovisionOauthCredential
                | CommandKind::DeleteOauthCredential
                | CommandKind::ReplaceLostRunner
                | CommandKind::AbandonLostRunner
                | CommandKind::PromotePendingRunner
                | CommandKind::SessionLifecycle,
            ) => Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            )),
            None => Err(SubmitInputCorruption::Inconsistent("winner claim disappeared").into()),
        };
    }

    if let Some(prepared) =
        prepare_attachment_authority_rejection(connection, &command, attachment_maximum_bytes)
            .await?
    {
        let recorded = prepared.result().clone();
        insert_prepared_command(connection, &prepared).await?;
        settle_injection_receipt(connection, &prepared).await?;
        return Ok(TransactionDecision::Commit(
            SubmitInputHandlingOutcome::Recorded(recorded),
        ));
    }

    let frontier_command = command.clone();
    if attachment_maximum_bytes.is_some() {
        sqlx::query("SAVEPOINT submit_input_attachment_frontier")
            .execute(&mut *connection)
            .await?;
    }

    if matches!(
        command.delivery(),
        DeliveryRequest::Interrupt {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        }
    ) {
        sqlx::query(crate::lock_inventory::DELEGATION_TERMINATION_SESSION_FRONTIER)
            .bind(session_id_to_uuid(command.session()))
            .bind(parent_termination_kind_to_str(cascade_root_kind))
            .execute(&mut *connection)
            .await?;
    }

    lock_delegated_child_endpoint_sessions(connection, command.session()).await?;
    let PreparedAgainstLockedState {
        prepared,
        scheduling,
        settles_closure,
    } = prepare_against_locked_state(
        connection,
        command,
        principal,
        accepted_input,
        turn,
        &mut next_closure_decision,
        &mut next_closure_attempt,
        select_definition,
        model_capabilities,
    )
    .await?;
    if settles_closure && matches!(prepared.result(), SubmitInputResult::Rejected(_)) {
        return Ok(TransactionDecision::Rollback(
            SubmitInputHandlingOutcome::Recorded(prepared.result().clone()),
        ));
    }
    let prior_queued_inputs = scheduling
        .as_ref()
        .map(|scheduling| {
            scheduling
                .turns()
                .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
                .map(|turn| turn.accepted_input().id())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let recorded = prepared.result().clone();
    let interrupt = match prepared.result() {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) => {
            origin.applied_interrupt().copied()
        }
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        | SubmitInputResult::Rejected(_) => None,
    };
    insert_prepared_command(connection, &prepared).await?;
    sqlx::query("SELECT materialize_session_delegation_termination_cascade($1, $2)")
        .bind(durable_command_id_to_uuid(command_id))
        .bind(parent_termination_kind_to_str(cascade_root_kind))
        .execute(&mut *connection)
        .await?;
    let interrupt_outcome = if let Some(interrupt) = interrupt {
        let runner_recovery_source_snapshot = load_runner_recovery_source_snapshot(
            connection,
            interrupt.session(),
            interrupt.proof().predecessor(),
        )
        .await
        .map_err(map_tool_loop_error)?;
        let active_tool_batch = if runner_recovery_source_snapshot.is_none() {
            load_active_batch_from_connection(
                connection,
                interrupt.session(),
                interrupt.proof().predecessor(),
            )
            .await
            .map_err(map_tool_loop_error)?
        } else {
            None
        };
        let executing_tool_batch = active_tool_batch.clone().filter(|batch| {
            matches!(
                batch.phase(),
                signalbox_domain::ToolBatchPhase::Executing { .. }
                    | signalbox_domain::ToolBatchPhase::AwaitingChild { .. }
            )
        });
        if let Some(mut batch) = executing_tool_batch {
            if let Some(current) =
                batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => {
                            Some(current.clone())
                        }
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(_)) | None => None,
                    })
            {
                if current.state() == signalbox_domain::CurrentToolAttemptState::InFlight {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "in-flight tool attempt escaped the dispatch gate",
                    )
                    .into());
                }
                let ended = match current.classify_crash_loss() {
                    signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(ended) => ended,
                    signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(_) => {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "prepared tool attempt classified ambiguous",
                        )
                        .into());
                    }
                };
                persist_ended_attempt(connection, &ended)
                    .await
                    .map_err(map_tool_loop_error)?;
                batch = load_active_batch_from_connection(
                    connection,
                    interrupt.session(),
                    batch.turn(),
                )
                .await
                .map_err(map_tool_loop_error)?
                .ok_or(SubmitInputCorruption::Missing("closed tool batch"))?;
            }
            let request_ids = batch
                .requests()
                .iter()
                .map(signalbox_domain::ToolRequest::id)
                .collect::<Vec<_>>();
            let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
            let child_wait =
                batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(attempt)) => {
                            match attempt.end() {
                                signalbox_domain::ToolAttemptEnd::AwaitingChild {
                                    spawning_request,
                                    child,
                                } => Some((request.id(), *spawning_request, *child)),
                                _ => None,
                            }
                        }
                        _ => None,
                    });
            let projection = match child_wait {
                Some((awaiting_request, spawning_request, child)) => batch
                    .prepare_delegation_cancellation_projection(
                        result_entries,
                        result_frontier,
                        load_optional_foreground_delegation_outcome(
                            connection,
                            interrupt.session(),
                            awaiting_request,
                            spawning_request,
                            child,
                        )
                        .await
                        .map_err(map_tool_loop_error)?,
                    ),
                None => batch.prepare_cancellation_projection(result_entries, result_frontier),
            }
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent(
                    "executing tool batch cannot project cancellation",
                )
            })?;
            // The scheduling projection is built from `queued_input_origin`, so
            // it carries an active turn only for an accepted-input origin. A
            // delegation-origin active turn is absent from it and must be
            // reconstituted through the delegated live-turn loader, exactly as
            // the recovery arm below decides.
            let projected_active_turn = scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution);
            if let Some(active_turn) = projected_active_turn {
                let Some(scheduling) = scheduling else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "tool interrupt scheduling projection",
                    )
                    .into());
                };
                let identities = attach_interrupt_reclassification_candidates_for_active(
                    cancellation_identities,
                    &active_turn,
                    &mut next_reclassified_turn,
                )
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent(
                        "tool interrupt reclassification candidates",
                    )
                })?;
                Some(ModelCallInterruptOutcome::Cancelled(
                    scheduling
                        .apply_interrupt_to_tool_batch(batch, projection, interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt cannot close executing tool batch",
                            )
                        })?,
                ))
            } else {
                let execution =
                    require_live_execution_for_restart(connection, interrupt.session()).await?;
                let identities = attach_interrupt_reclassification_candidates(
                    cancellation_identities,
                    &execution,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::Cancelled(
                    execution
                        .apply_interrupt_to_tool_batch(interrupt, projection, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt cannot close executing tool batch",
                            )
                        })?,
                ))
            }
        } else {
            let recovery_operation = scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution)
                .and_then(|active| match active.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision {
                        ambiguous_operations,
                        applied_interrupt: None,
                    } if ambiguous_operations.operation_count() == 1 => {
                        ambiguous_operations.iter().next()
                    }
                    signalbox_domain::ActiveTurnPhase::Running { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingApproval { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingChild { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. } => None,
                });
            if let Some(IssuedOperationRef::ToolAttempt(recovery_attempt)) = recovery_operation {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks active scheduling state",
                ))?;
                let batch = load_recovery_batch_by_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    recovery_attempt,
                )
                .await
                .map_err(map_tool_loop_error)?;
                let wait = batch
                    .awaiting_recovery()
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "tool recovery wait evidence",
                    ))?;
                let tool_attempt = batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(attempt))
                            if attempt.attempt() == recovery_attempt =>
                        {
                            Some(attempt.clone())
                        }
                        Some(signalbox_domain::ReconstitutedToolAttempt::Current(_))
                        | Some(signalbox_domain::ReconstitutedToolAttempt::Ended(_))
                        | None => None,
                    })
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "ambiguous tool attempt evidence",
                    ))?;
                let request_ids = batch
                    .requests()
                    .iter()
                    .map(signalbox_domain::ToolRequest::id)
                    .collect::<Vec<_>>();
                let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                let result_projection = batch
                    .prepare_reconciliation_projection(result_entries, result_frontier)
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "tool recovery batch cannot materialize terminal results",
                        )
                    })?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks active turn execution",
                    ),
                )?;
                let identities = attach_recovery_interrupt_reclassification_candidates(
                    signalbox_domain::AmbiguousModelCallTurnIdentities::new(result_frontier),
                    &active_turn,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::ToolReconciliationRequired(
                    scheduling
                        .apply_interrupt_to_tool_recovery(
                            wait,
                            tool_attempt,
                            result_projection,
                            interrupt,
                            identities,
                        )
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match tool recovery wait",
                            )
                        })?,
                ))
            } else if matches!(recovery_operation, Some(IssuedOperationRef::ModelCall(_))) {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks active scheduling state",
                ))?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks active turn execution",
                    ),
                )?;
                let identities = attach_recovery_interrupt_reclassification_candidates(
                    cancellation_identities.into_ambiguous(),
                    &active_turn,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::ReconciliationRequired(
                    scheduling
                        .apply_interrupt_to_model_call_recovery(interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match model-call recovery wait",
                            )
                        })?,
                ))
            } else if scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution)
                .is_some_and(|active| {
                    matches!(
                        active.phase(),
                        signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                    )
                })
            {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks runner recovery scheduling state",
                ))?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks runner recovery active turn",
                    ),
                )?;
                let source_snapshot = runner_recovery_source_snapshot.clone().ok_or(
                    SubmitInputCorruption::Missing("runner recovery source frontier"),
                )?;
                let source_frontier = source_snapshot.frontier().snapshot();
                let command = interrupt.proof().command();
                let yielded_attempt = load_runner_recovery_yielded_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                )
                .await?;
                let interrupted_tool_attempt = match active_turn.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery {
                        optional_tool_attempt,
                        ..
                    } => *optional_tool_attempt,
                    _ => None,
                };
                let outcome = if let Some(recovery_attempt) = interrupted_tool_attempt {
                    let preserves_ambiguity = terminalize_retryable_runner_recovery_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        recovery_attempt,
                    )
                    .await?;
                    let batch = if preserves_ambiguity {
                        load_recovery_batch_by_attempt(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            recovery_attempt,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                    } else {
                        load_runner_recovery_cancellation_batch(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            yielded_attempt,
                            Some(recovery_attempt),
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing(
                            "runner retryable tool recovery batch",
                        ))?
                    };
                    let request_ids = batch
                        .requests()
                        .iter()
                        .map(signalbox_domain::ToolRequest::id)
                        .collect::<Vec<_>>();
                    let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                    if !preserves_ambiguity {
                        let result_projection = batch
                            .prepare_cancellation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "runner retryable recovery batch cannot close",
                                )
                            })?;
                        let identities = attach_interrupt_reclassification_candidates_for_active(
                            cancellation_identities,
                            &active_turn,
                            &mut next_reclassified_turn,
                        )
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "runner retryable recovery interrupt candidates",
                            )
                        })?;
                        ModelCallInterruptOutcome::Cancelled(
                            scheduling
                                .apply_interrupt_to_retryable_runner_tool_recovery(
                                    batch,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "applied interrupt does not match retryable runner wait",
                                    )
                                })?,
                        )
                    } else {
                        let wait = batch.awaiting_recovery().ok_or(
                            SubmitInputCorruption::Inconsistent(
                                "runner tool recovery wait evidence",
                            ),
                        )?;
                        let tool_attempt = batch
                            .requests()
                            .iter()
                            .find_map(|request| match batch.attempt(request.id()) {
                                Some(signalbox_domain::ReconstitutedToolAttempt::Ended(
                                    attempt,
                                )) if attempt.attempt() == recovery_attempt => {
                                    Some(attempt.clone())
                                }
                                _ => None,
                            })
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner ambiguous tool attempt evidence",
                            ))?;
                        let result_projection = batch
                            .prepare_reconciliation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "runner recovery batch cannot preserve ambiguity",
                                )
                            })?;
                        let identities = attach_recovery_interrupt_reclassification_candidates(
                            signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                                result_frontier,
                            ),
                            &active_turn,
                            &mut next_reclassified_turn,
                        )?;
                        ModelCallInterruptOutcome::ToolReconciliationRequired(
                        scheduling
                            .apply_interrupt_to_runner_tool_recovery(
                                wait,
                                tool_attempt,
                                yielded_attempt,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match runner tool recovery wait",
                                )
                            })?,
                    )
                    }
                } else {
                    let result_projection = match load_runner_recovery_batch_without_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        yielded_attempt,
                    )
                    .await
                    .map_err(map_tool_loop_error)?
                    {
                        Some(batch) => {
                            let request_ids = batch
                                .requests()
                                .iter()
                                .map(signalbox_domain::ToolRequest::id)
                                .collect::<Vec<_>>();
                            let (result_entries, result_frontier) =
                                next_tool_cancellation(&request_ids);
                            Some(
                                batch
                                    .prepare_cancellation_projection(
                                        result_entries,
                                        result_frontier,
                                    )
                                    .map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "runner recovery batch cannot close",
                                        )
                                    })?,
                            )
                        }
                        None => None,
                    };
                    let identities = attach_interrupt_reclassification_candidates_for_active(
                        cancellation_identities,
                        &active_turn,
                        &mut next_reclassified_turn,
                    )
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "runner recovery interrupt reclassification candidates",
                        )
                    })?;
                    ModelCallInterruptOutcome::Cancelled(
                        scheduling
                            .apply_interrupt_to_runner_recovery(
                                source_snapshot,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match runner recovery wait",
                                )
                            })?,
                    )
                };
                persist_runner_recovery_interrupt_effect(
                    connection,
                    command,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    source_frontier,
                )
                .await?;
                Some(outcome)
            } else if let Some((active_turn, starting_snapshot)) =
                load_delegated_runner_recovery_for_interrupt(connection, interrupt.session())
                    .await?
            {
                let source_snapshot = runner_recovery_source_snapshot.ok_or(
                    SubmitInputCorruption::Missing("delegated runner recovery source frontier"),
                )?;
                let source_frontier = source_snapshot.frontier().snapshot();
                let command = interrupt.proof().command();
                let yielded_attempt = load_runner_recovery_yielded_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                )
                .await?;
                let interrupted_tool_attempt = match active_turn.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery {
                        optional_tool_attempt,
                        ..
                    } => *optional_tool_attempt,
                    _ => None,
                };
                let outcome = if let Some(recovery_attempt) = interrupted_tool_attempt {
                    let preserves_ambiguity = terminalize_retryable_runner_recovery_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        recovery_attempt,
                    )
                    .await?;
                    let batch = if preserves_ambiguity {
                        load_recovery_batch_by_attempt(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            recovery_attempt,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                    } else {
                        load_runner_recovery_cancellation_batch(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            yielded_attempt,
                            Some(recovery_attempt),
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing(
                            "delegated runner retryable recovery batch",
                        ))?
                    };
                    let request_ids = batch
                        .requests()
                        .iter()
                        .map(signalbox_domain::ToolRequest::id)
                        .collect::<Vec<_>>();
                    let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                    if !preserves_ambiguity {
                        let result_projection = batch
                            .prepare_cancellation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated retryable runner batch cannot close",
                                )
                            })?;
                        let identities =
                            attach_interrupt_reclassification_candidates_for_activated(
                                cancellation_identities,
                                &active_turn,
                                &mut next_reclassified_turn,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated retryable runner interrupt candidates",
                                )
                            })?;
                        ModelCallInterruptOutcome::Cancelled(
                            active_turn
                                .apply_interrupt_to_retryable_runner_tool_recovery(
                                    starting_snapshot,
                                    batch,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "delegated interrupt does not match retryable runner wait",
                                    )
                                })?,
                        )
                    } else {
                        let wait = batch.awaiting_recovery().ok_or(
                            SubmitInputCorruption::Inconsistent(
                                "delegated runner tool recovery wait",
                            ),
                        )?;
                        let tool_attempt = batch
                            .requests()
                            .iter()
                            .find_map(|request| match batch.attempt(request.id()) {
                                Some(signalbox_domain::ReconstitutedToolAttempt::Ended(
                                    attempt,
                                )) if attempt.attempt() == recovery_attempt => {
                                    Some(attempt.clone())
                                }
                                _ => None,
                            })
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "delegated runner ambiguous tool attempt",
                            ))?;
                        let result_projection = batch
                            .prepare_reconciliation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated runner recovery batch cannot preserve ambiguity",
                                )
                            })?;
                        let identities =
                            attach_recovery_interrupt_reclassification_candidates_for_activated(
                                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                                    result_frontier,
                                ),
                                &active_turn,
                                &mut next_reclassified_turn,
                            )?;
                        ModelCallInterruptOutcome::ToolReconciliationRequired(
                            active_turn
                                .apply_interrupt_to_runner_tool_recovery(
                                    wait,
                                    tool_attempt,
                                    yielded_attempt,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "delegated interrupt does not match runner tool recovery",
                                    )
                                })?,
                        )
                    }
                } else {
                    let result_projection = match load_runner_recovery_batch_without_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        yielded_attempt,
                    )
                    .await
                    .map_err(map_tool_loop_error)?
                    {
                        Some(batch) => {
                            let request_ids = batch
                                .requests()
                                .iter()
                                .map(signalbox_domain::ToolRequest::id)
                                .collect::<Vec<_>>();
                            let (result_entries, result_frontier) =
                                next_tool_cancellation(&request_ids);
                            Some(
                                batch
                                    .prepare_cancellation_projection(
                                        result_entries,
                                        result_frontier,
                                    )
                                    .map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "delegated runner recovery batch cannot close",
                                        )
                                    })?,
                            )
                        }
                        None => None,
                    };
                    let identities = attach_interrupt_reclassification_candidates_for_activated(
                        cancellation_identities,
                        &active_turn,
                        &mut next_reclassified_turn,
                    )
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "delegated runner recovery interrupt reclassification candidates",
                        )
                    })?;
                    ModelCallInterruptOutcome::Cancelled(
                        active_turn
                            .apply_interrupt_to_runner_recovery(
                                starting_snapshot,
                                source_snapshot,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match delegated runner recovery wait",
                                )
                            })?,
                    )
                };
                persist_runner_recovery_interrupt_effect(
                    connection,
                    command,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    source_frontier,
                )
                .await?;
                Some(outcome)
            } else {
                let execution =
                    require_live_execution_for_restart(connection, interrupt.session()).await?;
                let identities = attach_interrupt_reclassification_candidates(
                    cancellation_identities,
                    &execution,
                    &mut next_reclassified_turn,
                )?;
                Some(
                    execution
                        .apply_interrupt(interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match active model execution",
                            )
                        })?,
                )
            }
        }
    } else {
        None
    };
    insert_prepared_effects(connection, prepared).await?;
    match interrupt_outcome {
        Some(ModelCallInterruptOutcome::Cancelled(cancelled)) => {
            persist_terminal_outcome(
                connection,
                &ModelCallTerminalOutcome::Cancelled(cancelled),
                None,
            )
            .await?;
        }
        Some(ModelCallInterruptOutcome::CancellationRequested(stopped)) => {
            persist_stop_requested(connection, &stopped).await?;
        }
        Some(ModelCallInterruptOutcome::ReconciliationRequired(reconciliation)) => {
            let session = reconciliation.session();
            let turn = reconciliation.turn();
            persist_terminal_outcome(
                connection,
                &ModelCallTerminalOutcome::ReconciliationRequired(reconciliation),
                None,
            )
            .await?;
            supersede_automatic_reconciliation(connection, session, turn).await?;
        }
        Some(ModelCallInterruptOutcome::ToolReconciliationRequired(reconciliation)) => {
            persist_tool_reconciliation_required(connection, &reconciliation).await?;
            supersede_automatic_reconciliation(
                connection,
                reconciliation.session(),
                reconciliation.turn(),
            )
            .await?;
        }
        None => {}
    }
    if let Some(maximum_bytes) = attachment_maximum_bytes {
        if matches!(recorded, SubmitInputResult::Applied(_))
            && session_has_attachment_parts(connection, frontier_command.session()).await?
            && Box::pin(prospective_attachment_frontier_exceeds_bound(
                connection,
                frontier_command.session(),
                &prior_queued_inputs,
                &recorded,
                maximum_bytes,
            ))
            .await?
        {
            sqlx::query("ROLLBACK TO SAVEPOINT submit_input_attachment_frontier")
                .execute(&mut *connection)
                .await?;
            let rejected = frontier_command.prepare_attachment_byte_budget_exceeded(maximum_bytes);
            let recorded = rejected.result().clone();
            insert_prepared_command(connection, &rejected).await?;
            insert_prepared_effects(connection, rejected).await?;
            sqlx::query("RELEASE SAVEPOINT submit_input_attachment_frontier")
                .execute(&mut *connection)
                .await?;
            return Ok(TransactionDecision::Commit(
                SubmitInputHandlingOutcome::Recorded(recorded),
            ));
        }
        sqlx::query("RELEASE SAVEPOINT submit_input_attachment_frontier")
            .execute(&mut *connection)
            .await?;
    }
    Ok(TransactionDecision::Commit(
        SubmitInputHandlingOutcome::Recorded(recorded),
    ))
}
