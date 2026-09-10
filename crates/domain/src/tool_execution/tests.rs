//! Tool-batch transition and reconstitution tests for `docs/spec/tool-loop.md`.

use crate::{
    ActiveTurnPhase, CurrentToolAttemptState, DecideToolRequest, DecideToolRequestResult,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryPayload,
    ToolApprovalDecision, ToolApprovalResolution, ToolAttemptEnd, ToolEffectClass,
    ToolExecutionErrorKind, ToolRequest, ToolRequestId,
};

use super::*;
use crate::{
    DelegationContent, DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationProvenanceReconstitutionInput, DurableCommandId, NormalizedToolArguments,
    ReconstitutedToolAttempt, ToolApprovalResolutionReconstitutionInput, ToolArgumentsKind,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDecisionSource,
    ToolDispatchGeneration, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
    ToolResultContent, ToolResultText,
    test_support::{
        context_frontier_id, model_call_id, semantic_transcript_entry_id, session_id,
        tool_attempt_id, tool_request_id, turn_attempt_id, turn_id,
    },
};

fn request(id: u128, ordinal: u32) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        tool_request_id(id),
        session_id(1),
        turn_id(2),
        model_call_id(3),
        ToolRequestOrdinal::from_u32(ordinal),
        ToolName::try_new(format!("tool_{id}")).expect("fixture name is valid"),
        NormalizedToolArguments::try_from_stored(ToolArgumentsKind::Json, String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request()
}

fn approval(request: ToolRequestId, decision: ToolApprovalDecision) -> ToolApprovalResolution {
    ToolApprovalResolutionReconstitutionInput::user_fixture(request, decision)
        .reconstitute()
        .expect("user decisions are implemented")
}

fn automatic_approval(request: ToolRequestId) -> ToolApprovalResolution {
    ToolApprovalResolutionReconstitutionInput::policy_auto(request)
        .reconstitute()
        .expect("automatic approval is implemented")
}

fn yielded_snapshot() -> ResolvedContextFrontierSnapshot {
    ResolvedContextFrontierSnapshot::try_from_candidate(
        session_id(1),
        context_frontier_id(4),
        Vec::new(),
    )
    .expect("an empty fixture snapshot is valid")
}

fn awaiting_batch() -> ToolBatch {
    ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![request(10, 0), request(11, 1)],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: tool_request_id(10),
        },
    )
    .reconstitute()
    .expect("the first undecided request is exact")
}

/// user decisions advance exactly one earliest wait and retain explicit user provenance.
#[test]
fn user_decision_advances_to_next_wait() {
    let batch = awaiting_batch();
    let current_request = batch
        .requests()
        .first()
        .expect("the fixture has a current approval request")
        .id();
    let expected_next_request = batch
        .requests()
        .get(1)
        .expect("the fixture has one following approval request")
        .id();
    let command = DecideToolRequest::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
        current_request,
        ToolApprovalDecision::Approve,
    );
    let prepared = batch
        .prepare_user_decision(command, None)
        .expect("the earliest decision needs no continuation yet");
    let DecideToolRequestResult::Applied(applied) = prepared.prepared_command().result() else {
        panic!("the earliest exact decision applies");
    };

    assert_eq!(
        applied.resolution().source(),
        ToolDecisionSource::UserCommand
    );
    let ActiveTurnPhase::AwaitingApproval {
        request: next_request,
    } = prepared.active_phase()
    else {
        panic!("one decision advances to the next approval wait");
    };
    assert_eq!(*next_request, expected_next_request);
}

/// durable approval history is exactly a proposal-order prefix and cannot skip the current wait.
#[test]
fn reconstitution_rejects_nonprefix_approval_inventory() {
    let first = request(10, 0);
    let second = request(11, 1);
    let input = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        vec![approval(second.id(), ToolApprovalDecision::Approve)],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: first.id(),
        },
    );

    let error = input
        .reconstitute()
        .expect_err("a later approval cannot bypass the earliest request");
    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::ApprovalInventoryMismatch
    );
}

