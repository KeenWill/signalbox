//! Model execution tests for `docs/spec/model-call-execution.md`.

use std::num::NonZeroU64;

use super::tool_round::initial_tool_approval_matches_posture;
use super::*;
use crate::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnActivationIdentities,
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, AttachmentKind,
    BlobDigest, DeclaredMediaType, DelegationContent, DelegationOutcome, DelegationOutcomeKind,
    DelegationOutcomeReason, DelegationProvenanceReconstitutionInput, DeliveryRequest,
    ModelCallReconstitutionState, ModelSelectionOverride, ModelSelectionRequest,
    NormalizedToolArguments, PerInputConfigurationChoices, SemanticTranscriptEntryRef, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionReconstitutionInput, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptEnd, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolBatchPhaseReconstitutionInput,
    ToolBatchReconstitutionInput, ToolDispatchGeneration, ToolEffectClass, ToolExecutionError,
    ToolExecutionErrorKind, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
    TranscriptAncestry, UserContentPart,
    test_support::{
        accepted_input_id, command_id, context_frontier_id, direct, model_call_id,
        provider_model_identity, semantic_transcript_entry_id, session_id, tool_attempt_id,
        tool_request_id, turn_attempt_id, turn_id,
    },
};
use crate::{
    AttemptEnd, DangerousToolAutoApproval, InitialToolApproval, SteeringReclassificationReason,
    ToolRequest, ToolUsingAssistantResponse,
};
use crate::{FrozenModelSelection, ResolvedProviderTarget, ToolRequestId};

#[test]
fn always_confirm_approval_is_admitted_under_dangerous_blanket_posture() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::AlwaysConfirm,
    ));
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::SessionBlanket,
    ));
    assert!(!initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::Confirm,
    ));
}

#[test]
fn policy_auto_approval_is_admitted_under_dangerous_blanket_posture() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::PolicyAuto,
    ));
}

/// A configured `Delegated` posture now also satisfies an `AlwaysConfirm`
/// declaration, so `Delegated` reaches this admission check with the blanket
/// disabled and must be admitted there.
#[test]
fn delegated_approval_is_admitted_when_blanket_posture_is_disabled() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::Delegated,
    ));
}

/// The same configured `Delegated` posture reaches this check under the
/// dangerous blanket, which the `AlwaysConfirm` declaration refuses to honor
/// on its own.
#[test]
fn delegated_approval_is_admitted_under_dangerous_blanket_posture() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::Delegated,
    ));
}

/// A consumed user override is admitted wherever its `Delegated` base
/// selection is: the override substitutes for the judge, not for the
/// blanket, so neither frozen blanket posture contradicts it.
#[test]
fn user_override_approval_is_admitted_when_blanket_posture_is_disabled() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::UserOverride {
            command: command_id(1),
            denied_request: tool_request_id(2),
        },
    ));
}

/// See [`user_override_approval_is_admitted_when_blanket_posture_is_disabled`].
#[test]
fn user_override_approval_is_admitted_under_dangerous_blanket_posture() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::UserOverride {
            command: command_id(1),
            denied_request: tool_request_id(2),
        },
    ));
}

#[test]
fn human_approval_is_admitted_when_blanket_posture_is_disabled() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::Human,
    ));
}

#[test]
fn human_approval_is_admitted_under_dangerous_blanket_posture() {
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::ApproveAll,
        InitialToolApproval::Human,
    ));
}

#[test]
fn session_blanket_approval_is_rejected_when_blanket_posture_is_disabled() {
    assert!(!initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::SessionBlanket,
    ));
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::AlwaysConfirm,
    ));
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::Confirm,
    ));
    assert!(initial_tool_approval_matches_posture(
        DangerousToolAutoApproval::Disabled,
        InitialToolApproval::PolicyAuto,
    ));
}

fn active_execution() -> ModelCallExecution {
    let session_id = session_id(1);
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(2)));
    let session = SessionReconstitutionInput::new(
        session_id,
        session_id,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session_id,
        SessionConfigurationDefaultsVersion::first(),
        session_id,
        SessionConfigurationDefaultsVersion::first(),
        defaults,
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: session_id,
            current_pointer_version: crate::SessionPlacementVersion::INITIAL,
            selected_event_session: session_id,
            selected_event: crate::VersionedSessionPlacement::initial(
                crate::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("session facts are correlated");
    execution_from_activation(session)
}

fn delegation_result_entry(
    execution: &ModelCallExecution,
    mode: DelegationWaitMode,
) -> SemanticTranscriptEntry {
    let content = DelegationContent::try_new(String::from("checked child result"))
        .expect("fixture result content is valid");
    let outcome = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ResultReturned,
        Some(content),
        DelegationOutcomeReason::ChildCompleted,
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: session_id(31),
            turn: turn_id(32),
        },
    )
    .expect("child result tuple is canonical");
    SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(33),
        execution.session(),
        SemanticTranscriptEntryPayload::DelegationResult {
            awaiting_request: tool_request_id(35),
            spawning_request: tool_request_id(34),
            child: session_id(31),
            mode,
            delivery_sequence: None,
            outcome: Box::new(outcome),
        },
    )
}

#[test]
fn foreground_delegation_result_closes_exact_tool_continuation_round() {
    let execution = active_execution();
    let tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(30),
        execution.session(),
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(29),
            request: tool_request_id(35),
        },
    );
    let entries = execution
        .frontier_entries()
        .cloned()
        .chain([
            tool_use,
            delegation_result_entry(&execution, DelegationWaitMode::Foreground),
        ])
        .collect::<Vec<_>>();
    assert_eq!(
        frontier_closes_latest_tool_round(
            &execution.starting_snapshot,
            &entries,
            &BTreeMap::new(),
            &BTreeSet::new(),
        ),
        Ok(true)
    );
}

#[test]
fn background_delegation_result_does_not_complete_tool_continuation_round() {
    let execution = active_execution();
    let tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(30),
        execution.session(),
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(29),
            request: tool_request_id(35),
        },
    );
    let entries = execution
        .frontier_entries()
        .cloned()
        .chain([
            tool_use,
            delegation_result_entry(&execution, DelegationWaitMode::Background),
        ])
        .collect::<Vec<_>>();
    assert_eq!(
        frontier_closes_latest_tool_round(
            &execution.starting_snapshot,
            &entries,
            &BTreeMap::new(),
            &BTreeSet::new(),
        ),
        Ok(false)
    );
}

fn execution_from_activation(session: Session) -> ModelCallExecution {
    let checked = session
        .current_configuration_defaults()
        .derive_request(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        )
        .expect("defaults version is current");
    let configuration = OriginConfiguration::freeze(checked, |_| None)
        .expect("direct configuration needs no alias lookup");
    let turn = turn_id(3);
    let record = AcceptedInputTurnSchedulingRecord::new(
        session.id(),
        turn,
        session.id(),
        AcceptedInputLifecycle::new(
            accepted_input_id(4),
            AcceptedInputDisposition::OriginOf(turn),
        ),
        session.id(),
        turn,
        AcceptedInputQueueOrder::ordinary(crate::SessionInputPosition::first()),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
        configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let activation = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![record],
        Vec::new(),
        Vec::new(),
        None,
    )
    .reconstitute()
    .expect("queued scheduling projection is complete")
    .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
        semantic_transcript_entry_id(99),
        semantic_transcript_entry_id(5),
        context_frontier_id(6),
        turn_attempt_id(7),
    ))
    .expect("first turn is eligible");
    let (turn, entries, snapshot) = activation.into_parts();
    let origin = entries
        .last()
        .expect("activation carries its origin")
        .clone();
    ModelCallExecutionReconstitutionInput::new(
        turn,
        targets(),
        snapshot,
        vec![origin],
        vec![ModelCallOriginContent::from_validated_parts(
            accepted_input_id(4),
            UserContent::try_text(String::from("hello")).expect("test content is valid"),
        )],
        None,
        Vec::new(),
    )
    .reconstitute()
    .expect("activation facts reconstruct live execution")
}

fn attachment_execution_input(
    facts: Vec<AttachmentBlobFact>,
) -> ModelCallExecutionReconstitutionInput {
    let execution = active_execution();
    let digest = BlobDigest::digest(b"attachment fixture bytes");
    let content = UserContent::try_parts(vec![UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: None,
    }])
    .expect("the attachment-only fixture is valid");
    ModelCallExecutionReconstitutionInput::new(
        execution.active_turn,
        execution.targets,
        execution.starting_snapshot,
        execution.frontier_entries.into_vec(),
        vec![ModelCallOriginContent::from_validated_parts(
            accepted_input_id(4),
            content,
        )],
        None,
        Vec::new(),
    )
    .with_attachment_blob_facts(facts)
}

/// model preparation admits immutable catalog facts when they exactly cover every referenced
/// attachment digest.
#[test]
fn exact_attachment_catalog_facts_reach_preparation() {
    let digest = BlobDigest::digest(b"attachment fixture bytes");
    let length = NonZeroU64::new(24).expect("the fixture length is positive");

    let execution = attachment_execution_input(vec![AttachmentBlobFact::new(digest, length)])
        .reconstitute()
        .expect("the exact attachment catalog projection is complete");
    let request = execution
        .preview_initial_call(model_call_id(9))
        .expect("the covered attachment can reach call preparation");

    assert_eq!(request.attachment_byte_length(digest), Some(length));
}

/// model preparation fails closed when the catalog projection
/// omits a referenced attachment digest.
#[test]
fn missing_attachment_catalog_fact_fails_preparation() {
    let missing = attachment_execution_input(Vec::new())
        .reconstitute()
        .expect_err("a missing attachment catalog fact fails closed");

    assert_eq!(
        missing.failure(),
        ModelCallExecutionReconstitutionFailure::AttachmentBlobFactMismatch
    );
}

fn targets() -> ModelTargetCatalog {
    ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct(2),
        ResolvedProviderTarget::naming(provider_model_identity(8)),
    )])
    .expect("one definition is unique")
}

