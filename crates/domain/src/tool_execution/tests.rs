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
    ToolApprovalResolutionReconstitutionInput, ToolArgumentsKind, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolDecisionSource, ToolDispatchGeneration, ToolName,
    ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultContent, ToolResultText,
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

/// S10: user decisions advance exactly one
/// earliest wait and retain explicit user provenance.
#[test]
fn s10_user_decision_advances_to_next_wait() {
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

/// S10: durable approval history is exactly a proposal-order
/// prefix and cannot skip the current wait.
#[test]
fn s10_reconstitution_rejects_nonprefix_approval_inventory() {
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

/// S10: reconstitution enforces the same 32-request bound as
/// provider-response admission instead of granting authority to oversized
/// stored batches.
#[test]
fn s10_reconstitution_rejects_oversized_request_batch() {
    let requests = (0..33)
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

    let error = input
        .reconstitute()
        .expect_err("stored batches above the response bound are rejected");
    assert_eq!(
        error.failure(),
        ToolBatchReconstitutionFailure::TooManyRequests
    );
}

/// S10: model-call completion may freeze automatic
/// approval for a later request while an earlier confirmation still waits.
#[test]
fn s10_later_automatic_approval_survives_reconstitution() {
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

/// S10: a user decision is admissible only at the exact
/// durable approval wait and cannot manufacture a wait from execution.
#[test]
fn s10_user_decision_rejects_nonwaiting_batch_unchanged() {
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

/// S10: one active batch cannot turn an existing request from a
/// different aggregate into a user-global not-found result.
#[test]
fn s10_out_of_batch_decision_is_a_correlation_error() {
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

/// S10: serialized execution prepares only the first
/// approved request without terminal attempt evidence.
#[test]
fn s10_execution_prepares_first_unattempted_request() {
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

/// S06: only a completely reconstituted ambiguous
/// batch can expose the exact tool recovery-wait subject.
#[test]
fn s06_ambiguous_batch_exposes_opaque_recovery_wait() {
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

/// S06: impossible effect-free ambiguity cannot
/// manufacture recovery-wait authority during checked reconstitution.
#[test]
fn s06_effect_free_ambiguous_history_fails_closed() {
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

/// S10: a live serialized attempt is the last
/// attempt that can exist in proposal order.
#[test]
fn s10_reconstitution_rejects_attempt_after_live_attempt() {
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

/// S06: recovery evidence belongs to one issuing
/// continuation tenure throughout the complete batch.
#[test]
fn s06_recovery_rejects_mixed_issuing_attempts() {
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

/// S05: crash-lost evidence is a turn-level blocker,
/// so no later approved request can be prepared or already attempted.
#[test]
fn s05_crash_loss_stops_serial_batch_execution() {
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

/// S11: result projection uses only attempt/request
/// references and preserves proposal order.
#[test]
fn s11_result_projection_is_reference_only_and_ordered() {
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

/// S17: a delivered foreground child wait reopens the
/// batch under a fresh turn attempt and projects the typed result once.
#[test]
fn s17_foreground_child_wait_resumes_and_projects_typed_result() {
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
            outcome.clone(),
        )
        .expect("the delivered child result closes the logical request");
    let interrupted = waiting
        .prepare_delegation_cancellation_projection(
            vec![semantic_transcript_entry_id(18)],
            context_frontier_id(19),
            None,
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

/// S06: terminal recovery closes every
/// logical request in proposal order without rewriting physical ambiguity.
#[test]
fn s06_reconciliation_projection_closes_ambiguity() {
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
/// S31: every clone of one checked batch shares one
/// runner-authorization issuance capability.
#[test]
fn s31_runner_authorization_is_single_use_across_batch_clones() {
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

/// S31: restored in-flight authority is also
/// single-use for runner conversion across clones of one checked batch.
#[test]
fn s31_in_flight_runner_authorization_is_single_use_across_clones() {
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

/// S31: reconstitution restores every retired
/// identity and rejects it as a later claimed-attempt replacement.
#[test]
fn s31_reconstituted_batch_rejects_retired_identity_reuse() {
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

/// S31: ordinary preparation rejects every durably retired identity.
#[test]
fn s31_ordinary_preparation_rejects_retired_identity_reuse() {
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

/// S31: retired and current inventories must be disjoint.
#[test]
fn s31_reconstitution_rejects_current_identity_as_retired() {
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