/// stored delegate evidence cannot be cross-wired to a request
/// whose frozen posture reserves the decision for a human.
#[test]
fn reconstitution_rejects_delegate_resolution_for_human_request() {
    const SUBJECT_REQUEST_SEED: u128 = 10;
    const SUBJECT_SESSION_SEED: u128 = 1;
    const SUBJECT_TURN_SEED: u128 = 2;
    const ISSUING_CALL_SEED: u128 = 3;
    const SUBJECT_ORDINAL: u32 = 0;
    const JUDGE_MODEL_SEED: u128 = 11;
    const JUDGE_CALL_SEED: u128 = 12;
    const EXECUTION_ATTEMPT_SEED: u128 = 4;
    const SUBJECT_TOOL_NAME: &str = "tool_10";
    const SUBJECT_ARGUMENTS: &str = "{}";
    const JUDGE_RATIONALE: &str = "bounded request";

    let request_id = tool_request_id(SUBJECT_REQUEST_SEED);
    let session = session_id(SUBJECT_SESSION_SEED);
    let turn = turn_id(SUBJECT_TURN_SEED);
    let issuing_call = model_call_id(ISSUING_CALL_SEED);
    let ordinal = ToolRequestOrdinal::from_u32(SUBJECT_ORDINAL);
    let name = ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid");
    let arguments = NormalizedToolArguments::try_from_stored(
        ToolArgumentsKind::Json,
        String::from(SUBJECT_ARGUMENTS),
    )
    .expect("fixture arguments are canonical");
    let request_with_posture = |posture| {
        ToolRequestReconstitutionInput::new(
            request_id,
            session,
            turn,
            issuing_call,
            ordinal,
            name.clone(),
            arguments.clone(),
        )
        .with_approval_posture(posture)
        .into_request()
    };
    let delegated_request = request_with_posture(crate::ToolApprovalPosture::Delegated);
    let stored_request = request_with_posture(crate::ToolApprovalPosture::Human);
    let rationale = crate::ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
        .expect("fixture rationale is admitted");
    let delegated = crate::DelegateToolApproval::try_new(
        &delegated_request,
        crate::DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED)),
        model_call_id(JUDGE_CALL_SEED),
        crate::DelegateApprovalRecommendation::Approve,
        rationale,
    )
    .expect("the delegated fixture permits approval");
    let resolution = ToolApprovalResolutionReconstitutionInput::delegate(delegated, None)
        .reconstitute()
        .expect("the delegate evidence is internally valid");
    let input = ToolBatchReconstitutionInput::new(
        session,
        turn,
        issuing_call,
        yielded_snapshot(),
        vec![stored_request],
        vec![resolution],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(EXECUTION_ATTEMPT_SEED),
        },
    );

    let error = input
        .reconstitute()
        .expect_err("delegate evidence cannot widen human-only authority");

    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::ApprovalInventoryMismatch
    );
}

#[test]
fn reconstitution_preserves_large_request_batches() {
    let requests = (0..40)
        .map(|ordinal| request(u128::from(ordinal) + 10, ordinal))
        .collect();
    let input = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        requests,
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: tool_request_id(10),
        },
    );

    let batch = input
        .reconstitute()
        .expect("stored batches retain every logical request");
    assert_eq!(batch.requests().len(), 40);
}

/// model-call completion may freeze automatic approval for a later request while an earlier
/// confirmation still waits.
#[test]
fn later_automatic_approval_survives_reconstitution() {
    let first = request(10, 0);
    let second = request(11, 1);
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        vec![automatic_approval(second.id())],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: first.id(),
        },
    )
    .reconstitute()
    .expect("later frozen policy authority does not bypass the earlier wait");

    assert_eq!(
        batch
            .approval(second.id())
            .map(ToolApprovalResolution::source),
        Some(ToolDecisionSource::PolicyAuto)
    );
}

/// a user decision is admissible only at the exact durable approval wait and cannot manufacture a
/// wait from execution.
#[test]
fn user_decision_rejects_nonwaiting_batch_unchanged() {
    let only = request(10, 0);
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(12),
        },
    )
    .reconstitute()
    .expect("complete approval admits execution");
    let command = DecideToolRequest::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
        only.id(),
        ToolApprovalDecision::Deny { reason: None },
    );
    let error = batch
        .prepare_user_decision(command, None)
        .expect_err("execution is not an approval decision point");

    assert_eq!(
        error.failure(),
        ToolBatchDecisionFailure::NoUndecidedRequest
    );
    assert_eq!(
        error.batch().phase(),
        ToolBatchPhase::Executing {
            turn_attempt: turn_attempt_id(12)
        }
    );
}