fn prepared_execution() -> ModelCallExecution {
    let initial = active_execution();
    let prepared = initial
        .clone()
        .prepare_initial_call(model_call_id(9))
        .expect("initial prepared checkpoint is valid");
    ModelCallExecutionReconstitutionInput::new(
        initial.active_turn.clone(),
        initial.targets.clone(),
        initial.starting_snapshot.clone(),
        initial.frontier_entries.to_vec(),
        initial
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            prepared.call().turn(),
            prepared.call().target(),
        )),
        vec![ModelCallReconstitutionInput::new(
            prepared.call().id(),
            prepared.call().turn(),
            prepared.call().attempt(),
            prepared.call().selection(),
            prepared.call().target(),
            prepared.call().frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .reconstitute()
    .expect("prepared facts reconstruct")
}

fn prepared_execution_consuming_steering() -> ModelCallExecution {
    let mut initial = active_execution();
    let accepted_input = accepted_input_id(20);
    let acceptance_position =
        crate::SessionInputPosition::try_from_u64(2).expect("the steering position is positive");
    initial.active_turn = initial.active_turn.with_pending_steering_for_test(
        vec![(accepted_input, acceptance_position)].into_boxed_slice(),
    );
    initial.origin_contents.insert(
        accepted_input,
        UserContent::try_text(String::from("steer")).expect("steering content is valid"),
    );
    let active_turn = initial.active_turn.clone();
    let targets = initial.targets.clone();
    let starting_snapshot = initial.starting_snapshot.clone();
    let mut frontier_entries = initial.frontier_entries.to_vec();
    let origin_contents = initial
        .origin_contents
        .iter()
        .map(|(accepted_input, content)| {
            ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
        })
        .collect();
    let call_id = model_call_id(9);
    let prepared = initial
        .prepare_initial_call_consuming_steering(
            call_id,
            vec![semantic_transcript_entry_id(22)],
            Some(context_frontier_id(24)),
        )
        .expect("steering may be consumed by a prepared call");
    frontier_entries.extend(
        prepared
            .consumed_steering()
            .iter()
            .map(|consumed| consumed.semantic_entry().clone()),
    );
    let call = prepared.call();
    let call_snapshot = prepared
        .steering_snapshot()
        .expect("steering creates a call snapshot");
    ModelCallExecutionReconstitutionInput::new(
        active_turn.with_consumed_steering_for_test(
            vec![(accepted_input, acceptance_position, call_id)].into_boxed_slice(),
        ),
        targets,
        starting_snapshot,
        frontier_entries,
        origin_contents,
        Some(PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )),
        vec![ModelCallReconstitutionInput::new(
            call.id(),
            call.turn(),
            call.attempt(),
            call.selection(),
            call.target(),
            call.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        session_id(1),
        call_snapshot.frontier().snapshot(),
        call_snapshot.ordered_entries().collect(),
    ))
    .reconstitute()
    .expect("a steering-consuming prepared call reconstructs")
}

fn in_flight_execution() -> ModelCallExecution {
    let prepared = prepared_execution();
    let authorized = prepared
        .clone()
        .authorize_send()
        .expect("prepared execution may authorize send");
    ModelCallExecutionReconstitutionInput::new(
        prepared
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: authorized.attempt.clone(),
            }),
        prepared.targets.clone(),
        prepared.starting_snapshot.clone(),
        prepared.frontier_entries.to_vec(),
        prepared
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            authorized.call.turn(),
            authorized.call.target(),
        )),
        vec![ModelCallReconstitutionInput::new(
            authorized.call.id(),
            authorized.call.turn(),
            authorized.call.attempt(),
            authorized.call.selection(),
            authorized.call.target(),
            authorized.call.frontier().snapshot(),
            ModelCallReconstitutionState::InFlight,
        )],
    )
    .reconstitute()
    .expect("in-flight facts reconstruct")
}

fn reconstitution_input_with_calls(
    execution: &ModelCallExecution,
    calls: Vec<ModelCallReconstitutionInput>,
) -> ModelCallExecutionReconstitutionInput {
    let pinned_target = execution
        .current_call()
        .map(|call| PinnedProviderTargetReconstitutionInput::new(call.turn(), call.target()));
    ModelCallExecutionReconstitutionInput::new(
        execution
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: execution.current_attempt.clone(),
            }),
        execution.targets.clone(),
        execution.starting_snapshot.clone(),
        execution.frontier_entries.to_vec(),
        execution
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        pinned_target,
        calls,
    )
}

fn correlated_observation(
    execution: &ModelCallExecution,
    observation: ModelCallTerminalObservation,
) -> CorrelatedModelCallTerminalObservation {
    let call = execution
        .current_call()
        .expect("a correlated test observation requires one live call");
    CorrelatedModelCallTerminalObservation {
        correlation: IssuedModelCallCorrelation {
            session: execution.session(),
            turn: execution.turn(),
            attempt: execution.current_attempt().id(),
            call: call.id(),
            target: call.target(),
            frontier: call.frontier().snapshot(),
        },
        observation,
        usage: ProviderReportedTokenUsage::unreported(),
        provider_failure_cause: None,
        retry_after: None,
        non_acceptance_proven: false,
        rate_limits: None,
    }
}

/// Canonical sealed completion fixture: the live call derives up to two
/// assistant entries from identities 10 and 11, terminal entry 12, and
/// terminal frontier 13 from the existing session-1, turn-3, call-9
/// execution fixture.
pub(crate) fn completed_turn_fixture(values: &[&str]) -> CompletedModelCallTurn {
    assert!(values.len() <= 2, "fixture has two assistant identities");
    let execution = in_flight_execution();
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::Completed {
            assistant_text: values
                .iter()
                .map(|value| {
                    crate::AssistantText::try_new((*value).to_owned())
                        .expect("nonempty fixture text")
                })
                .collect(),
        },
    );
    let assistant_entries = values
        .iter()
        .enumerate()
        .map(|(ordinal, _)| semantic_transcript_entry_id(10 + ordinal as u128))
        .collect();
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                assistant_entries,
                semantic_transcript_entry_id(12),
                context_frontier_id(13),
            )),
        )
        .expect("definitive fixture completion is admissible");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("completed fixture evidence selects completed outcome");
    };
    completed
}

pub(crate) fn completed_turn_with_provider_compaction_fixture(
    value: &str,
) -> CompletedModelCallTurn {
    let execution = in_flight_execution();
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithProviderCompaction {
            response: vec![
                AssistantResponsePart::ProviderCompaction(
                    crate::ProviderCompactionBlock::try_new(
                        r#"{"type":"compaction","content":"summary"}"#.to_string(),
                    )
                    .expect("fixture compaction block is complete"),
                ),
                AssistantResponsePart::Text(
                    crate::AssistantText::try_new(value.to_owned()).expect("nonempty fixture text"),
                ),
            ],
            retained_input_tokens: 23,
            retained_output_tokens: 5,
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![
                    semantic_transcript_entry_id(10),
                    semantic_transcript_entry_id(11),
                ],
                semantic_transcript_entry_id(12),
                context_frontier_id(13),
            )),
        )
        .expect("provider compaction fixture completion is admissible");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("provider compaction evidence selects completed outcome");
    };
    completed
}

#[test]
fn reasoning_completion_rejects_compaction_without_retained_iteration_usage() {
    let execution = in_flight_execution();
    let compaction = crate::ProviderCompactionBlock::try_new(
        r#"{"type":"compaction","content":"summary"}"#.to_owned(),
    )
    .expect("fixture compaction block is complete");
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithProviderReasoning {
            response: vec![AssistantResponsePart::ProviderCompaction(compaction)],
        },
    );
    let result = execution.apply_terminal_observation(
        observation,
        ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
            vec![semantic_transcript_entry_id(10)],
            semantic_transcript_entry_id(12),
            context_frontier_id(13),
        )),
    );
    assert_eq!(
        result,
        Err(ModelCallClosureError::UnexpectedProviderCompaction)
    );
}

pub(crate) fn completed_turn_with_provider_reasoning_fixture(
    value: &str,
) -> CompletedModelCallTurn {
    let execution = in_flight_execution();
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithProviderReasoning {
            response: vec![
                AssistantResponsePart::ProviderReasoning(
                    crate::ProviderReasoningItem::try_new(
                        r#"{"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"opaque"}"#.to_string(),
                    )
                    .expect("fixture reasoning item is complete"),
                ),
                AssistantResponsePart::Text(
                    crate::AssistantText::try_new(value.to_owned()).expect("nonempty fixture text"),
                ),
            ],
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![
                    semantic_transcript_entry_id(10),
                    semantic_transcript_entry_id(11),
                ],
                semantic_transcript_entry_id(12),
                context_frontier_id(13),
            )),
        )
        .expect("provider reasoning fixture completion is admissible");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("provider reasoning evidence selects completed outcome");
    };
    completed
}

/// Canonical sealed failure fixture for the existing session-1, turn-3
/// active execution.
pub(crate) fn failed_turn_fixture() -> FailedModelCallTurn {
    let mut execution = active_execution();
    execution.targets =
        ModelTargetCatalog::try_from_definitions([]).expect("empty fixture catalog is valid");
    let preparation = execution
        .prepare_initial_call(model_call_id(9))
        .expect_err("empty fixture catalog cannot resolve a target");
    let proof = preparation
        .target_resolution_error()
        .expect("fixture failure retains target-resolution evidence");
    preparation
        .execution()
        .clone()
        .fail_target_resolution(
            proof,
            FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            ),
        )
        .expect("matching fixture proof closes the turn as failed")
}

/// Canonical sealed cancellation fixture for the existing session-1,
/// turn-3 active execution.
pub(crate) fn cancelled_turn_fixture() -> CancelledModelCallTurn {
    let execution = active_execution();
    let interrupt = applied_interrupt(&execution);
    let outcome = execution
        .apply_interrupt(
            interrupt,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(33),
                context_frontier_id(34),
            ),
        )
        .expect("matching fixture interrupt cancels unsent work");
    let ModelCallInterruptOutcome::Cancelled(cancelled) = outcome else {
        panic!("unsent fixture work closes as cancelled");
    };
    cancelled
}

fn tool_proposal(name: &str, arguments: &str) -> crate::ToolCallProposal {
    crate::ToolCallProposal::new(
        ToolName::try_new(name.to_owned()).expect("test tool names are canonical"),
        NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
            .expect("test arguments fit the admission bound"),
    )
}

fn batch_request(id: u128, execution: &ModelCallExecution) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        tool_request_id(id),
        execution.session(),
        execution.turn(),
        model_call_id(40),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("fixture_tool")).expect("the tool name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("the fixture arguments are canonical"),
    )
    .into_request()
}

fn denied_approval(request: ToolRequestId) -> ToolApprovalResolution {
    ToolApprovalResolutionReconstitutionInput::user_fixture(
        request,
        ToolApprovalDecision::Deny { reason: None },
    )
    .reconstitute()
    .expect("the denial fixture is implemented")
}

fn with_pending_steering(
    mut execution: ModelCallExecution,
    pending: AcceptedInputId,
) -> ModelCallExecution {
    execution.active_turn = execution.active_turn.with_pending_steering_for_test(
        vec![(
            pending,
            crate::SessionInputPosition::try_from_u64(2)
                .expect("the test steering position is positive"),
        )]
        .into_boxed_slice(),
    );
    execution
}

fn one_reclassification(
    pending: AcceptedInputId,
    turn: TurnId,
) -> Vec<PendingSteeringReclassificationIdentity> {
    vec![PendingSteeringReclassificationIdentity::new(pending, turn)]
}

fn applied_interrupt(execution: &ModelCallExecution) -> AppliedInterruptCommandResult {
    AppliedInterruptCommandResult::from_correlated_submit(
        crate::test_support::command_id(30),
        execution.session(),
        execution.turn(),
        accepted_input_id(31),
        turn_id(32),
        AcceptedInputQueueOrder::interrupt_immediately_after(
            crate::SessionInputPosition::try_from_u64(2)
                .expect("the interrupt acceptance position is positive"),
            execution.turn(),
        ),
    )
    .expect("the fixture interrupt is exactly correlated")
}

