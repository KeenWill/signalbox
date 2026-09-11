use super::load::{decode_logical_delegation_terminal, project_logical_delegation_terminal};
use super::transcript_types::{
    ProcessCurrentModelCall, ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
    ProcessFailedTerminalModelCall, ProcessReconciliationOperation, ProcessTranscriptTurn,
    ProcessTurnState,
};
use super::turn_facts::{
    DecodedStartLineage, DecodedTurn, DecodedTurnOrigin,
    admitted_automatic_reconciliation_attempts, decode_attachment_preparation_failure_cause,
    decode_provider_failure_cause, decode_transcript_turn_model_settings,
    decode_transcript_turn_origin,
};
use super::{
    ProcessReadCorruption, ProcessReadError, decode_positive, decode_runner_generation, required,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    ContextFrontierId, ModelCallId, RunnerId, SessionId, ToolAttemptId, ToolRequestId,
    TurnAttemptId, TurnId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;

pub(super) fn decode_transcript_turn(
    row: &PgRow,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
) -> Result<DecodedTurn, ProcessReadError> {
    let turn = TurnId::from_uuid(required(row, "turn_id")?);
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "turn acceptance position",
    )?;
    let origin_kind: String = required(row, "origin_kind")?;
    let origin = decode_transcript_turn_origin(
        origin_kind,
        row.try_get("origin_accepted_input_id")?,
        row.try_get("accepted_input_id")?,
        row.try_get("accepted_position")?,
        row.try_get("origin_turn_id")?,
        row.try_get("accepted_content")?,
        row.try_get("delegated_spawning_tool_request_id")?,
        row.try_get("delegated_parent_session_id")?,
        row.try_get("delegated_parent_turn_id")?,
        row.try_get("delegated_task_content")?,
        row.try_get("delegated_wake_first_delivery_sequence")?,
        row.try_get("delegated_wake_through_delivery_sequence")?,
        turn,
        acceptance_position,
    )?;
    let logical_terminal = decode_logical_delegation_terminal(row)?;
    // The accepted-input correlation checks now live in
    // `decode_transcript_turn_origin`, which also admits delegation origins.
    // Model-settings evidence is keyed by the originating accepted input, and
    // the schema forbids one on a delegation-origin turn, so those decode to no
    // resolved settings instead of demanding structurally absent evidence.
    let model_settings = match &origin {
        DecodedTurnOrigin::AcceptedInput { accepted_input, .. } => {
            decode_transcript_turn_model_settings(row, turn, *accepted_input)?
        }
        DecodedTurnOrigin::DelegatedTask { .. } | DecodedTurnOrigin::DelegationWake { .. } => None,
    };
    let state_kind: String = required(row, "state_kind")?;
    let start_lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
    let immediate_predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
    let start_lineage = match (
        state_kind.as_str(),
        start_lineage_kind.as_deref(),
        immediate_predecessor,
    ) {
        ("queued", None, None) => None,
        ("active" | "terminal", Some("first_in_session"), None) => {
            Some(DecodedStartLineage::FirstInSession)
        }
        ("active" | "terminal", Some("after"), Some(predecessor)) => {
            Some(DecodedStartLineage::After(TurnId::from_uuid(predecessor)))
        }
        ("queued" | "active" | "terminal", Some(value), _)
            if !matches!(value, "first_in_session" | "after") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "turn start lineage kind",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn start lineage shape").into());
        }
    };
    let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
    let active_phase: Option<String> = row.try_get("active_phase_kind")?;
    let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
    let child_wait_request: Option<Uuid> = row.try_get("child_wait_request_id")?;
    let child_wait_spawning_request: Option<Uuid> =
        row.try_get("child_wait_spawning_request_id")?;
    let child_wait_child: Option<Uuid> = row.try_get("child_wait_child_session_id")?;
    let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
    let recovery_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
    let active_tool_round_call: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
    let approval_tool_request: Option<Uuid> = row.try_get("approval_tool_request_id")?;
    let recovery_tool_attempt: Option<Uuid> = row.try_get("recovery_tool_attempt_id")?;
    let runner_recovery_runner: Option<Uuid> = row.try_get("runner_recovery_runner_id")?;
    let runner_recovery_revision: Option<Decimal> =
        row.try_get("runner_recovery_placement_revision")?;
    let runner_recovery_tool_attempt: Option<Uuid> =
        row.try_get("runner_recovery_tool_attempt_id")?;
    let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
    let terminal_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
    let terminal_tool_attempt: Option<Uuid> = row.try_get("terminal_tool_attempt_id")?;
    let terminal_call_disposition: Option<String> =
        row.try_get("terminal_model_call_disposition_kind")?;
    let terminal_call_provider_failure_cause: Option<String> =
        row.try_get("terminal_model_call_provider_failure_cause")?;
    let terminal_call_attachment_preparation_failure_cause: Option<String> =
        row.try_get("terminal_model_call_attachment_preparation_failure_cause")?;
    if active_phase.as_deref() != Some("awaiting_runner_recovery")
        && (runner_recovery_runner.is_some()
            || runner_recovery_revision.is_some()
            || runner_recovery_tool_attempt.is_some())
    {
        return Err(
            ProcessReadCorruption::Inconsistent("runner recovery lifecycle payload").into(),
        );
    }
    if terminal_call_provider_failure_cause.is_some()
        && terminal_call_disposition.as_deref() != Some("known_failed")
    {
        return Err(ProcessReadCorruption::Inconsistent(
            "provider failure cause without known-failed model call",
        )
        .into());
    }
    if terminal_call_attachment_preparation_failure_cause.is_some()
        && (terminal_call_disposition.as_deref() != Some("known_failed")
            || terminal_call_provider_failure_cause.is_some())
    {
        return Err(ProcessReadCorruption::Inconsistent(
            "attachment-preparation failure cause without local known-failed model call",
        )
        .into());
    }
    let current_model_call: Option<Uuid> = row.try_get("current_model_call_id")?;
    let current_model_call_state: Option<String> = row.try_get("current_model_call_state_kind")?;
    let current_model_call_frontier: Option<Uuid> =
        row.try_get("current_model_call_frontier_id")?;
    let recovery_model_call_frontier: Option<Uuid> =
        row.try_get("recovery_model_call_frontier_id")?;
    let automatic_reconciliation_state: Option<String> =
        row.try_get("automatic_reconciliation_state_kind")?;
    let automatic_reconciliation_attempts: Option<i32> =
        row.try_get("automatic_reconciliation_attempt_count")?;
    let automatic_reconciliation_model_call: Option<Uuid> =
        row.try_get("automatic_reconciliation_model_call_id")?;
    let automatic_reconciliation_tool_attempt: Option<Uuid> =
        row.try_get("automatic_reconciliation_tool_attempt_id")?;
    let active_tool_round_frontier: Option<Uuid> = row.try_get("active_tool_round_frontier_id")?;

    if !matches!(state_kind.as_str(), "queued" | "active" | "terminal") {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn state kind",
            value: state_kind,
        }
        .into());
    }
    if let Some(value) = active_phase.as_deref()
        && !matches!(
            value,
            "running"
                | "awaiting_model_call_recovery"
                | "awaiting_tool_approval"
                | "awaiting_child"
                | "awaiting_tool_recovery"
                | "awaiting_runner_recovery"
                | "awaiting_credential_availability"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn active phase",
            value: value.to_owned(),
        }
        .into());
    }
    let automatic_reconciliation_present = automatic_reconciliation_state.is_some()
        || automatic_reconciliation_attempts.is_some()
        || automatic_reconciliation_model_call.is_some()
        || automatic_reconciliation_tool_attempt.is_some();
    let terminal_reconciliation = state_kind == "terminal"
        && terminal_disposition.as_deref() == Some("reconciliation_required");
    if automatic_reconciliation_present {
        if !matches!(
            active_phase.as_deref(),
            Some("awaiting_model_call_recovery" | "awaiting_tool_recovery")
        ) && !terminal_reconciliation
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "automatic model-call reconciliation outside recovery wait",
            )
            .into());
        }
        if terminal_reconciliation {
            let valid_terminal_automatic = matches!(
                (
                    automatic_reconciliation_state.as_deref(),
                    automatic_reconciliation_attempts,
                    automatic_reconciliation_model_call,
                    automatic_reconciliation_tool_attempt,
                ),
                (
                    Some("reconciled" | "superseded"),
                    Some(attempts),
                    model_call,
                    tool_attempt,
                ) if admitted_automatic_reconciliation_attempts(
                        attempts,
                        false,
                        automatic_reconciliation_attempt_budget,
                    ).is_ok()
                    && model_call == terminal_call
                    && tool_attempt == terminal_tool_attempt
                    && model_call.is_some() != tool_attempt.is_some()
            );
            if !valid_terminal_automatic {
                return Err(ProcessReadCorruption::Inconsistent(
                    "terminal automatic model-call reconciliation state",
                )
                .into());
            }
        }
    }
    if let Some(value) = terminal_disposition.as_deref()
        && !matches!(
            value,
            "failed" | "completed" | "refused" | "cancelled" | "reconciliation_required"
        )
    {
        return Err(ProcessReadCorruption::Unsupported {
            field: "turn terminal disposition",
            value: value.to_owned(),
        }
        .into());
    }
    let (current_model_call, current_model_call_frontier) = match (
        current_model_call,
        current_model_call_state.as_deref(),
        current_model_call_frontier,
    ) {
        (None, None, None) => (None, None),
        (Some(call), Some("prepared"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::Prepared,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("in_flight"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::InFlight,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(call), Some("cancellation_requested"), Some(frontier)) => (
            Some(ProcessCurrentModelCall {
                call: ModelCallId::from_uuid(call),
                state: ProcessCurrentModelCallState::CancellationRequested,
            }),
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (Some(_), Some(value), _)
            if !matches!(value, "prepared" | "in_flight" | "cancellation_requested") =>
        {
            return Err(ProcessReadCorruption::Unsupported {
                field: "current model call state",
                value: value.to_owned(),
            }
            .into());
        }
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("current model call shape").into());
        }
    };
    let recovery_model_call_frontier =
        recovery_model_call_frontier.map(ContextFrontierId::from_uuid);

    if let Some(predecessor) = row.try_get::<Option<Uuid>, _>("wait_failure_predecessor_call_id")? {
        if state_kind != "terminal"
            || terminal_disposition.as_deref() != Some("failed")
            || active_phase.is_some()
            || current_attempt.is_some()
            || current_model_call.is_some()
            || terminal_call.is_some()
            || starting_frontier.is_none()
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "terminal credential wait release shape",
            )
            .into());
        }
        let frontier = ContextFrontierId::from_uuid(required(row, "terminal_frontier_id")?);
        let attempt = TurnAttemptId::from_uuid(required(row, "terminal_attempt_id")?);
        let cause = decode_provider_failure_cause(&required::<String>(
            row,
            "wait_failure_provider_cause",
        )?)?;
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::FailedAfterCredentialWait {
                        terminal_frontier: frontier,
                        terminal_attempt: attempt,
                        predecessor_call: ModelCallId::from_uuid(predecessor),
                        provider_cause: cause,
                    },
                },
                start_lineage,
                latest_frontier: Some(frontier),
            },
            logical_terminal,
        );
    }

    if matches!(
        active_phase.as_deref(),
        Some("awaiting_credential_availability")
    ) {
        if state_kind != "active"
            || current_attempt.is_some()
            || current_model_call.is_some()
            || terminal_frontier.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_disposition.is_some()
            || starting_frontier.is_none()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("credential availability wait shape").into(),
            );
        }
        let attempt = TurnAttemptId::from_uuid(required(row, "credential_wait_attempt_id")?);
        let frontier = ContextFrontierId::from_uuid(required(row, "credential_wait_frontier_id")?);
        let cause = match required::<String>(row, "credential_wait_cause")?.as_str() {
            "exhausted" => signalbox_domain::CredentialAvailabilityWaitCause::Exhausted,
            "contended" => signalbox_domain::CredentialAvailabilityWaitCause::Contended,
            "network_unavailable" => {
                signalbox_domain::CredentialAvailabilityWaitCause::NetworkUnavailable
            }
            _ => {
                return Err(ProcessReadCorruption::Inconsistent(
                    "credential availability wait cause",
                )
                .into());
            }
        };
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingCredentialAvailability {
                        wait: signalbox_domain::CredentialAvailabilityWait::new(
                            attempt, frontier, cause,
                        ),
                    },
                },
                start_lineage,
                latest_frontier: Some(frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_runner_recovery")) {
        let (Some(starting_frontier), Some(runner), Some(revision)) = (
            starting_frontier,
            runner_recovery_runner,
            runner_recovery_revision,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("runner recovery wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || child_wait_request.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
            || active_tool_round_call.is_some() != active_tool_round_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("runner recovery wait shape").into());
        }
        let latest_frontier = active_tool_round_frontier.unwrap_or(starting_frontier);
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingRunnerRecovery {
                        runner: RunnerId::from_uuid(runner),
                        placement_revision: decode_runner_generation(
                            revision,
                            "runner recovery placement revision",
                        )?,
                        interrupted_tool_attempt: runner_recovery_tool_attempt
                            .map(ToolAttemptId::from_uuid),
                    },
                },
                start_lineage,
                latest_frontier: Some(ContextFrontierId::from_uuid(latest_frontier)),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_child")) {
        let (
            Some(starting_frontier),
            Some(awaiting_request),
            Some(spawning_request),
            Some(child),
            Some(_producing_call),
            Some(tool_frontier),
        ) = (
            starting_frontier,
            child_wait_request,
            child_wait_spawning_request,
            child_wait_child,
            active_tool_round_call,
            active_tool_round_frontier,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("child wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("child wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("child wait frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingChild {
                        awaiting_request: ToolRequestId::from_uuid(awaiting_request),
                        spawning_request: ToolRequestId::from_uuid(spawning_request),
                        child: SessionId::from_uuid(child),
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_tool_approval")) {
        let (Some(starting_frontier), Some(_producing_call), Some(request), Some(tool_frontier)) = (
            starting_frontier,
            active_tool_round_call,
            approval_tool_request,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || current_attempt.is_some()
            || terminal_disposition.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool approval wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool approval frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingToolApproval {
                        request: ToolRequestId::from_uuid(request),
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("awaiting_tool_recovery")) {
        let (
            Some(starting_frontier),
            Some(ended_attempt),
            Some(_producing_call),
            Some(recovery_attempt),
            Some(tool_frontier),
        ) = (
            starting_frontier,
            current_attempt,
            active_tool_round_call,
            recovery_tool_attempt,
            active_tool_round_frontier,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery wait shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("tool recovery frontier").into());
        }
        if automatic_reconciliation_model_call.is_some()
            || automatic_reconciliation_tool_attempt
                .is_some_and(|stored| stored != recovery_attempt)
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "automatic tool reconciliation attempt identity",
            )
            .into());
        }
        let (automatic_reconciliation_attempts, operator_action_required) = match (
            automatic_reconciliation_state.as_deref(),
            automatic_reconciliation_attempts,
            automatic_reconciliation_tool_attempt,
        ) {
            (None, None, None) => (0, false),
            (Some("scheduled" | "attempting"), Some(attempts), Some(_)) => (
                admitted_automatic_reconciliation_attempts(
                    attempts,
                    false,
                    automatic_reconciliation_attempt_budget,
                )?,
                false,
            ),
            (Some("exhausted"), Some(attempts), Some(_)) => (
                admitted_automatic_reconciliation_attempts(
                    attempts,
                    true,
                    automatic_reconciliation_attempt_budget,
                )?,
                true,
            ),
            _ => {
                return Err(ProcessReadCorruption::Inconsistent(
                    "active automatic tool reconciliation state",
                )
                .into());
            }
        };
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveAwaitingToolRecovery {
                        ended_attempt: TurnAttemptId::from_uuid(ended_attempt),
                        recovery_attempt: ToolAttemptId::from_uuid(recovery_attempt),
                        automatic_reconciliation_attempts,
                        operator_action_required,
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if matches!(active_phase.as_deref(), Some("running")) && active_tool_round_call.is_some() {
        let (Some(starting_frontier), Some(attempt), Some(tool_frontier)) = (
            starting_frontier,
            current_attempt,
            active_tool_round_frontier,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        };
        if state_kind != "active"
            || terminal_frontier.is_some()
            || terminal_disposition.is_some()
            || approval_tool_request.is_some()
            || recovery_call.is_some()
            || recovery_tool_attempt.is_some()
            || terminal_attempt.is_some()
            || terminal_call.is_some()
            || terminal_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("running tool round shape").into());
        }
        let latest_frontier = ContextFrontierId::from_uuid(tool_frontier);
        if latest_frontier == ContextFrontierId::from_uuid(starting_frontier) {
            return Err(ProcessReadCorruption::Inconsistent("running tool frontier").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ActiveRunning {
                        current_attempt: TurnAttemptId::from_uuid(attempt),
                        current_model_call: None,
                    },
                },
                start_lineage,
                latest_frontier: Some(latest_frontier),
            },
            logical_terminal,
        );
    }

    if state_kind == "terminal"
        && terminal_disposition.as_deref() == Some("reconciliation_required")
        && terminal_call.is_none()
        && terminal_tool_attempt.is_some()
    {
        let (Some(frontier), Some(attempt), Some(tool_attempt)) =
            (terminal_frontier, terminal_attempt, terminal_tool_attempt)
        else {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        };
        if active_phase.is_some()
            || current_attempt.is_some()
            || recovery_call.is_some()
            || active_tool_round_call.is_some()
            || approval_tool_request.is_some()
            || recovery_tool_attempt.is_some()
            || current_model_call.is_some()
            || current_model_call_frontier.is_some()
            || recovery_model_call_frontier.is_some()
            || active_tool_round_frontier.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool reconciliation shape").into());
        }
        return project_logical_delegation_terminal(
            DecodedTurn {
                turn: ProcessTranscriptTurn {
                    turn,
                    acceptance_position,
                    model_settings,
                    state: ProcessTurnState::ReconciliationRequired {
                        terminal_frontier: ContextFrontierId::from_uuid(frontier),
                        terminal_attempt: TurnAttemptId::from_uuid(attempt),
                        operation: ProcessReconciliationOperation::ToolAttempt(
                            ToolAttemptId::from_uuid(tool_attempt),
                        ),
                    },
                },
                start_lineage,
                latest_frontier: Some(ContextFrontierId::from_uuid(frontier)),
            },
            logical_terminal,
        );
    }

    if active_tool_round_call.is_some()
        || approval_tool_request.is_some()
        || recovery_tool_attempt.is_some()
        || terminal_tool_attempt.is_some()
        || active_tool_round_frontier.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("tool lifecycle authority shape").into());
    }

    let (state, latest_frontier) = match (
        state_kind.as_str(),
        starting_frontier,
        terminal_frontier,
        active_phase.as_deref(),
        current_attempt,
        terminal_disposition.as_deref(),
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_call_disposition.as_deref(),
        current_model_call,
    ) {
        ("queued", None, None, None, None, None, None, None, None, None, None) => {
            let state = match origin {
                DecodedTurnOrigin::AcceptedInput {
                    accepted_input,
                    content,
                } => ProcessTurnState::Queued {
                    accepted_input,
                    content,
                },
                DecodedTurnOrigin::DelegatedTask {
                    spawning_request,
                    parent_session,
                    parent_turn,
                    content,
                } => ProcessTurnState::QueuedDelegated {
                    spawning_request,
                    parent_session,
                    parent_turn,
                    content,
                },
                DecodedTurnOrigin::DelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                } => ProcessTurnState::QueuedDelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                },
            };
            (state, None)
        }
        (
            "active",
            Some(frontier),
            None,
            Some("running"),
            Some(attempt),
            None,
            None,
            None,
            None,
            None,
            current_model_call,
        ) => (
            ProcessTurnState::ActiveRunning {
                current_attempt: TurnAttemptId::from_uuid(attempt),
                current_model_call,
            },
            Some(
                current_model_call_frontier
                    .unwrap_or_else(|| ContextFrontierId::from_uuid(frontier)),
            ),
        ),
        (
            "active",
            Some(_),
            None,
            Some("awaiting_model_call_recovery"),
            Some(attempt),
            None,
            Some(call),
            None,
            None,
            None,
            None,
        ) => {
            if automatic_reconciliation_tool_attempt.is_some()
                || automatic_reconciliation_model_call.is_some_and(|stored| stored != call)
            {
                return Err(ProcessReadCorruption::Inconsistent(
                    "automatic model-call reconciliation call identity",
                )
                .into());
            }
            let call_frontier = recovery_model_call_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("recovery model call frontier"),
            )?;
            let (automatic_reconciliation_attempts, operator_action_required) = match (
                automatic_reconciliation_state.as_deref(),
                automatic_reconciliation_attempts,
                automatic_reconciliation_model_call,
            ) {
                (None, None, None) => (0, false),
                (Some("scheduled" | "attempting"), Some(attempts), Some(_)) => (
                    admitted_automatic_reconciliation_attempts(
                        attempts,
                        false,
                        automatic_reconciliation_attempt_budget,
                    )?,
                    false,
                ),
                (Some("exhausted"), Some(attempts), Some(_)) => (
                    admitted_automatic_reconciliation_attempts(
                        attempts,
                        true,
                        automatic_reconciliation_attempt_budget,
                    )?,
                    true,
                ),
                _ => {
                    return Err(ProcessReadCorruption::Inconsistent(
                        "active automatic model-call reconciliation state",
                    )
                    .into());
                }
            };
            (
                ProcessTurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt: TurnAttemptId::from_uuid(attempt),
                    recovery_call: ModelCallId::from_uuid(call),
                    automatic_reconciliation_attempts,
                    operator_action_required,
                },
                Some(call_frontier),
            )
        }
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: None,
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("failed"),
            None,
            Some(attempt),
            Some(call),
            Some(disposition @ ("known_failed" | "cancelled")),
            None,
        ) => (
            ProcessTurnState::Failed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: Some(TurnAttemptId::from_uuid(attempt)),
                terminal_model_call: Some(ProcessFailedTerminalModelCall {
                    call: ModelCallId::from_uuid(call),
                    disposition: match disposition {
                        "known_failed" => ProcessFailedModelCallDisposition::KnownFailed,
                        "cancelled" => ProcessFailedModelCallDisposition::Cancelled,
                        _ => {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "failed terminal model call disposition",
                            )
                            .into());
                        }
                    },
                    provider_failure_cause: terminal_call_provider_failure_cause
                        .as_deref()
                        .map(decode_provider_failure_cause)
                        .transpose()?,
                    attachment_preparation_failure_cause:
                        terminal_call_attachment_preparation_failure_cause
                            .as_deref()
                            .map(decode_attachment_preparation_failure_cause)
                            .transpose()?,
                }),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("completed"),
            None,
            Some(attempt),
            Some(call),
            Some("completed"),
            None,
        ) => (
            ProcessTurnState::Completed {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("refused"),
            None,
            Some(attempt),
            Some(call),
            Some("refused"),
            None,
        ) => (
            ProcessTurnState::Refused {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: ModelCallId::from_uuid(call),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            None,
            None,
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: None,
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("cancelled"),
            None,
            Some(attempt),
            Some(call),
            Some("cancelled"),
            None,
        ) => (
            ProcessTurnState::Cancelled {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                terminal_call: Some(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        (
            "terminal",
            Some(_),
            Some(frontier),
            None,
            None,
            Some("reconciliation_required"),
            None,
            Some(attempt),
            Some(call),
            Some("ambiguous"),
            None,
        ) => (
            ProcessTurnState::ReconciliationRequired {
                terminal_frontier: ContextFrontierId::from_uuid(frontier),
                terminal_attempt: TurnAttemptId::from_uuid(attempt),
                operation: ProcessReconciliationOperation::ModelCall(ModelCallId::from_uuid(call)),
            },
            Some(ContextFrontierId::from_uuid(frontier)),
        ),
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("turn lifecycle state shape").into());
        }
    };

    project_logical_delegation_terminal(
        DecodedTurn {
            turn: ProcessTranscriptTurn {
                turn,
                acceptance_position,
                model_settings,
                state,
            },
            start_lineage,
            latest_frontier,
        },
        logical_terminal,
    )
}