/// one active batch cannot turn an existing request from a different aggregate into a user-global
/// not-found result.
#[test]
fn out_of_batch_decision_is_a_correlation_error() {
    let command = DecideToolRequest::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
        tool_request_id(99),
        ToolApprovalDecision::Approve,
    );
    let error = awaiting_batch()
        .prepare_user_decision(command, None)
        .expect_err("batch-local absence cannot establish global absence");

    assert_eq!(
        error.failure(),
        ToolBatchDecisionFailure::CommandCorrelationMismatch
    );
    assert_eq!(error.command().request(), tool_request_id(99));
    assert_eq!(
        error.batch().phase(),
        ToolBatchPhase::AwaitingApproval {
            request: tool_request_id(10)
        }
    );
}

/// serialized execution prepares only the first approved request without terminal attempt evidence.
#[test]
fn execution_prepares_first_unattempted_request() {
    let first = request(10, 0);
    let second = request(11, 1);
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second],
        vec![
            approval(first.id(), ToolApprovalDecision::Approve),
            approval(tool_request_id(11), ToolApprovalDecision::Approve),
        ],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(12),
        },
    )
    .reconstitute()
    .expect("complete approvals admit execution");
    let prepared = batch
        .prepare_next_attempt(tool_attempt_id(13), ToolEffectClass::EffectFree)
        .expect("the first approved request is next");

    assert_eq!(prepared.attempt().request(), first.id());
    assert_eq!(
        prepared.attempt().state(),
        CurrentToolAttemptState::Prepared
    );
}

/// only a completely reconstituted ambiguous batch can expose the exact tool recovery-wait subject.
#[test]
fn ambiguous_batch_exposes_opaque_recovery_wait() {
    let only = request(10, 0);
    let attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(13),
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id(13),
        },
    )
    .reconstitute()
    .expect("the exact ambiguous attempt admits recovery");
    let wait = batch
        .awaiting_recovery()
        .expect("a validated recovery batch exposes its opaque wait");

    assert_eq!(wait.session(), session_id(1));
    assert_eq!(wait.turn(), turn_id(2));
    assert_eq!(wait.issuing_attempt(), turn_attempt_id(12));
    assert_eq!(wait.attempt(), tool_attempt_id(13));
}

/// impossible effect-free ambiguity cannot manufacture recovery-wait authority during checked
/// reconstitution.
#[test]
fn effect_free_ambiguous_history_fails_closed() {
    let only = request(10, 0);
    let attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(13),
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let error = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id(13),
        },
    )
    .reconstitute()
    .expect_err("effect-free ambiguity is not trusted recovery evidence");

    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch
    );
}

/// a live serialized attempt is the last attempt that can exist in proposal order.
#[test]
fn reconstitution_rejects_attempt_after_live_attempt() {
    let first = request(10, 0);
    let second = request(11, 1);
    let current = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(13),
        first.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Prepared,
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let later = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(14),
        second.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
            error: crate::ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let error = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        vec![
            approval(first.id(), ToolApprovalDecision::Approve),
            approval(second.id(), ToolApprovalDecision::Approve),
        ],
        vec![current, later],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(12),
        },
    )
    .reconstitute()
    .expect_err("serialized execution cannot create work after a live attempt");

    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::AttemptOrderMismatch
    );
}

/// recovery evidence belongs to one issuing continuation tenure throughout the complete batch.
#[test]
fn recovery_rejects_mixed_issuing_attempts() {
    let first = request(10, 0);
    let second = request(11, 1);
    let completed = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(13),
        first.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("ok")).expect("bounded result is valid"),
            ),
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let ambiguous = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(14),
        second.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(15),
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let error = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        vec![
            approval(first.id(), ToolApprovalDecision::Approve),
            approval(second.id(), ToolApprovalDecision::Approve),
        ],
        vec![completed, ambiguous],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id(14),
        },
    )
    .reconstitute()
    .expect_err("one recovery batch cannot cross continuation tenures");

    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch
    );
}