fn stop_requested_execution(
    execution: ModelCallExecution,
) -> (ModelCallExecution, AppliedInterruptCommandResult) {
    let interrupt = applied_interrupt(&execution);
    let outcome = execution
        .clone()
        .apply_interrupt(
            interrupt,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(33),
                context_frontier_id(34),
            ),
        )
        .expect("an issued call accepts the matching interrupt");
    let ModelCallInterruptOutcome::CancellationRequested(stopped) = outcome else {
        panic!("issued work requests physical cancellation");
    };
    let mut reloaded = execution;
    reloaded.current_attempt = stopped.attempt().clone();
    reloaded.current_call = Some(stopped.call().clone());
    reloaded.active_turn = reloaded
        .active_turn
        .with_phase_for_test(ActiveTurnPhase::Running {
            current_attempt: stopped.attempt().clone(),
        });
    (reloaded, interrupt)
}

fn assert_one_reclassified_turn(
    reclassified: &[ReclassifiedPendingSteeringTurn],
    pending: AcceptedInputId,
    source_turn: TurnId,
    successor: TurnId,
) {
    assert_eq!(reclassified.len(), 1);
    let reclassified = &reclassified[0];
    assert_eq!(reclassified.session(), session_id(1));
    assert_eq!(reclassified.source_turn(), source_turn);
    assert_eq!(reclassified.accepted_input().id(), pending);
    assert_eq!(reclassified.turn(), successor);
    assert_eq!(
        reclassified.accepted_input().disposition(),
        &AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
            turn: successor,
            reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
        }
    );
    assert_eq!(reclassified.binding().source_turn(), source_turn);
    assert_eq!(
        reclassified.order(),
        AcceptedInputQueueOrder::ordinary(
            crate::SessionInputPosition::try_from_u64(2)
                .expect("the test steering position is positive")
        )
    );
    assert_eq!(
        reclassified.effective_configuration().model(),
        &FrozenModelSelection::Direct(direct(2))
    );
}

/// a complete frontier read must preserve exact semantic order, not merely the same entry
/// membership.
#[test]
fn reconstitution_rejects_reordered_frontier_entries() {
    let execution = active_execution();
    let first = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(20),
        execution.session,
        SemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: accepted_input_id(21),
        },
    );
    let second = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(22),
        execution.session,
        SemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: accepted_input_id(23),
        },
    );
    let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        execution.session,
        context_frontier_id(24),
        vec![first.reference(), second.reference()],
    )
    .expect("ordered test frontier is valid");
    let start = AcceptedInputTurnStart::from_validated_eligibility(
        crate::AcceptedInputStartingLineage::FirstInSession,
        snapshot.frontier(),
    );
    let input = ModelCallExecutionReconstitutionInput::new(
        execution.active_turn.with_start_for_test(start),
        execution.targets.clone(),
        snapshot,
        vec![second, first],
        vec![
            ModelCallOriginContent::from_validated_parts(
                accepted_input_id(21),
                UserContent::try_text(String::from("first")).expect("valid text"),
            ),
            ModelCallOriginContent::from_validated_parts(
                accepted_input_id(23),
                UserContent::try_text(String::from("second")).expect("valid text"),
            ),
        ],
        None,
        Vec::new(),
    );

    let error = input
        .reconstitute()
        .expect_err("same membership in another order is not the stored frontier");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::FrontierEntryMismatch
    );
}

/// an execution snapshot must be the exact eligibility-fixed turn start, not another same-content
/// frontier.
#[test]
fn reconstitution_rejects_nonstarting_snapshot() {
    let execution = active_execution();
    let other_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        execution.session,
        context_frontier_id(25),
        execution.starting_snapshot.ordered_entries().collect(),
    )
    .expect("same-content test snapshot is valid");
    let input = ModelCallExecutionReconstitutionInput::new(
        execution.active_turn.clone(),
        execution.targets.clone(),
        other_snapshot,
        execution.frontier_entries.to_vec(),
        execution
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        None,
        Vec::new(),
    );

    let error = input
        .reconstitute()
        .expect_err("a same-content snapshot is not the fixed starting frontier");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::StartingSnapshotMismatch
    );
}

/// a fresh continuation attempt admits its call-free result frontier only inside the transaction
/// that will insert the prepared continuation call.
#[test]
fn continuation_reconstitutes_exact_frontier_and_pin() {
    let initial = active_execution();
    let request = tool_request_id(30);
    let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(31),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(32),
            request,
        },
    );
    let denied = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(33),
        initial.session,
        SemanticTranscriptEntryPayload::ToolDenied { request },
    );
    let yielded = initial
        .starting_snapshot
        .derive_appending_candidate(
            context_frontier_id(34),
            vec![assistant_tool_use.reference()],
        )
        .expect("tool proposal preserves the starting prefix");
    let continuation = yielded
        .derive_appending_candidate(context_frontier_id(37), vec![denied.reference()])
        .expect("tool denial extends the yielded frontier");
    let projection = PreparedToolResultProjection::from_validated_parts(
        yielded.frontier().snapshot(),
        initial.turn,
        model_call_id(32),
        vec![denied.clone()],
        continuation.clone(),
    );
    let pinned = PinnedProviderTargetReconstitutionInput::new(
        initial.turn,
        ResolvedProviderTarget::naming(provider_model_identity(8)),
    );
    let input = ModelCallExecutionReconstitutionInput::new(
        initial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(35))
                    .begin_running()
                    .expect("tool dispatch starts the continuation tenure"),
            }),
        initial.targets,
        initial.starting_snapshot,
        vec![
            initial.frontier_entries[0].clone(),
            assistant_tool_use,
            denied,
        ],
        initial
            .origin_contents
            .into_iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(accepted_input, content)
            })
            .collect(),
        Some(pinned),
        Vec::new(),
    )
    .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        continuation.frontier().owning_session(),
        continuation.frontier().snapshot(),
        continuation.ordered_entries().collect(),
    ))
    .with_tool_denial_correlations(vec![denied_approval(request)]);
    let mut missing_denial = input.clone();
    missing_denial.tool_denial_correlations.clear();
    assert_eq!(
        missing_denial
            .reconstitute()
            .expect_err("a denial entry requires its exact durable resolution")
            .failure(),
        ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch
    );
    let mut approved_instead = input.clone();
    approved_instead.tool_denial_correlations = vec![
        ToolApprovalResolutionReconstitutionInput::user_fixture(
            request,
            ToolApprovalDecision::Approve,
        )
        .reconstitute()
        .expect("the mismatching approval fixture is valid"),
    ];
    assert_eq!(
        approved_instead
            .reconstitute()
            .expect_err("approval authority cannot back a denial entry")
            .failure(),
        ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch
    );
    assert_eq!(
        input
            .clone()
            .reconstitute()
            .expect_err("a durably visible resolved frontier requires its prepared call")
            .failure(),
        ModelCallExecutionReconstitutionFailure::LifecycleMismatch
    );
    let mut prepared_denial = input.clone();
    prepared_denial.active_turn =
        prepared_denial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(35)),
            });
    assert_eq!(
        prepared_denial
            .clone()
            .reconstitute()
            .expect_err("a prepared denial continuation still requires atomic projection proof")
            .failure(),
        ModelCallExecutionReconstitutionFailure::LifecycleMismatch
    );
    let prepared_denial =
        prepared_denial.with_uncommitted_tool_result_projection(projection.clone());
    let prepared_denial_call = prepared_denial
        .clone()
        .reconstitute()
        .expect("a denied-only continuation preserves its prepared attempt")
        .prepare_initial_call(model_call_id(39))
        .expect("the prepared denial continuation admits its next call");
    let mut prepared_denial_reload = prepared_denial;
    prepared_denial_reload.call_snapshot = prepared_denial_reload.continuation_snapshot.take();
    prepared_denial_reload.uncommitted_tool_result_projection = None;
    prepared_denial_reload.calls = vec![ModelCallReconstitutionInput::new(
        prepared_denial_call.call().id(),
        prepared_denial_call.turn(),
        prepared_denial_call.attempt(),
        prepared_denial_call.call().selection(),
        prepared_denial_call.call().target(),
        prepared_denial_call.call().frontier().snapshot(),
        ModelCallReconstitutionState::Prepared,
    )];
    assert_eq!(
        prepared_denial_reload
            .reconstitute()
            .expect("the denied-only prepared continuation reloads")
            .resume_prepared_call()
            .expect("the denial continuation call remains resumable")
            .call()
            .id(),
        prepared_denial_call.call().id()
    );
    let input = input.with_uncommitted_tool_result_projection(projection);
    let resumed = input
        .clone()
        .reconstitute()
        .expect("the exact transaction-local projection reconstructs");
    let crash_failure_entry = semantic_transcript_entry_id(37);
    let crash_failed = input
        .clone()
        .reconstitute()
        .expect("the exact tool frontier reconstructs for crash closure")
        .recover_tool_crash_after_restart(FailedModelCallTurnIdentities::new(
            crash_failure_entry,
            context_frontier_id(38),
        ))
        .expect("tool crash failure extends the current result frontier");
    assert_eq!(
        crash_failed
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            SemanticTranscriptEntryRef::from_source(session_id(1), semantic_transcript_entry_id(5),),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(31),
            ),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(33),
            ),
            SemanticTranscriptEntryRef::from_source(session_id(1), crash_failure_entry),
        ]
    );
    let prepared = resumed
        .prepare_initial_call(model_call_id(36))
        .expect("the continuation attempt prepares its next call");

    assert_eq!(prepared.call().frontier(), continuation.frontier());
    assert_eq!(
        prepared.call().target(),
        ResolvedProviderTarget::naming(provider_model_identity(8))
    );
    assert!(prepared.steering_snapshot().is_none());

    let mut prepared_input = input;
    prepared_input.call_snapshot = prepared_input.continuation_snapshot.take();
    prepared_input.uncommitted_tool_result_projection = None;
    prepared_input.calls = vec![ModelCallReconstitutionInput::new(
        prepared.call().id(),
        prepared.turn(),
        prepared.attempt(),
        prepared.call().selection(),
        prepared.call().target(),
        prepared.call().frontier().snapshot(),
        ModelCallReconstitutionState::Prepared,
    )];
    let reloaded = prepared_input
        .reconstitute()
        .expect("a running tool tenure may own its prepared continuation call");
    assert_eq!(
        reloaded
            .resume_prepared_call()
            .expect("the prepared continuation resumes")
            .call()
            .id(),
        prepared.call().id()
    );
    let authorized = reloaded
        .authorize_send()
        .expect("send authorization keeps the existing running tenure");
    assert_eq!(
        authorized.attempt().state(),
        &CurrentTurnAttemptState::Running
    );
    assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
}

/// each continuation result must name the physical attempt that executed its exact request in the
/// producing model-call batch.
#[test]
fn continuation_rejects_duplicate_attempt_for_two_requests() {
    let initial = active_execution();
    let producing_call = model_call_id(70);
    let first_request = tool_request_id(71);
    let second_request = tool_request_id(72);
    let first_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(73),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call,
            request: first_request,
        },
    );
    let second_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(74),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call,
            request: second_request,
        },
    );
    let shared_attempt = tool_attempt_id(75);
    let first_result = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(76),
        initial.session,
        SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: shared_attempt,
        },
    );
    let second_result = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(77),
        initial.session,
        SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: shared_attempt,
        },
    );
    let continuation = initial
        .starting_snapshot
        .derive_appending_candidate(
            context_frontier_id(78),
            vec![
                first_use.reference(),
                second_use.reference(),
                first_result.reference(),
                second_result.reference(),
            ],
        )
        .expect("the malformed candidate still preserves the starting prefix");
    let input = ModelCallExecutionReconstitutionInput::new(
        initial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(79))
                    .begin_running()
                    .expect("the tool tenure is running"),
            }),
        initial.targets,
        initial.starting_snapshot,
        vec![
            initial.frontier_entries[0].clone(),
            first_use,
            second_use,
            first_result,
            second_result,
        ],
        initial
            .origin_contents
            .into_iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(accepted_input, content)
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            initial.turn,
            ResolvedProviderTarget::naming(provider_model_identity(8)),
        )),
        Vec::new(),
    )
    .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        continuation.frontier().owning_session(),
        continuation.frontier().snapshot(),
        continuation.ordered_entries().collect(),
    ))
    .with_tool_result_correlations(vec![ToolResultAttemptCorrelation::new(
        shared_attempt,
        first_request,
        producing_call,
    )]);

    let error = input
        .reconstitute()
        .expect_err("one physical attempt cannot resolve two logical requests");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch
    );
}

/// a prepared continuation belongs to the most recent tool round and cannot reuse results from an
/// earlier round.
#[test]
fn continuation_rejects_unresolved_latest_tool_round() {
    let initial = active_execution();
    let earlier_request = tool_request_id(30);
    let latest_request = tool_request_id(40);
    let earlier_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(31),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(32),
            request: earlier_request,
        },
    );
    let earlier_denial = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(33),
        initial.session,
        SemanticTranscriptEntryPayload::ToolDenied {
            request: earlier_request,
        },
    );
    let latest_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(34),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(35),
            request: latest_request,
        },
    );
    let continuation = initial
        .starting_snapshot
        .derive_appending_candidate(
            context_frontier_id(36),
            vec![
                earlier_use.reference(),
                earlier_denial.reference(),
                latest_use.reference(),
            ],
        )
        .expect("the malformed continuation still preserves its prefix");
    let attempt = turn_attempt_id(37);
    let turn = initial.turn;
    let selection = *initial.configuration.effective().model();
    let target = ResolvedProviderTarget::naming(provider_model_identity(8));
    let input = ModelCallExecutionReconstitutionInput::new(
        initial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(attempt)
                    .begin_running()
                    .expect("the tool tenure is running"),
            }),
        initial.targets.clone(),
        initial.starting_snapshot.clone(),
        vec![
            initial.frontier_entries[0].clone(),
            earlier_use,
            earlier_denial,
            latest_use,
        ],
        initial
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(turn, target)),
        vec![ModelCallReconstitutionInput::new(
            model_call_id(38),
            turn,
            attempt,
            selection,
            target,
            continuation.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        continuation.frontier().owning_session(),
        continuation.frontier().snapshot(),
        continuation.ordered_entries().collect(),
    ))
    .with_tool_denial_correlations(vec![denied_approval(earlier_request)]);

    let error = input
        .reconstitute()
        .expect_err("an old result cannot close the latest tool round");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::LifecycleMismatch
    );
}

/// a cancellation-only close marker is terminal history and cannot satisfy ordinary continuation
/// resolution.
#[test]
fn continuation_rejects_tool_closed() {
    let initial = active_execution();
    let request = tool_request_id(30);
    let attempt = turn_attempt_id(35);
    let call = model_call_id(36);
    let selection = *initial.configuration.effective().model();
    let target = ResolvedProviderTarget::naming(provider_model_identity(8));
    let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(31),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(32),
            request,
        },
    );
    let closed = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(33),
        initial.session,
        SemanticTranscriptEntryPayload::ToolClosed { request },
    );
    let continuation = initial
        .starting_snapshot
        .derive_appending_candidate(
            context_frontier_id(34),
            vec![assistant_tool_use.reference(), closed.reference()],
        )
        .expect("the cancellation frontier preserves its prefix");
    let input = ModelCallExecutionReconstitutionInput::new(
        initial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(attempt)
                    .begin_running()
                    .expect("the stored continuation tenure is running"),
            }),
        initial.targets.clone(),
        initial.starting_snapshot.clone(),
        vec![
            initial.frontier_entries[0].clone(),
            assistant_tool_use,
            closed,
        ],
        initial
            .origin_contents
            .into_iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(accepted_input, content)
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            initial.turn,
            target,
        )),
        vec![ModelCallReconstitutionInput::new(
            call,
            initial.turn,
            attempt,
            selection,
            target,
            continuation.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        continuation.frontier().owning_session(),
        continuation.frontier().snapshot(),
        continuation.ordered_entries().collect(),
    ));

    let error = input
        .reconstitute()
        .expect_err("a tool-close marker cannot reopen cancelled work");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::LifecycleMismatch
    );
}

/// a call-free continuation pin is checked against the immutable target catalog before it can
/// authorize the next provider call.
#[test]
fn continuation_rejects_crosswired_turn_pin() {
    let initial = active_execution();
    let request = tool_request_id(30);
    let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(31),
        initial.session,
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(32),
            request,
        },
    );
    let denied = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(33),
        initial.session,
        SemanticTranscriptEntryPayload::ToolDenied { request },
    );
    let continuation = initial
        .starting_snapshot
        .derive_appending_candidate(
            context_frontier_id(34),
            vec![assistant_tool_use.reference(), denied.reference()],
        )
        .expect("tool entries preserve the starting prefix");
    let input = ModelCallExecutionReconstitutionInput::new(
        initial
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(35)),
            }),
        initial.targets,
        initial.starting_snapshot,
        vec![
            initial.frontier_entries[0].clone(),
            assistant_tool_use,
            denied,
        ],
        initial
            .origin_contents
            .into_iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(accepted_input, content)
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            initial.turn,
            ResolvedProviderTarget::naming(provider_model_identity(99)),
        )),
        Vec::new(),
    )
    .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        continuation.frontier().owning_session(),
        continuation.frontier().snapshot(),
        continuation.ordered_entries().collect(),
    ))
    .with_tool_denial_correlations(vec![denied_approval(request)]);

    let error = input
        .reconstitute()
        .expect_err("the frozen selection rejects a cross-wired continuation pin");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::CallTargetMismatch
    );
}

/// steering correlation considers only the current turn's suffix and ignores steering retained in
/// its start.
#[test]
fn reconstitution_ignores_historical_steering() {
    let execution = active_execution();
    let historical_input = accepted_input_id(20);
    let historical = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(21),
        execution.session,
        SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: historical_input,
            source_turn: turn_id(22),
        },
    );
    let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        execution.session,
        context_frontier_id(23),
        vec![
            execution.frontier_entries[0].reference(),
            historical.reference(),
        ],
    )
    .expect("historical steering is valid starting history");
    let start = AcceptedInputTurnStart::from_validated_eligibility(
        crate::AcceptedInputStartingLineage::FirstInSession,
        snapshot.frontier(),
    );
    let mut origin_contents = execution
        .origin_contents
        .into_iter()
        .map(|(accepted_input, content)| {
            ModelCallOriginContent::from_validated_parts(accepted_input, content)
        })
        .collect::<Vec<_>>();
    origin_contents.push(ModelCallOriginContent::from_validated_parts(
        historical_input,
        UserContent::try_text(String::from("historical steering")).expect("valid text"),
    ));
    let input = ModelCallExecutionReconstitutionInput::new(
        execution.active_turn.with_start_for_test(start),
        execution.targets,
        snapshot,
        vec![execution.frontier_entries[0].clone(), historical],
        origin_contents,
        None,
        Vec::new(),
    );

    input
        .reconstitute()
        .expect("historical steering is not current-turn consumed steering");
}

/// a call that names a distinct snapshot must consume a nonempty steering suffix.
#[test]
fn reconstitution_rejects_empty_distinct_call_snapshot() {
    let execution = prepared_execution();
    let call = execution
        .current_call()
        .expect("prepared execution has one call");
    let distinct_snapshot = context_frontier_id(25);
    let input = ModelCallExecutionReconstitutionInput::new(
        execution.active_turn.clone(),
        execution.targets.clone(),
        execution.starting_snapshot.clone(),
        execution.frontier_entries.to_vec(),
        execution
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect(),
        Some(PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )),
        vec![ModelCallReconstitutionInput::new(
            call.id(),
            call.turn(),
            call.attempt(),
            call.selection(),
            call.target(),
            distinct_snapshot,
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        execution.session(),
        distinct_snapshot,
        execution.starting_snapshot.ordered_entries().collect(),
    ));

    let error = input
        .reconstitute()
        .expect_err("a distinct same-content call snapshot consumes no steering");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch
    );
}