/// crash-lost evidence is a turn-level blocker, so no later approved request can be prepared or
/// already attempted.
#[test]
fn crash_loss_stops_serial_batch_execution() {
    let first = request(10, 0);
    let second = request(11, 1);
    let crash_lost = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(13),
        first.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
            error: crate::ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let approvals = vec![
        approval(first.id(), ToolApprovalDecision::Approve),
        approval(second.id(), ToolApprovalDecision::Approve),
    ];
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        approvals.clone(),
        vec![crash_lost.clone()],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(12),
        },
    )
    .reconstitute()
    .expect("crash-loss history remains inspectable for terminalization");
    assert_eq!(
        batch
            .prepare_next_attempt(tool_attempt_id(14), ToolEffectClass::ExternalEffect)
            .expect_err("no later tool may run after crash loss")
            .failure(),
        ToolBatchExecutionFailure::TurnLevelFailure
    );
    let failure_projection = batch
        .prepare_failure_projection(
            vec![
                semantic_transcript_entry_id(15),
                semantic_transcript_entry_id(16),
            ],
            context_frontier_id(17),
        )
        .expect("the blocked batch has a public failure projection");
    assert_eq!(
        failure_projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: tool_attempt_id(13),
        }
    );
    assert_eq!(
        failure_projection.entries()[1].payload(),
        &SemanticTranscriptEntryPayload::ToolClosed {
            request: tool_request_id(11),
        }
    );

    let later = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(14),
        second.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(12),
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
            error: crate::ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let error = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first, second],
        approvals,
        vec![crash_lost, later],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(12),
        },
    )
    .reconstitute()
    .expect_err("stored execution after crash loss is impossible history");
    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::AttemptOrderMismatch
    );
}

/// result projection uses only attempt/request references and preserves proposal order.
#[test]
fn result_projection_is_reference_only_and_ordered() {
    let executed = request(10, 0);
    let denied = request(11, 1);
    let success = ToolAttemptEnd::Completed {
        result: ToolResultContent::Text(
            ToolResultText::try_new(String::from("ok")).expect("bounded result is valid"),
        ),
    };
    let attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(12),
        executed.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(success),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![executed, denied],
        vec![
            approval(tool_request_id(10), ToolApprovalDecision::Approve),
            approval(
                tool_request_id(11),
                ToolApprovalDecision::Deny { reason: None },
            ),
        ],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .reconstitute()
    .expect("terminal evidence and denial resolve the batch");
    let projection = batch
        .prepare_result_projection(
            vec![
                semantic_transcript_entry_id(14),
                semantic_transcript_entry_id(15),
            ],
            context_frontier_id(16),
        )
        .expect("all logical results can be projected");

    assert_eq!(
        projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: tool_attempt_id(12),
        }
    );
    assert_eq!(
        projection.entries()[1].payload(),
        &SemanticTranscriptEntryPayload::ToolDenied {
            request: tool_request_id(11),
        }
    );
}

/// a delivered foreground child wait reopens the batch under a fresh turn attempt and projects the
/// typed result once.
#[test]
fn foreground_child_wait_resumes_and_projects_typed_result() {
    let awaited = request(10, 0);
    let spawning_request = tool_request_id(11);
    let child = session_id(9);
    let waiting_attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(12),
        awaited.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::AwaitingChild {
            spawning_request,
            child,
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let approvals = vec![approval(awaited.id(), ToolApprovalDecision::Approve)];
    let waiting = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![awaited.clone()],
        approvals.clone(),
        vec![waiting_attempt.clone()],
        ToolBatchPhaseReconstitutionInput::AwaitingChild {
            request: awaited.id(),
            spawning_request,
            child,
        },
    )
    .reconstitute()
    .expect("the exact foreground child wait reconstitutes");
    let resumed = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![awaited.clone()],
        approvals,
        vec![waiting_attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(14),
        },
    )
    .reconstitute()
    .expect("a delivered wait resumes under a fresh turn attempt");
    let content = DelegationContent::try_new(String::from("checked child result"))
        .expect("the child result is bounded");
    let outcome = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ResultReturned,
        Some(content.clone()),
        DelegationOutcomeReason::ChildCompleted,
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: child,
            turn: turn_id(15),
        },
    )
    .expect("the child completion outcome is exact");
    let projection = resumed
        .prepare_delegation_result_projection(
            vec![semantic_transcript_entry_id(16)],
            context_frontier_id(17),
            [(awaited.id(), outcome.clone())].into(),
        )
        .expect("the delivered child result closes the logical request");
    let interrupted = waiting
        .prepare_delegation_cancellation_projection(
            vec![semantic_transcript_entry_id(18)],
            context_frontier_id(19),
            Default::default(),
        )
        .expect("a parent-only interrupt closes the child wait without a result");

    assert_eq!(
        waiting.phase(),
        ToolBatchPhase::AwaitingChild {
            request: awaited.id(),
            spawning_request,
            child,
        }
    );
    assert_eq!(
        projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::DelegationResult {
            awaiting_request: awaited.id(),
            spawning_request,
            child,
            mode: crate::DelegationWaitMode::Foreground,
            delivery_sequence: None,
            outcome: Box::new(outcome),
        }
    );
    assert_eq!(
        interrupted.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolClosed {
            request: awaited.id(),
        }
    );
}