/// persisted target facts must still match immutable configured target resolution when an execution
/// is reloaded.
#[test]
fn reconstitution_rejects_call_target_crosswired_from_turn_pin() {
    let execution = prepared_execution();
    let call = execution
        .current_call()
        .expect("prepared execution has one call");
    let input = reconstitution_input_with_calls(
        &execution,
        vec![ModelCallReconstitutionInput::new(
            call.id(),
            call.turn(),
            call.attempt(),
            call.selection(),
            ResolvedProviderTarget::naming(provider_model_identity(99)),
            call.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    );

    let error = input
        .reconstitute()
        .expect_err("stored target drift cannot reconstruct live authority");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::CallTargetMismatch
    );
}

/// a call row cannot manufacture the durable target that belongs independently to its owning turn.
#[test]
fn reconstitution_requires_independent_turn_pin() {
    let execution = prepared_execution();
    let call = execution
        .current_call()
        .expect("prepared execution has one call");
    let mut input = reconstitution_input_with_calls(
        &execution,
        vec![ModelCallReconstitutionInput::new(
            call.id(),
            call.turn(),
            call.attempt(),
            call.selection(),
            call.target(),
            call.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    );
    input.pinned_target = None;

    let error = input
        .reconstitute()
        .expect_err("a call without its independent turn pin fails closed");
    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::PinnedTargetMissing
    );
}

/// once a call has durably pinned its exact target, a later deployment-availability change cannot
/// retarget or strand that call.
#[test]
fn prepared_call_reloads_after_target_becomes_unavailable() {
    let execution = prepared_execution();
    let expected_call = execution
        .current_call()
        .expect("prepared execution has one call")
        .clone();
    let mut input = reconstitution_input_with_calls(
        &execution,
        vec![ModelCallReconstitutionInput::new(
            expected_call.id(),
            expected_call.turn(),
            expected_call.attempt(),
            expected_call.selection(),
            expected_call.target(),
            expected_call.frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    );
    input.targets = ModelTargetCatalog::try_from_definitions([])
        .expect("an empty current-availability catalog is valid");

    let reloaded = input
        .reconstitute()
        .expect("durable pinned authority survives current unavailability");

    assert_eq!(reloaded.current_call(), Some(&expected_call));
}

/// target resolution records the frozen selection, target, and exact frontier before send
/// authorization.
#[test]
fn preparation_is_a_distinct_checkpoint() {
    let execution = active_execution();
    let prepared = execution
        .prepare_initial_call(model_call_id(9))
        .expect("initial call may be prepared");

    assert_eq!(prepared.call().state(), CurrentModelCallState::Prepared);
    assert_eq!(
        prepared.call().selection(),
        FrozenModelSelection::Direct(direct(2))
    );
    assert_eq!(
        prepared.call().target().identity(),
        provider_model_identity(8)
    );
    assert_eq!(
        prepared.call().frontier().snapshot(),
        context_frontier_id(6)
    );
}

/// preparation must supply one fresh semantic identity for every pending steering input in the
/// complete active acceptance tail.
#[test]
fn preparation_requires_the_complete_steering_identity_inventory() {
    let mut execution = active_execution();
    execution.active_turn = execution.active_turn.with_pending_steering_for_test(
        vec![(
            accepted_input_id(20),
            crate::SessionInputPosition::try_from_u64(2)
                .expect("the test steering position is positive"),
        )]
        .into_boxed_slice(),
    );

    let error = execution
        .prepare_initial_call(model_call_id(9))
        .expect_err("the empty identity inventory cannot consume steering");

    assert_eq!(
        error.failure(),
        ModelCallPreparationFailure::SteeringIdentityCountMismatch
    );
}

/// every pending input is consumed in immutable acceptance order into one prefix extension named by
/// the prepared call.
#[test]
fn preparation_consumes_multiple_steering_inputs_in_order() {
    let mut execution = active_execution();
    let first = accepted_input_id(20);
    let second = accepted_input_id(21);
    execution.active_turn = execution.active_turn.with_pending_steering_for_test(
        vec![
            (
                first,
                crate::SessionInputPosition::try_from_u64(2)
                    .expect("the first steering position is positive"),
            ),
            (
                second,
                crate::SessionInputPosition::try_from_u64(3)
                    .expect("the second steering position is positive"),
            ),
        ]
        .into_boxed_slice(),
    );
    execution.origin_contents.insert(
        first,
        UserContent::try_text(String::from("first steering"))
            .expect("the first steering content is valid"),
    );
    execution.origin_contents.insert(
        second,
        UserContent::try_text(String::from("second steering"))
            .expect("the second steering content is valid"),
    );
    let call = model_call_id(9);
    let entry_ids = [
        semantic_transcript_entry_id(22),
        semantic_transcript_entry_id(23),
    ];
    let frontier = context_frontier_id(24);

    let prepared = execution
        .prepare_initial_call_consuming_steering(call, entry_ids.to_vec(), Some(frontier))
        .expect("the complete ordered steering inventory prepares atomically");

    assert_eq!(prepared.call().frontier().snapshot(), frontier);
    assert_eq!(
        prepared
            .consumed_steering()
            .iter()
            .map(|consumed| consumed.accepted_input().id())
            .collect::<Vec<_>>(),
        vec![first, second]
    );
    let first_consumed = &prepared.consumed_steering()[0];
    assert_eq!(
        first_consumed.accepted_input().disposition(),
        &AcceptedInputDisposition::ConsumedAsSteering { call }
    );
    assert_eq!(first_consumed.semantic_entry().identity(), entry_ids[0]);
    assert_eq!(
        first_consumed.semantic_entry().payload(),
        &SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: first,
            source_turn: turn_id(3),
        }
    );
    let second_consumed = &prepared.consumed_steering()[1];
    assert_eq!(
        second_consumed.accepted_input().disposition(),
        &AcceptedInputDisposition::ConsumedAsSteering { call }
    );
    assert_eq!(second_consumed.semantic_entry().identity(), entry_ids[1]);
    assert_eq!(
        second_consumed.semantic_entry().payload(),
        &SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: second,
            source_turn: turn_id(3),
        }
    );
    assert_eq!(
        prepared
            .steering_snapshot()
            .expect("steering creates an extended frontier")
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            SemanticTranscriptEntryRef::from_source(session_id(1), semantic_transcript_entry_id(5),),
            SemanticTranscriptEntryRef::from_source(session_id(1), entry_ids[0]),
            SemanticTranscriptEntryRef::from_source(session_id(1), entry_ids[1]),
        ]
    );
}

/// an immutable-catalog miss is retained as the exact proof authorizing known-failure closure
/// before any call exists.
#[test]
fn target_resolution_failure_requires_matching_proof() {
    let mut execution = active_execution();
    execution.targets =
        ModelTargetCatalog::try_from_definitions([]).expect("the empty test catalog is valid");
    let preparation = execution
        .prepare_initial_call(model_call_id(9))
        .expect_err("the configured selection is unavailable");
    let proof = preparation
        .target_resolution_error()
        .expect("target unavailability retains the exact catalog miss");

    let failed = preparation
        .execution()
        .clone()
        .fail_target_resolution(
            proof,
            FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            ),
        )
        .expect("the matching catalog miss authorizes known-failure closure");

    assert!(failed.call().is_none());
    assert_eq!(failed.disposition(), &TurnDisposition::Failed);
}

/// a catalog miss obtained elsewhere cannot discard a turn whose own immutable catalog resolves
/// successfully.
#[test]
fn resolvable_turn_rejects_foreign_resolution_failure() {
    let execution = active_execution();
    let foreign_proof = ModelTargetCatalog::try_from_definitions([])
        .expect("the empty test catalog is valid")
        .resolve(*execution.configuration().effective().model())
        .expect_err("the foreign empty catalog cannot resolve the selection");

    let error = execution
        .fail_target_resolution(
            foreign_proof,
            FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            ),
        )
        .expect_err("another catalog's miss cannot terminalize a resolvable turn");

    assert_eq!(error, ModelCallClosureError::TargetResolutionMismatch);
}

/// provider rendering receives the frontier in semantic order and the exact accepted user content
/// keyed by its origin identity.
#[test]
fn prepared_request_preserves_exact_origin_content() {
    let execution = prepared_execution();
    let request = execution
        .resume_prepared_call()
        .expect("a committed prepared call yields rendering material");
    let entry = request
        .frontier_entries()
        .next()
        .expect("the first-turn frontier contains its origin");

    assert!(matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            if *accepted_input == accepted_input_id(4)
    ));
    assert_eq!(
        request
            .origin_content(accepted_input_id(4))
            .expect("the checked origin has exact user content")
            .single_text()
            .expect("the fixture has exactly one text part")
            .as_str(),
        "hello"
    );
}

/// a resumed prepared request carries the turn's exact frozen validated model settings.
#[test]
fn prepared_request_carries_the_turns_exact_validated_model_settings() {
    let execution = prepared_execution();
    let expected = execution.configuration().effective().model_settings();

    let request = execution
        .resume_prepared_call()
        .expect("a committed prepared call yields its frozen settings");

    assert_eq!(request.model_settings(), expected);
}

/// resuming a prepared call renders only content named by that call's immutable frontier, excluding
/// steering accepted later.
#[test]
fn prepared_request_excludes_later_pending_steering_content() {
    let mut execution = prepared_execution_consuming_steering();
    let later = accepted_input_id(21);
    execution.active_turn = execution.active_turn.with_pending_steering_for_test(
        vec![(
            later,
            crate::SessionInputPosition::try_from_u64(3)
                .expect("the later steering position is positive"),
        )]
        .into_boxed_slice(),
    );
    execution.origin_contents.insert(
        later,
        UserContent::try_text(String::from("later steering"))
            .expect("the later steering content is valid"),
    );

    let request = execution
        .resume_prepared_call()
        .expect("the committed prepared call remains resumable");

    assert!(request.origin_content(later).is_none());
    assert_eq!(request.origin_contents.len(), 2);
}

/// authorization advances the exact attempt and call together without changing identity or
/// frontier.
#[test]
fn authorization_advances_attempt_and_call_together() {
    let authorized = prepared_execution()
        .authorize_send()
        .expect("prepared execution may authorize send");

    assert_eq!(
        authorized.attempt().state(),
        &CurrentTurnAttemptState::Running
    );
    assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
    assert_eq!(authorized.call().id(), model_call_id(9));
    assert_eq!(
        authorized.call().frontier().snapshot(),
        context_frontier_id(6)
    );
    let correlation = authorized.observation_correlation();
    assert_eq!(correlation.session(), authorized.session());
    assert_eq!(correlation.turn(), authorized.turn());
    assert_eq!(correlation.attempt(), authorized.attempt().id());
    assert_eq!(correlation.call(), authorized.call().id());
    assert_eq!(correlation.target(), authorized.call().target());
    assert_eq!(
        correlation.frontier(),
        authorized.call().frontier().snapshot()
    );
    let observation =
        correlation.bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    assert_eq!(observation.call(), authorized.call().id());
    assert_eq!(observation.correlation(), &correlation);
    assert_eq!(
        observation.observation(),
        &ModelCallTerminalObservation::KnownFailed
    );
}

/// interruption before a physical call exists ends the attempt and turn directly with the sole
/// applied proof and one explicit cancellation marker.
#[test]
fn interrupt_cancels_unprepared_work_directly() {
    let execution = active_execution();
    let interrupt = applied_interrupt(&execution);
    let expected_turn = execution.turn();
    let outcome = execution
        .apply_interrupt(
            interrupt,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(33),
                context_frontier_id(34),
            ),
        )
        .expect("a matching interrupt cancels unsent work");
    let ModelCallInterruptOutcome::Cancelled(cancelled) = outcome else {
        panic!("unsent work is terminally cancelled");
    };

    assert!(cancelled.call().is_none());
    assert_eq!(
        cancelled
            .attempt()
            .expect("unsent cancellation closes its prepared attempt")
            .end(),
        &crate::AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Cancelled,
        }
    );
    assert_eq!(
        cancelled.disposition(),
        &TurnDisposition::Cancelled {
            cause: interrupt.proof(),
        }
    );
    assert!(matches!(
        cancelled.cancellation_entry().payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn }
            if *turn == expected_turn
    ));
}

/// an interrupt closes a checkpointed but unsent tool attempt without inventing send authorization.
#[test]
fn interrupt_closes_prepared_tool_attempt() {
    let execution = active_execution();
    let request = batch_request(41, &execution);
    let tool_use = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(42),
        execution.session(),
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: request.producing_call(),
            request: request.id(),
        },
    );
    let yielded = execution
        .current_snapshot
        .derive_appending_candidate(context_frontier_id(43), vec![tool_use.reference()])
        .expect("the tool proposal extends the current frontier");
    let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request.id(),
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("the user approval is valid");
    let crash_lost_attempt = tool_attempt_id(44);
    let crash_lost = ToolAttemptReconstitutionInput::new(
        crash_lost_attempt,
        request.id(),
        execution.session(),
        execution.turn(),
        execution.current_attempt().id(),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
            error: ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
        }),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let batch = ToolBatchReconstitutionInput::new(
        execution.session(),
        execution.turn(),
        request.producing_call(),
        yielded,
        vec![request.clone()],
        vec![approval],
        vec![crash_lost],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: execution.current_attempt().id(),
        },
    )
    .reconstitute()
    .expect("the crash-lost prepared attempt remains terminalizable");
    let result_projection = batch
        .prepare_cancellation_projection(
            vec![semantic_transcript_entry_id(45)],
            context_frontier_id(46),
        )
        .expect("the resolved batch projects its terminal result");
    let interrupt = applied_interrupt(&execution);
    let cancelled = apply_interrupt_to_executing_tool_batch(
        execution.active_turn,
        batch,
        result_projection,
        interrupt,
        CancelledModelCallTurnIdentities::new(
            semantic_transcript_entry_id(47),
            context_frontier_id(48),
        ),
    )
    .expect("the prepared tool checkpoint closes directly");

    let AttemptEnd::AfterCancellation { cause, disposition } = cancelled
        .attempt()
        .expect("the executing batch retains its live turn attempt")
        .end()
    else {
        panic!("the interrupted attempt ends after cancellation");
    };
    assert_eq!(*cause, interrupt.proof());
    assert_eq!(*disposition, CancellationStopDisposition::Cancelled);
    assert_eq!(cancelled.tool_result_entries().len(), 1);
    let SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } =
        cancelled.tool_result_entries()[0].payload()
    else {
        panic!("the interrupted tool projects its execution result");
    };
    assert_eq!(*attempt, crash_lost_attempt);
}

/// interrupt result projection is bound to the exact yielded frontier identity, not merely equal
/// semantic content.
#[test]
fn tool_cancellation_rejects_same_content_foreign_frontier() {
    let execution = active_execution();
    let foreign_yield = ResolvedContextFrontierSnapshot::try_from_candidate(
        execution.session(),
        context_frontier_id(40),
        execution.current_snapshot.ordered_entries().collect(),
    )
    .expect("same-content foreign frontier is structurally valid");
    let request = batch_request(41, &execution);
    let projection = ToolBatchReconstitutionInput::new(
        execution.session(),
        execution.turn(),
        request.producing_call(),
        foreign_yield,
        vec![request.clone()],
        vec![denied_approval(request.id())],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: execution.current_attempt().id(),
        },
    )
    .reconstitute()
    .expect("the denied fixture batch is complete")
    .prepare_cancellation_projection(
        vec![semantic_transcript_entry_id(42)],
        context_frontier_id(43),
    )
    .expect("the foreign batch can prepare its own cancellation projection");
    let interrupt = applied_interrupt(&execution);

    let error = execution
        .apply_interrupt_to_tool_batch(
            interrupt,
            projection,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(44),
                context_frontier_id(45),
            ),
        )
        .expect_err("same semantic content cannot substitute another yielded frontier");
    assert_eq!(error, ModelCallClosureError::InterruptCorrelationMismatch);
}

/// an executing-batch cancellation projection must be bound to the interrupted turn. A projection
/// prepared for a foreign turn, but reusing this turn's current frontier identity as its yielded
/// source, cannot terminalize this turn with foreign results.
#[test]
fn tool_cancellation_rejects_foreign_turn_projection() {
    let execution = active_execution();
    let foreign_turn = turn_id(99);
    assert_ne!(foreign_turn, execution.turn());
    let foreign_request = ToolRequestReconstitutionInput::new(
        tool_request_id(41),
        execution.session(),
        foreign_turn,
        model_call_id(40),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("fixture_tool")).expect("the tool name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("the fixture arguments are canonical"),
    )
    .into_request();
    let projection = ToolBatchReconstitutionInput::new(
        execution.session(),
        foreign_turn,
        foreign_request.producing_call(),
        execution.current_snapshot.clone(),
        vec![foreign_request.clone()],
        vec![denied_approval(foreign_request.id())],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: turn_attempt_id(50),
        },
    )
    .reconstitute()
    .expect("a foreign-turn batch reusing this frontier reconstitutes")
    .prepare_cancellation_projection(
        vec![semantic_transcript_entry_id(42)],
        context_frontier_id(43),
    )
    .expect("the foreign batch can prepare its own cancellation projection");
    assert_eq!(
        projection.source_frontier(),
        execution.current_snapshot.frontier().snapshot()
    );
    assert_ne!(projection.turn(), execution.turn());
    let interrupt = applied_interrupt(&execution);

    let error = execution
        .apply_interrupt_to_tool_batch(
            interrupt,
            projection,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(44),
                context_frontier_id(45),
            ),
        )
        .expect_err("a foreign-turn projection cannot terminalize this turn");
    assert_eq!(error, ModelCallClosureError::InterruptCorrelationMismatch);
}

/// the legitimate same-turn executing-batch cancellation projection remains accepted after the turn
/// binding is added.
#[test]
fn tool_cancellation_accepts_same_turn_projection() {
    let execution = active_execution();
    let expected_turn = execution.turn();
    let expected_prefix = execution.current_snapshot.frontier();
    let request = batch_request(41, &execution);
    let projection = ToolBatchReconstitutionInput::new(
        execution.session(),
        execution.turn(),
        request.producing_call(),
        execution.current_snapshot.clone(),
        vec![request.clone()],
        vec![denied_approval(request.id())],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: execution.current_attempt().id(),
        },
    )
    .reconstitute()
    .expect("the same-turn denied batch is complete")
    .prepare_cancellation_projection(
        vec![semantic_transcript_entry_id(42)],
        context_frontier_id(43),
    )
    .expect("the same-turn batch prepares its cancellation projection");
    let interrupt = applied_interrupt(&execution);

    let cancelled = execution
        .apply_interrupt_to_tool_batch(
            interrupt,
            projection,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(44),
                context_frontier_id(45),
            ),
        )
        .expect("a turn-bound same-frontier projection terminalizes this turn");
    assert_eq!(cancelled.turn(), expected_turn);
    assert_eq!(cancelled.tool_result_entries().len(), 1);
    assert_eq!(
        cancelled.terminal_snapshot().immediate_semantic_prefix(),
        Some(expected_prefix)
    );
    assert_eq!(
        cancelled
            .terminal_snapshot()
            .appended_entries()
            .map(|entry| entry.entry())
            .collect::<Vec<_>>(),
        vec![
            semantic_transcript_entry_id(42),
            semantic_transcript_entry_id(44)
        ]
    );
}

/// a prepared but unsent call closes as proof-bearing cancellation without crossing send
/// authorization.
#[test]
fn interrupt_cancels_prepared_call_directly() {
    let execution = prepared_execution();
    let interrupt = applied_interrupt(&execution);
    let outcome = execution
        .apply_interrupt(
            interrupt,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(33),
                context_frontier_id(34),
            ),
        )
        .expect("a matching interrupt cancels a prepared call");
    let ModelCallInterruptOutcome::Cancelled(cancelled) = outcome else {
        panic!("a prepared call is terminally cancelled");
    };

    assert_eq!(
        cancelled
            .call()
            .expect("the prepared call remains immutable history")
            .disposition(),
        ModelCallDisposition::Cancelled
    );
    assert_eq!(
        cancelled
            .attempt()
            .expect("prepared-call cancellation closes its turn attempt")
            .end(),
        &crate::AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Cancelled,
        }
    );
}

/// issued work durably records the same cancellation authority on the attempt and call while
/// retaining the active slot.
#[test]
fn interrupt_requests_issued_call_cancellation() {
    let execution = in_flight_execution();
    let interrupt = applied_interrupt(&execution);
    let outcome = execution
        .apply_interrupt(
            interrupt,
            CancelledModelCallTurnIdentities::new(
                semantic_transcript_entry_id(33),
                context_frontier_id(34),
            ),
        )
        .expect("a matching interrupt stops issued work");
    let ModelCallInterruptOutcome::CancellationRequested(stopped) = outcome else {
        panic!("issued work retains the slot while cancellation is requested");
    };

    assert_eq!(
        stopped.attempt().state(),
        &CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly {
                interrupt: interrupt.proof(),
            },
        }
    );
    assert_eq!(
        stopped.call().state(),
        CurrentModelCallState::CancellationRequested
    );
    assert_eq!(stopped.interrupt(), interrupt.proof());
}

/// confirmed physical cancellation after a durable stop request is the evidence that releases the
/// slot as `Cancelled`.
#[test]
fn confirmed_cancellation_terminalizes_stopped_call() {
    let (execution, interrupt) = stop_requested_execution(in_flight_execution());
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Cancelled);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(35),
                    context_frontier_id(36),
                ),
            ),
        )
        .expect("confirmed cancellation closes the stopped call");
    let ModelCallTerminalOutcome::Cancelled(cancelled) = outcome else {
        panic!("physical cancellation plus exact proof cancels the turn");
    };

    assert_eq!(
        cancelled
            .call()
            .expect("the issued call remains terminal history")
            .disposition(),
        ModelCallDisposition::Cancelled
    );
    assert_eq!(
        cancelled
            .attempt()
            .expect("issued-call cancellation closes its turn attempt")
            .end(),
        &crate::AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Cancelled,
        }
    );
}

/// outcome-authoritative completion racing a stop request wins while retaining the interrupt in
/// attempt history.
#[test]
fn completion_race_preserves_outcome_and_stop_history() {
    let (execution, interrupt) = stop_requested_execution(in_flight_execution());
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                crate::AssistantText::try_new("race winner".to_owned()).expect("nonempty text"),
            ],
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![semantic_transcript_entry_id(35)],
                semantic_transcript_entry_id(36),
                context_frontier_id(37),
            )),
        )
        .expect("definitive completion wins the cancellation race");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("definitive completion remains authoritative");
    };

    assert_eq!(
        completed.attempt().end(),
        &crate::AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::TurnCompleted,
        }
    );
}