/// terminal recovery closes every logical request in proposal order without rewriting physical
/// ambiguity.
#[test]
fn reconciliation_projection_closes_ambiguity() {
    let ambiguous = request(10, 0);
    let unresolved = request(11, 1);
    let attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(12),
        ambiguous.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![ambiguous, unresolved],
        vec![
            approval(tool_request_id(10), ToolApprovalDecision::Approve),
            approval(tool_request_id(11), ToolApprovalDecision::Approve),
        ],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id(12),
        },
    )
    .reconstitute()
    .expect("the exact external-effect ambiguity admits recovery");
    let projection = batch
        .prepare_reconciliation_projection(
            vec![
                semantic_transcript_entry_id(14),
                semantic_transcript_entry_id(15),
            ],
            context_frontier_id(16),
        )
        .expect("terminal recovery closes every logical request");

    assert_eq!(
        projection
            .entries()
            .iter()
            .map(SemanticTranscriptEntry::payload)
            .collect::<Vec<_>>(),
        vec![
            &SemanticTranscriptEntryPayload::ToolClosed {
                request: tool_request_id(10),
            },
            &SemanticTranscriptEntryPayload::ToolClosed {
                request: tool_request_id(11),
            },
        ]
    );
    assert_eq!(projection.snapshot().entry_count(), 2);
}
/// every clone of one checked batch shares one runner-authorization issuance capability.
#[test]
fn runner_authorization_is_single_use_across_batch_clones() {
    let only = request(10, 0);
    let attempt_id = tool_attempt_id(12);
    let attempt = ToolAttemptReconstitutionInput::new(
        attempt_id,
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Prepared,
    )
    .reconstitute()
    .expect("the prepared attempt is valid");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .reconstitute()
    .expect("the prepared batch is complete");
    let duplicate = batch.clone();

    batch
        .authorize_runner_attempt(attempt_id)
        .expect("the batch atomically pairs canonical request authority once");
    let duplicate = duplicate
        .authorize_runner_attempt(attempt_id)
        .expect_err("the shared runner authority cannot be paired twice");

    assert_eq!(
        duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
}

/// restored in-flight authority is also single-use for runner conversion across clones of one
/// checked batch.
#[test]
fn in_flight_runner_authorization_is_single_use_across_clones() {
    let only = request(10, 0);
    let attempt_id = tool_attempt_id(12);
    let approval = approval(only.id(), ToolApprovalDecision::Approve);
    let attempt = ToolAttemptReconstitutionInput::new(
        attempt_id,
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the in-flight attempt is valid");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval.clone()],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .reconstitute()
    .expect("the in-flight batch is complete");
    let duplicate = batch.clone();

    batch
        .resume_runner_attempt(attempt_id)
        .expect("the batch atomically restores canonical runner authority once");
    let durable_issuance = batch.runner_authorized_attempts().collect::<Vec<_>>();
    let current = batch
        .attempt(only.id())
        .expect("the durable batch retains the in-flight attempt")
        .clone();
    let restored = ToolBatchReconstitutionInput::new(
        batch.session(),
        batch.turn(),
        batch.producing_call(),
        batch.yielded_snapshot().clone(),
        vec![only],
        vec![approval],
        vec![current],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .with_runner_authorized_attempts(durable_issuance.clone())
    .reconstitute()
    .expect("durable runner issuance restores with the batch");
    let duplicate = duplicate
        .resume_runner_attempt(attempt_id)
        .expect_err("the clone shares the consumed runner authority");
    let restored = restored
        .resume_runner_attempt(attempt_id)
        .expect_err("durable reconstitution preserves consumed runner authority");

    assert_eq!(durable_issuance, vec![attempt_id]);
    assert_eq!(
        duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
    assert_eq!(
        restored.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
}

/// reconstitution restores every retired identity and rejects it as a later claimed-attempt
/// replacement.
#[test]
fn reconstituted_batch_rejects_retired_identity_reuse() {
    let only = request(10, 0);
    let current_id = tool_attempt_id(12);
    let retired_id = tool_attempt_id(11);
    let current = ToolAttemptReconstitutionInput::new(
        current_id,
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the in-flight attempt is valid");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![current],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .with_retired_attempts(vec![retired_id])
    .reconstitute()
    .expect("the durable retired inventory is complete");

    assert_eq!(
        batch.retired_attempts().collect::<Vec<_>>(),
        vec![retired_id]
    );
    assert_eq!(
        batch
            .replace_claimed_attempt(current_id, retired_id)
            .err()
            .expect("a retired identity cannot be reused")
            .failure(),
        ToolBatchExecutionFailure::AttemptIdentityReuse
    );
}

/// ordinary preparation rejects every durably retired identity.
#[test]
fn ordinary_preparation_rejects_retired_identity_reuse() {
    let first = request(10, 0);
    let second = request(11, 1);
    let ended_id = tool_attempt_id(13);
    let retired_id = tool_attempt_id(12);
    let ended = ToolAttemptReconstitutionInput::new(
        ended_id,
        first.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(14),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
            error: crate::ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
        }),
    )
    .reconstitute()
    .expect("the ended attempt is valid");
    let batch = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![first.clone(), second.clone()],
        vec![
            approval(first.id(), ToolApprovalDecision::Approve),
            approval(second.id(), ToolApprovalDecision::Approve),
        ],
        vec![ended],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(14),
        },
    )
    .with_retired_attempts(vec![retired_id])
    .reconstitute()
    .expect("the retired inventory and ended history are complete");

    assert_eq!(
        batch
            .prepare_next_attempt(retired_id, ToolEffectClass::EffectFree)
            .expect_err("ordinary preparation cannot reuse a retired identity")
            .failure(),
        ToolBatchExecutionFailure::AttemptIdentityReuse
    );
}

/// retired and current inventories must be disjoint.
#[test]
fn reconstitution_rejects_current_identity_as_retired() {
    let only = request(10, 0);
    let current_id = tool_attempt_id(12);
    let current = ToolAttemptReconstitutionInput::new(
        current_id,
        only.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(13),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the in-flight attempt is valid");
    let input = ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        vec![only.clone()],
        vec![approval(only.id(), ToolApprovalDecision::Approve)],
        vec![current],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .with_retired_attempts(vec![current_id]);

    assert_eq!(
        input
            .reconstitute()
            .expect_err("current and retired identities cannot overlap")
            .failure(),
        ToolBatchReconstitutionFailure::AttemptInventoryMismatch
    );
}

fn inadmissible_request() -> ToolRequest {
    let request = request(10, 0);
    ToolRequestReconstitutionInput::new(
        request.id(),
        request.session(),
        request.turn(),
        request.producing_call(),
        request.ordinal(),
        request.name().clone(),
        request.arguments().clone(),
    )
    .with_inadmissible_reason(Some(crate::ToolInadmissibleReason::PlacementLost))
    .into_request()
}

#[test]
fn inadmissible_only_batch_projects_one_result_without_execution() {
    let request = inadmissible_request();
    let batch = ToolBatchReconstitutionInput::new(
        request.session(),
        request.turn(),
        request.producing_call(),
        yielded_snapshot(),
        vec![request.clone()],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(13),
        },
    )
    .reconstitute()
    .expect("inadmissibility resolves the request without approval or attempt");
    let projection = batch
        .prepare_result_projection(
            vec![semantic_transcript_entry_id(14)],
            context_frontier_id(16),
        )
        .expect("the complete batch can continue");
    assert_eq!(
        projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolInadmissible {
            request: request.id()
        }
    );
    let cancellation = batch
        .prepare_cancellation_projection(
            vec![semantic_transcript_entry_id(15)],
            context_frontier_id(17),
        )
        .expect("terminalization preserves the request resolution");
    assert_eq!(
        cancellation.entries()[0].payload(),
        projection.entries()[0].payload()
    );
    let error = batch
        .prepare_next_attempt(tool_attempt_id(12), ToolEffectClass::EffectFree)
        .expect_err("closed requests never mint physical attempts");
    assert_eq!(
        error.failure(),
        ToolBatchExecutionFailure::ReadyForContinuation
    );
}

#[test]
fn inadmissible_request_does_not_park_a_later_approval() {
    let closed = inadmissible_request();
    let pending = request(11, 1);
    let batch = ToolBatchReconstitutionInput::new(
        closed.session(),
        closed.turn(),
        closed.producing_call(),
        yielded_snapshot(),
        vec![closed.clone(), pending.clone()],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: pending.id(),
        },
    )
    .reconstitute()
    .expect("only the later request is undecided");
    let decision = batch
        .prepare_user_decision(
            DecideToolRequest::new(
                DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
                pending.id(),
                ToolApprovalDecision::Deny { reason: None },
            ),
            Some(turn_attempt_id(13)),
        )
        .expect("the last approval opens continuation");
    let projection = decision
        .batch()
        .prepare_result_projection(
            vec![
                semantic_transcript_entry_id(14),
                semantic_transcript_entry_id(15),
            ],
            context_frontier_id(16),
        )
        .expect("both logical resolutions project");
    assert_eq!(
        projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolInadmissible {
            request: closed.id()
        }
    );
    assert_eq!(
        projection.entries()[1].payload(),
        &SemanticTranscriptEntryPayload::ToolDenied {
            request: pending.id()
        }
    );
    assert!(
        crate::ApprovedToolRequest::try_from_resolution(
            closed.clone(),
            approval(closed.id(), ToolApprovalDecision::Approve)
        )
        .is_err()
    );
}

#[test]
fn placement_loss_retires_prepared_attempt_and_preserves_dispatched_attempt() {
    let request = request(10, 0);
    let approved = crate::ApprovedToolRequest::try_from_resolution(
        request.clone(),
        approval(request.id(), ToolApprovalDecision::Approve),
    )
    .expect("fixture approval matches");
    let prepared = approved.prepare_attempt(
        tool_attempt_id(12),
        turn_attempt_id(13),
        ToolEffectClass::ExternalEffect,
    );
    let ended = prepared
        .clone()
        .end_placement_lost()
        .expect("placement loss may retire a prepared attempt");
    let ToolAttemptEnd::KnownFailed { error } = ended.end() else {
        panic!("retired attempt retains a known failure");
    };
    assert_eq!(error.kind(), ToolExecutionErrorKind::ExecutionFailed);
    assert_eq!(
        error.detail().map(|detail| detail.as_str()),
        Some("placement_lost")
    );
    let authorized = prepared.authorize().expect("the baseline may dispatch");
    let dispatched = authorized.attempt().clone();
    let rejection = dispatched
        .clone()
        .end_placement_lost()
        .expect_err("loss cannot rewrite a dispatched attempt");
    assert_eq!(rejection.attempt(), &dispatched);
}

struct ForegroundWaitFixture {
    request: ToolRequest,
    spawning_request: ToolRequestId,
    child: crate::SessionId,
    attempt: ReconstitutedToolAttempt,
}

// Arbitrary identities descend for requests so identity order differs from proposal order.
fn foreground_wait_fixture(ordinal: u32) -> ForegroundWaitFixture {
    let seed = u128::from(ordinal);
    let request = request(100 - seed, ordinal);
    let spawning_request = tool_request_id(300 + seed);
    let child = session_id(200 + seed);
    let attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(500 + seed),
        request.id(),
        session_id(1),
        turn_id(2),
        turn_attempt_id(400 + seed),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::AwaitingChild {
            spawning_request,
            child,
        }),
    )
    .reconstitute()
    .expect("the recorded wait has an exact dispatch");
    ForegroundWaitFixture {
        request,
        spawning_request,
        child,
        attempt,
    }
}