/// a tool-using response racing an applied interrupt records its proposals, closes them without
/// attempts, and terminalizes through the original stop proof.
#[test]
fn tool_response_race_closes_without_execution() {
    let (execution, interrupt) = stop_requested_execution(in_flight_execution());
    let request = tool_request_id(40);
    let expected_turn = execution.turn();
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithTools {
            response: ToolUsingAssistantResponse::try_from_parts(vec![
                AssistantResponsePart::ToolCall(tool_proposal("risky_tool", "{}")),
            ])
            .expect("the response contains one tool proposal"),
            retained_input_tokens: None,
            retained_output_tokens: None,
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::StoppedToolRound(
                StoppedToolRoundModelCallIdentities::new(
                    vec![StoppedToolResponsePartIdentity::tool_call(
                        semantic_transcript_entry_id(41),
                        request,
                        semantic_transcript_entry_id(42),
                        InitialToolApproval::Confirm,
                    )],
                    semantic_transcript_entry_id(43),
                    context_frontier_id(44),
                ),
            ),
        )
        .expect("the stop proof closes newly proposed tools");
    let ModelCallTerminalOutcome::CancelledWithToolResponse(cancelled) = outcome else {
        panic!("a stopped tool response terminalizes through cancellation");
    };

    assert_eq!(
        cancelled.attempt().end(),
        &AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Cancelled,
        }
    );
    assert_eq!(cancelled.requests()[0].id(), request);
    assert_eq!(
        cancelled.closed_result_entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ToolClosed { request }
    );
    assert!(matches!(
        cancelled.cancellation_entry().payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn }
            if *turn == expected_turn
    ));
}

/// an applied interrupt makes unacknowledged call ambiguity terminal reconciliation, preserving the
/// exact operation and stop proof while releasing the slot.
#[test]
fn stopped_ambiguity_requires_reconciliation() {
    let pending = accepted_input_id(40);
    let execution = with_pending_steering(in_flight_execution(), pending);
    let source_turn = execution.turn();
    let (execution, interrupt) = stop_requested_execution(execution);
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Ambiguous);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Ambiguous(
                AmbiguousModelCallTurnIdentities::new(context_frontier_id(41))
                    .with_pending_steering_reclassifications(one_reclassification(
                        pending,
                        turn_id(42),
                    )),
            ),
        )
        .expect("stopped ambiguity is exactly representable");
    let ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) = outcome else {
        panic!("stopped ambiguity must release the slot through reconciliation");
    };

    assert_eq!(
        reconciliation.attempt().end(),
        &crate::AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Ambiguous,
        }
    );
    let TurnDisposition::ReconciliationRequired { marker } = reconciliation.disposition() else {
        panic!("the terminal disposition carries its complete marker");
    };
    assert_eq!(
        marker.reason(),
        &crate::ReconciliationReason::InterruptRequiresReconciliation {
            interrupt: interrupt.proof(),
        }
    );
    assert_eq!(marker.ambiguous_operations().operation_count(), 1);
    assert!(
        marker
            .ambiguous_operations()
            .contains(crate::IssuedOperationRef::ModelCall(
                reconciliation.call().id()
            ))
    );
    assert_one_reclassified_turn(
        reconciliation.reclassified_pending_steering(),
        pending,
        source_turn,
        turn_id(42),
    );
}

/// an authoritative reread of a durably issued call reconstructs the same provider-facing
/// correlation without authorizing or transitioning it a second time.
#[test]
fn in_flight_reread_reconstructs_exact_authorization() {
    let execution = in_flight_execution();
    let expected_call = execution
        .current_call()
        .expect("the fixture contains one issued call")
        .id();
    let authorized = execution
        .resume_in_flight_call()
        .expect("checked InFlight state is resumable for reread only");

    assert_eq!(authorized.call().id(), expected_call);
    assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
    assert_eq!(
        authorized.attempt().state(),
        &CurrentTurnAttemptState::Running
    );
    assert_eq!(authorized.observation_correlation().call(), expected_call);
    assert_eq!(authorized.session(), execution.session());
    assert_eq!(authorized.turn(), execution.turn());
    assert_eq!(authorized.attempt(), execution.current_attempt());
    assert_eq!(authorized.call(), execution.current_call().unwrap());
    assert!(prepared_execution().resume_in_flight_call().is_none());
}

/// a provider observation remains bound to the exact session, turn, attempt, call, target, and
/// frontier that crossed send authorization.
#[test]
fn terminal_observation_rejects_cross_wired_call() {
    let execution = in_flight_execution();
    let mut observation =
        correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);
    observation.correlation.call = model_call_id(99);

    let error = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            )),
        )
        .expect_err("another call's observation cannot close fresh authority");

    assert_eq!(error, ModelCallClosureError::ObservationCorrelationMismatch);
}

/// successful final text, physical completion, attempt/turn completion, and the final marker share
/// one prefix-preserving candidate.
#[test]
fn completion_is_atomic_and_ordered() {
    let execution = in_flight_execution();
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                crate::AssistantText::try_new("first".to_string()).expect("nonempty text"),
                crate::AssistantText::try_new(" second ".to_string()).expect("nonempty text"),
            ],
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![
                    semantic_transcript_entry_id(10),
                    semantic_transcript_entry_id(11),
                ],
                semantic_transcript_entry_id(12),
                context_frontier_id(13),
            )),
        )
        .expect("definitive text completion is admissible");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("completed evidence selects completed outcome");
    };

    assert_eq!(
        completed.call().disposition(),
        ModelCallDisposition::Completed
    );
    assert_eq!(completed.disposition(), &TurnDisposition::Completed);
    assert_eq!(completed.assistant_entries().len(), 2);
    assert!(matches!(
        completed.completion_entry().payload(),
        SemanticTranscriptEntryPayload::TurnCompleted { turn } if *turn == turn_id(3)
    ));
    assert_eq!(
        completed
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            crate::SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(5)
            ),
            crate::SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(10)
            ),
            crate::SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(11)
            ),
            crate::SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(12)
            ),
        ]
    );
}

/// a tool-using completion commits ordered request references, yields its attempt, and parks on the
/// earliest undecided request without completing the turn.
#[test]
fn tool_round_yields_and_parks_in_order() {
    let execution = in_flight_execution();
    let first_request = tool_request_id(20);
    let second_request = tool_request_id(21);
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithTools {
            response: ToolUsingAssistantResponse::try_from_parts(vec![
                AssistantResponsePart::Text(
                    crate::AssistantText::try_new(String::from("checking"))
                        .expect("assistant text is nonempty"),
                ),
                AssistantResponsePart::ToolCall(tool_proposal("risky_tool", r#"{"b":2,"a":1}"#)),
                AssistantResponsePart::ToolCall(tool_proposal("current_time", "{}")),
            ])
            .expect("the response contains tool proposals"),
            retained_input_tokens: None,
            retained_output_tokens: None,
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![
                    ToolResponsePartIdentity::text(semantic_transcript_entry_id(10)),
                    ToolResponsePartIdentity::tool_call(
                        semantic_transcript_entry_id(11),
                        first_request,
                        InitialToolApproval::Confirm,
                    ),
                    ToolResponsePartIdentity::tool_call(
                        semantic_transcript_entry_id(12),
                        second_request,
                        InitialToolApproval::PolicyAuto,
                    ),
                ],
                context_frontier_id(13),
                None,
            )),
        )
        .expect("ordered request content and identities produce one tool yield");
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("tool-using completion yields a tool round");
    };

    assert_eq!(
        round.attempt().end(),
        &AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
        }
    );
    assert!(matches!(
        round.next_phase(),
        ActiveTurnPhase::AwaitingApproval { request } if *request == first_request
    ));
    assert_eq!(round.requests()[0].id(), first_request);
    assert_eq!(
        round.requests()[0].ordinal(),
        ToolRequestOrdinal::from_u32(0)
    );
    assert_eq!(round.requests()[0].arguments().as_str(), r#"{"a":1,"b":2}"#);
    assert_eq!(round.requests()[1].id(), second_request);
    assert_eq!(
        round.requests()[1].ordinal(),
        ToolRequestOrdinal::from_u32(1)
    );
    assert_eq!(round.automatic_approvals().len(), 1);
    assert_eq!(
        round.automatic_approvals()[0].source(),
        crate::ToolDecisionSource::PolicyAuto
    );
    assert!(matches!(
        round.assistant_entries()[1].payload(),
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call,
            request,
        } if *producing_call == model_call_id(9) && *request == first_request
    ));
    assert_eq!(
        round
            .yielded_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            SemanticTranscriptEntryRef::from_source(session_id(1), semantic_transcript_entry_id(5),),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(10),
            ),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(11),
            ),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(12),
            ),
        ]
    );
}

/// a later model round cannot reuse a tool request identity already present in immutable transcript
/// history.
#[test]
fn tool_round_rejects_historical_request_identity() {
    let execution = in_flight_execution();
    let request = tool_request_id(20);
    let mut frontier_entries = execution.frontier_entries.to_vec();
    frontier_entries.push(SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(40),
        execution.session(),
        SemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: model_call_id(41),
            request,
        },
    ));
    frontier_entries.push(SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(42),
        execution.session(),
        SemanticTranscriptEntryPayload::ToolDenied { request },
    ));
    let call = execution
        .current_call
        .clone()
        .expect("the fixture has an issued call")
        .end_classified(ModelCallDisposition::Completed)
        .expect("issued calls accept completed classification");
    let attempt = execution
        .current_attempt
        .clone()
        .end_without_stop(UnstoppedAttemptDisposition::YieldedToDurableWait)
        .expect("the running fixture can yield");
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            tool_proposal("current_time", "{}"),
        )])
        .expect("the response contains one tool proposal");

    let error = assemble_tool_round(
        ModelCallTurnScope {
            session: execution.session(),
            turn: execution.turn(),
        },
        call,
        attempt,
        frontier_entries,
        response,
        ToolRoundModelCallIdentities::new(
            vec![ToolResponsePartIdentity::tool_call(
                semantic_transcript_entry_id(43),
                request,
                InitialToolApproval::Confirm,
            )],
            context_frontier_id(44),
            None,
        ),
        DangerousToolAutoApproval::Disabled,
    )
    .expect_err("immutable request identity cannot be reused");

    assert_eq!(error, ModelCallClosureError::FrontierDerivationFailed);
}

/// an all-auto batch creates one fresh prepared continuation attempt while retaining the same
/// logical turn.
#[test]
fn all_auto_tool_round_prepares_continuation() {
    let execution = in_flight_execution();
    let request = tool_request_id(20);
    let continuation = turn_attempt_id(21);
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::CompletedWithTools {
            response: ToolUsingAssistantResponse::try_from_parts(vec![
                AssistantResponsePart::ToolCall(tool_proposal("current_time", "{}")),
            ])
            .expect("the response contains one tool proposal"),
            retained_input_tokens: None,
            retained_output_tokens: None,
        },
    );
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    semantic_transcript_entry_id(10),
                    request,
                    InitialToolApproval::PolicyAuto,
                )],
                context_frontier_id(11),
                Some(continuation),
            )),
        )
        .expect("an all-auto batch has no approval wait");
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("tool-using completion yields a tool round");
    };
    let ActiveTurnPhase::Running { current_attempt } = round.next_phase() else {
        panic!("all-auto policy prepares continuation");
    };

    assert_eq!(round.turn(), turn_id(3));
    assert_eq!(current_attempt.id(), continuation);
    assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);
}

/// a definitive response terminalizes its source only together with ordered, visible
/// reclassification of pending steering.
#[test]
fn completion_reclassifies_pending_steering_atomically() {
    let pending = accepted_input_id(20);
    let successor = turn_id(21);
    let execution = with_pending_steering(in_flight_execution(), pending);
    let observation = correlated_observation(
        &execution,
        ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                crate::AssistantText::try_new("reply".to_owned()).expect("nonempty text"),
            ],
        },
    );
    let identities = CompletedModelCallIdentities::new(
        vec![semantic_transcript_entry_id(10)],
        semantic_transcript_entry_id(11),
        context_frontier_id(12),
    )
    .with_pending_steering_reclassifications(one_reclassification(pending, successor));

    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Completed(identities),
        )
        .expect("terminal completion may reclassify complete steering facts");
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("completed evidence selects completed outcome");
    };

    assert_one_reclassified_turn(
        completed.reclassified_pending_steering(),
        pending,
        turn_id(3),
        successor,
    );
}

/// terminal observation cannot release the source while a pending input lacks its exact
/// reclassified successor identity.
#[test]
fn terminal_observation_rejects_missing_reclassification() {
    let pending = accepted_input_id(20);
    let execution = with_pending_steering(in_flight_execution(), pending);
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);

    let error = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            )),
        )
        .expect_err("pending steering cannot disappear at terminalization");

    assert_eq!(
        error,
        ModelCallClosureError::PendingSteeringReclassificationMismatch
    );
}

/// a refusal reclassifies pending steering without adding response content to the refused turn's
/// terminal frontier.
#[test]
fn refusal_reclassifies_pending_steering_atomically() {
    let pending = accepted_input_id(20);
    let successor = turn_id(21);
    let execution = with_pending_steering(in_flight_execution(), pending);
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Refused);
    let identities = RefusedModelCallTurnIdentities::new(context_frontier_id(10))
        .with_pending_steering_reclassifications(one_reclassification(pending, successor));

    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Refused(identities),
        )
        .expect("terminal refusal may reclassify complete steering facts");
    let ModelCallTerminalOutcome::Refused(refused) = outcome else {
        panic!("refused evidence selects refused outcome");
    };

    assert_one_reclassified_turn(
        refused.reclassified_pending_steering(),
        pending,
        turn_id(3),
        successor,
    );
}

/// trustworthy pre-send failure releases its source only together with pending-steering
/// reclassification.
#[test]
fn prepared_failure_reclassifies_pending_steering_atomically() {
    let pending = accepted_input_id(20);
    let successor = turn_id(21);
    let execution = with_pending_steering(prepared_execution(), pending);
    let identities = FailedModelCallTurnIdentities::new(
        semantic_transcript_entry_id(10),
        context_frontier_id(11),
    )
    .with_pending_steering_reclassifications(one_reclassification(pending, successor));

    let failed = execution
        .fail_prepared_call(identities)
        .expect("pre-send failure may reclassify complete steering facts");

    assert_one_reclassified_turn(
        failed.reclassified_pending_steering(),
        pending,
        turn_id(3),
        successor,
    );
}

/// a failed required compaction closes the fresh physical attempt without fabricating a provider
/// call.
#[test]
fn automatic_compaction_failure_closes_call_free_turn() {
    let failure_entry = semantic_transcript_entry_id(10);
    let execution = active_execution();
    let session = execution.session();
    let starting_entry = execution
        .current_snapshot
        .ordered_entries()
        .next()
        .expect("fixture activation carries its origin");
    let failed = execution
        .fail_automatic_context_compaction(FailedModelCallTurnIdentities::new(
            failure_entry,
            context_frontier_id(11),
        ))
        .expect("required compaction failure closes the call-free attempt");

    assert_eq!(failed.call(), None);
    assert!(matches!(
        failed.attempt().end(),
        crate::AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::KnownFailure,
        }
    ));
    assert_eq!(
        failed
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            starting_entry,
            SemanticTranscriptEntryRef::from_source(session, failure_entry),
        ]
    );
}

/// a known failure after steering consumption appends its marker to the call frontier without
/// losing consumed input.
#[test]
fn prepared_failure_extends_steering_call_frontier() {
    let failure_entry = semantic_transcript_entry_id(30);
    let failed = prepared_execution_consuming_steering()
        .fail_prepared_call(FailedModelCallTurnIdentities::new(
            failure_entry,
            context_frontier_id(31),
        ))
        .expect("known failure extends the exact prepared call frontier");

    assert_eq!(
        failed
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            SemanticTranscriptEntryRef::from_source(session_id(1), semantic_transcript_entry_id(5),),
            SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(22),
            ),
            SemanticTranscriptEntryRef::from_source(session_id(1), failure_entry),
        ]
    );
}

/// ambiguous physical completion ends the live attempt and retains the exact call in a durable
/// recovery wait.
#[test]
fn ambiguity_preserves_call_and_waits() {
    let execution = in_flight_execution();
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Ambiguous);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                context_frontier_id(43),
            )),
        )
        .expect("ambiguous evidence is representable");
    let ModelCallTerminalOutcome::AwaitingRecovery(waiting) = outcome else {
        panic!("ambiguous evidence selects recovery wait");
    };

    assert_eq!(
        waiting.call().disposition(),
        ModelCallDisposition::Ambiguous
    );
    assert!(
        waiting
            .ambiguous_operations()
            .contains(crate::IssuedOperationRef::ModelCall(model_call_id(9)))
    );
}

/// startup converts an unsent prepared call to known failure, records the lost attempt, and
/// reclassifies steering before releasing the source.
#[test]
fn restart_closes_prepared_call_and_reclassifies_steering() {
    let pending = accepted_input_id(20);
    let successor = turn_id(21);
    let execution = with_pending_steering(prepared_execution(), pending);
    let identities = FailedModelCallTurnIdentities::new(
        semantic_transcript_entry_id(10),
        context_frontier_id(11),
    )
    .with_pending_steering_reclassifications(one_reclassification(pending, successor));
    let outcome = execution
        .recover_after_restart(identities)
        .expect("startup may close an unsent prepared call");
    let ModelCallTerminalOutcome::Failed(failed) = outcome else {
        panic!("a prior-process prepared call selects failed outcome");
    };

    assert_eq!(
        failed
            .call()
            .expect("the prepared call becomes terminal")
            .disposition(),
        ModelCallDisposition::KnownFailed
    );
    assert!(matches!(
        failed.attempt().end(),
        crate::AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    ));
    assert_one_reclassified_turn(
        failed.reclassified_pending_steering(),
        pending,
        turn_id(3),
        successor,
    );
}

/// startup cannot infer the fate of an issued prior-process call, so it records ambiguity and a
/// lost attempt.
#[test]
fn restart_preserves_in_flight_call_as_ambiguous() {
    let outcome = in_flight_execution()
        .recover_after_restart(FailedModelCallTurnIdentities::new(
            semantic_transcript_entry_id(10),
            context_frontier_id(11),
        ))
        .expect("startup may classify an abandoned issued call");
    let ModelCallTerminalOutcome::AwaitingRecovery(waiting) = outcome else {
        panic!("a prior-process issued call selects recovery wait");
    };

    assert_eq!(
        waiting.call().disposition(),
        ModelCallDisposition::Ambiguous
    );
    assert!(matches!(
        waiting.attempt().end(),
        crate::AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    ));
    assert!(
        waiting
            .ambiguous_operations()
            .contains(crate::IssuedOperationRef::ModelCall(model_call_id(9)))
    );
}

/// cancellation-requested call state lacks the proof-bearing stopped-attempt facts required by
/// docs/spec/turn-lifecycle-and-scheduling.md, so this evidence-free execution projection fails
/// closed during reconstitution.
#[test]
fn cancellation_requested_reconstitution_fails_closed() {
    let in_flight = in_flight_execution();
    let cancellation_requested = in_flight
        .current_call()
        .expect("in-flight execution has one call")
        .clone()
        .request_cancellation()
        .expect("an in-flight call may request cancellation");
    let error = reconstitution_input_with_calls(
        &in_flight,
        vec![ModelCallReconstitutionInput::new(
            cancellation_requested.id(),
            cancellation_requested.turn(),
            cancellation_requested.attempt(),
            cancellation_requested.selection(),
            cancellation_requested.target(),
            cancellation_requested.frontier().snapshot(),
            ModelCallReconstitutionState::CancellationRequested,
        )],
    )
    .reconstitute()
    .expect_err("proof-free cancellation-requested storage must not reconstruct live");

    assert_eq!(
        error.failure(),
        ModelCallExecutionReconstitutionFailure::LifecycleMismatch
    );
}

/// definitive known failure closes the physical call and logical turn as failed in one candidate.
#[test]
fn known_failure_closes_call_attempt_and_turn() {
    let execution = in_flight_execution();
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            )),
        )
        .expect("known-failure evidence is admissible");
    let ModelCallTerminalOutcome::Failed(failed) = outcome else {
        panic!("known-failure evidence selects failed outcome");
    };

    assert_eq!(
        failed
            .call()
            .expect("the issued call is terminal")
            .disposition(),
        ModelCallDisposition::KnownFailed
    );
    assert_eq!(failed.disposition(), &TurnDisposition::Failed);
}

/// a cause-free physical cancellation is not a logical cancellation and closes the logical turn as
/// failed.
#[test]
fn cause_free_physical_cancellation_fails_turn() {
    let execution = in_flight_execution();
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Cancelled);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                ),
            ),
        )
        .expect("cause-free physical cancellation is admissible");
    let ModelCallTerminalOutcome::Failed(failed) = outcome else {
        panic!("cause-free cancellation selects failed outcome");
    };

    assert_eq!(
        failed
            .call()
            .expect("the issued call is terminal")
            .disposition(),
        ModelCallDisposition::Cancelled
    );
    assert_eq!(failed.disposition(), &TurnDisposition::Failed);
}

/// an explicit provider refusal preserves its physical and logical classifications without
/// manufacturing semantic response text.
#[test]
fn refusal_closes_call_attempt_and_turn_without_content() {
    let execution = in_flight_execution();
    let observation = correlated_observation(&execution, ModelCallTerminalObservation::Refused);
    let outcome = execution
        .apply_terminal_observation(
            observation,
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                context_frontier_id(11),
            )),
        )
        .expect("explicit refusal evidence is admissible");
    let ModelCallTerminalOutcome::Refused(refused) = outcome else {
        panic!("refusal evidence selects refused outcome");
    };

    assert_eq!(refused.call().disposition(), ModelCallDisposition::Refused);
    assert_eq!(refused.disposition(), &TurnDisposition::Refused);
    assert_eq!(refused.terminal_snapshot().entry_count(), 1);
}