fn foreground_batch_input(
    waits: &[ForegroundWaitFixture],
    phase: ToolBatchPhaseReconstitutionInput,
) -> ToolBatchReconstitutionInput {
    ToolBatchReconstitutionInput::new(
        session_id(1),
        turn_id(2),
        model_call_id(3),
        yielded_snapshot(),
        waits.iter().map(|wait| wait.request.clone()).collect(),
        waits
            .iter()
            .map(|wait| automatic_approval(wait.request.id()))
            .collect(),
        waits.iter().map(|wait| wait.attempt.clone()).collect(),
        phase,
    )
}

#[test]
fn later_foreground_wait_reconstitutes_after_an_earlier_delivery() {
    let waits = [foreground_wait_fixture(0), foreground_wait_fixture(1)];
    let second = &waits[1];
    let batch = foreground_batch_input(
        &waits,
        ToolBatchPhaseReconstitutionInput::AwaitingChild {
            request: second.request.id(),
            spawning_request: second.spawning_request,
            child: second.child,
        },
    )
    .reconstitute()
    .expect("an earlier delivered wait remains in the same batch");
    assert_eq!(
        batch.phase(),
        ToolBatchPhase::AwaitingChild {
            request: second.request.id(),
            spawning_request: second.spawning_request,
            child: second.child,
        }
    );
}

#[test]
fn foreground_results_follow_proposal_order_with_each_child_outcome() {
    let waits = [foreground_wait_fixture(0), foreground_wait_fixture(1)];
    let batch = foreground_batch_input(
        &waits,
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(600),
        },
    )
    .reconstitute()
    .expect("both waits resumed under a fresh turn attempt");
    let completed = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ResultReturned,
        Some(
            DelegationContent::try_new(String::from("first child result")).expect("bounded result"),
        ),
        DelegationOutcomeReason::ChildCompleted,
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: waits[0].child,
            turn: turn_id(610),
        },
    )
    .expect("completed child has exact provenance");
    let failed = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ChildFailed,
        None,
        DelegationOutcomeReason::ChildResultUnavailable,
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: waits[1].child,
            turn: turn_id(611),
        },
    )
    .expect("failed child has exact provenance");
    let projection = batch
        .prepare_delegation_result_projection(
            vec![
                semantic_transcript_entry_id(620),
                semantic_transcript_entry_id(621),
            ],
            context_frontier_id(630),
            [
                (waits[1].request.id(), failed.clone()),
                (waits[0].request.id(), completed.clone()),
            ]
            .into(),
        )
        .expect("both child results close in proposal order");
    assert_eq!(projection.entries().len(), waits.len());
    for ((entry, wait), expected) in projection
        .entries()
        .iter()
        .zip(&waits)
        .zip([completed, failed])
    {
        assert_eq!(
            entry.payload(),
            &SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request: wait.request.id(),
                spawning_request: wait.spawning_request,
                child: wait.child,
                mode: crate::DelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: Box::new(expected),
            },
            "await request {:?}",
            wait.request.id()
        );
    }
}

#[test]
fn cancelling_later_wait_preserves_the_earlier_delivered_result() {
    let waits = [foreground_wait_fixture(0), foreground_wait_fixture(1)];
    let second = &waits[1];
    let batch = foreground_batch_input(
        &waits,
        ToolBatchPhaseReconstitutionInput::AwaitingChild {
            request: second.request.id(),
            spawning_request: second.spawning_request,
            child: second.child,
        },
    )
    .reconstitute()
    .expect("the second child is still awaited");
    let delivered = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ResultReturned,
        Some(
            DelegationContent::try_new(String::from("delivered before stop"))
                .expect("bounded result"),
        ),
        DelegationOutcomeReason::ChildCompleted,
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: waits[0].child,
            turn: turn_id(640),
        },
    )
    .expect("the earlier child result has exact provenance");
    let projection = batch
        .prepare_delegation_cancellation_projection(
            vec![
                semantic_transcript_entry_id(650),
                semantic_transcript_entry_id(651),
            ],
            context_frontier_id(660),
            [(waits[0].request.id(), delivered.clone())].into(),
        )
        .expect("stop preserves delivered results and closes the unresolved wait");
    assert_eq!(
        projection.entries()[0].payload(),
        &SemanticTranscriptEntryPayload::DelegationResult {
            awaiting_request: waits[0].request.id(),
            spawning_request: waits[0].spawning_request,
            child: waits[0].child,
            mode: crate::DelegationWaitMode::Foreground,
            delivery_sequence: None,
            outcome: Box::new(delivered),
        }
    );
    assert_eq!(
        projection.entries()[1].payload(),
        &SemanticTranscriptEntryPayload::ToolClosed {
            request: second.request.id(),
        }
    );
}
