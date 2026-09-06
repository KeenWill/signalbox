//! Session delegation tests for `docs/spec/sessions-and-transcript.md`.

use super::content::delegation_content_digest;
use super::request::{
    AWAIT_SESSION_TOOL_NAME, SEND_SESSION_MESSAGE_TOOL_NAME, SPAWN_SESSION_TOOL_NAME,
};
use super::terminal_turn::TerminalChildTurnKind;
use crate::{GoalGeneration, ToolRequest};

use super::*;
use crate::{
    InitialToolApproval, NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
    model_execution::{cancelled_turn_fixture, completed_turn_fixture, failed_turn_fixture},
    test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
};
use expect_test::expect;

const TEST_TASK: &str = "inspect delegated work";

/// Canonical request fixture: seed N derives request N+100, session N, turn
/// N+10, call N+20, and ordinal zero.
fn named_request(session: u128, name: &str, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest::from_model_proposal(
        tool_request_id(session + 100),
        session_id(session),
        turn_id(session + 10),
        model_call_id(session + 20),
        ToolRequestOrdinal::from_u32(0),
        ToolCallProposal::new(
            ToolName::try_new(name.into()).expect("valid name"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                .expect("valid arguments"),
        ),
        InitialToolApproval::Confirm,
    )
}

fn content(value: &str) -> DelegationContent {
    DelegationContent::try_new(value.into()).expect("bounded nonempty content")
}

fn parent_termination_authority(scope: DescendantTerminationScope) -> ParentTerminationAuthority {
    ParentTerminationAuthority {
        parent: session_id(1),
        source: ParentTerminationCommandSource::Turn { turn: turn_id(2) },
        command: command_id(3),
        kind: ParentTerminationKind::Stopped,
        scope,
    }
}

#[test]
fn spawn_request_seals_task_policy_and_provenance() {
    let raw = named_request(
        1,
        SPAWN_SESSION_TOOL_NAME,
        serde_json::json!({
            "relationship": { "kind": "background" },
            "task": TEST_TASK,
        }),
    );
    let expected_identity = (raw.session(), raw.turn(), raw.id());
    let request =
        DelegatedSpawnRequest::parse(raw, TEST_TASK.into(), ChildRelationshipPolicy::Background)
            .expect("canonical spawn request");
    let provenance = DelegationProvenance::from_spawn(&request);

    assert_eq!(request.task(), &content(TEST_TASK));
    assert_eq!(request.policy(), ChildRelationshipPolicy::Background);
    assert_eq!(provenance.tool_request(), Some(expected_identity));
}

#[test]
fn provenance_projection_exposes_each_closed_authority_variant() {
    let spawn = DelegatedSpawnRequest::parse(
        named_request(
            1,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": { "kind": "background" },
                "task": TEST_TASK,
            }),
        ),
        TEST_TASK.into(),
        ChildRelationshipPolicy::Background,
    )
    .expect("canonical spawn request");
    let terminal = TerminalChildTurn::from_failed(&failed_turn_fixture());
    let authority = parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);

    assert_eq!(
        DelegationProvenance::from_spawn(&spawn).projection(),
        DelegationProvenanceProjection::ToolRequest {
            source_session: spawn.request().session(),
            source_turn: spawn.request().turn(),
            request: spawn.request().id(),
        }
    );
    assert_eq!(
        DelegationProvenance::from_terminal_child(terminal).projection(),
        DelegationProvenanceProjection::ChildTurn { terminal }
    );
    assert_eq!(
        DelegationProvenance::from_parent_termination(authority).projection(),
        DelegationProvenanceProjection::ParentCommand { authority }
    );
}

#[test]
fn spawn_request_accepts_canonical_bound_relationship() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let raw = named_request(
        1,
        SPAWN_SESSION_TOOL_NAME,
        serde_json::json!({
            "relationship": {
                "kind": "bound",
                "on_parent_cancelled": "cancel",
                "on_parent_stopped": "stop",
            },
            "task": TEST_TASK,
        }),
    );
    let request = DelegatedSpawnRequest::parse(raw, TEST_TASK.into(), policy)
        .expect("canonical bound relationship is admitted");

    assert_eq!(request.policy(), policy);
}

#[test]
fn await_request_seals_child_and_mode() {
    let child = session_id(2);
    let raw = named_request(
        1,
        AWAIT_SESSION_TOOL_NAME,
        serde_json::json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    let expected_identity = (raw.session(), raw.turn(), raw.id());
    let request = DelegationAwaitRequest::parse(raw, child, DelegationWaitMode::Foreground)
        .expect("canonical await request");

    assert_eq!(request.child(), child);
    assert_eq!(request.mode(), DelegationWaitMode::Foreground);
    assert_eq!(
        DelegationProvenance::from_await(&request).tool_request(),
        Some(expected_identity)
    );
}

#[test]
fn message_request_seals_peer_content_and_provenance() {
    let peer = session_id(2);
    let message = content("progress update");
    let raw = named_request(
        1,
        SEND_SESSION_MESSAGE_TOOL_NAME,
        serde_json::json!({
            "content": message.as_str(),
            "peer_session_id": peer.as_uuid().to_string(),
        }),
    );
    let expected_identity = (raw.session(), raw.turn(), raw.id());
    let request = DelegationMessageRequest::parse(raw, peer, message.as_str().into())
        .expect("canonical message request");

    assert_eq!(request.peer(), peer);
    assert_eq!(request.content(), &message);
    assert_eq!(
        DelegationProvenance::from_message(&request).tool_request(),
        Some(expected_identity)
    );
}

#[test]
fn request_parser_rejects_noncanonical_bound_spelling_without_consuming_request() {
    let raw = named_request(
        1,
        SPAWN_SESSION_TOOL_NAME,
        serde_json::json!({
            "relationship": {
                "kind": "Bound",
                "on_parent_cancelled": "keep_running",
                "on_parent_stopped": "keep_running",
            },
            "task": TEST_TASK,
        }),
    );
    let error = DelegatedSpawnRequest::parse(
        raw.clone(),
        TEST_TASK.into(),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::KeepRunning,
            on_parent_cancelled: BoundChildAction::KeepRunning,
        },
    )
    .expect_err("relationship spelling is closed and lowercase");

    assert_eq!(
        error.failure(),
        &DelegationRequestFailure::InvalidToolRequestPurpose
    );
    assert_eq!(error.into_request(), raw);
}

#[test]
fn spawn_request_rejects_carried_task_drift() {
    let raw = named_request(
        1,
        SPAWN_SESSION_TOOL_NAME,
        serde_json::json!({
            "relationship": { "kind": "background" },
            "task": TEST_TASK,
        }),
    );
    let error = DelegatedSpawnRequest::parse(
        raw,
        "different task".into(),
        ChildRelationshipPolicy::Background,
    )
    .expect_err("carried task must match the canonical request");

    assert_eq!(
        error.failure(),
        &DelegationRequestFailure::InvalidToolRequestPurpose
    );
}

#[test]
fn request_error_forwards_recoverable_invalid_content() {
    let raw = named_request(
        1,
        SPAWN_SESSION_TOOL_NAME,
        serde_json::json!({
            "relationship": { "kind": "background" },
            "task": "",
        }),
    );
    let error = DelegatedSpawnRequest::parse(raw, "".into(), ChildRelationshipPolicy::Background)
        .expect_err("empty task is invalid content");
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<DelegationContentError>())
        .expect("invalid content is the request error source");

    assert_eq!(source.value(), "");
    assert_eq!(
        source.failure(),
        DelegationContentFailure::Invalid(crate::NonEmptyUnicodeTextFailure::Empty)
    );
}

#[test]
fn spawn_request_checks_tool_purpose_before_task_content() {
    let raw = named_request(
        1,
        AWAIT_SESSION_TOOL_NAME,
        serde_json::json!({
            "child_session_id": session_id(2).as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    let error = DelegatedSpawnRequest::parse(
        raw.clone(),
        String::new(),
        ChildRelationshipPolicy::Background,
    )
    .expect_err("the wrong operation is rejected before invalid task content");

    assert_eq!(
        error.failure(),
        &DelegationRequestFailure::InvalidToolRequestPurpose
    );
    assert_eq!(error.into_request(), raw);
}

#[test]
fn message_request_checks_tool_purpose_before_message_content() {
    let raw = named_request(
        1,
        AWAIT_SESSION_TOOL_NAME,
        serde_json::json!({
            "child_session_id": session_id(2).as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    let error = DelegationMessageRequest::parse(raw.clone(), session_id(2), String::new())
        .expect_err("the wrong operation is rejected before invalid message content");

    assert_eq!(
        error.failure(),
        &DelegationRequestFailure::InvalidToolRequestPurpose
    );
    assert_eq!(error.into_request(), raw);
}

#[test]
fn await_request_rejects_carried_child_drift() {
    let child = session_id(2);
    let unrelated_child = session_id(9);
    let awaiting = named_request(
        1,
        AWAIT_SESSION_TOOL_NAME,
        serde_json::json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    assert!(
        DelegationAwaitRequest::parse(awaiting, unrelated_child, DelegationWaitMode::Foreground)
            .is_err()
    );
}

#[test]
fn await_request_rejects_carried_mode_drift() {
    let child = session_id(2);
    let awaiting = named_request(
        1,
        AWAIT_SESSION_TOOL_NAME,
        serde_json::json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": "foreground",
        }),
    );

    assert!(
        DelegationAwaitRequest::parse(awaiting, child, DelegationWaitMode::Background).is_err()
    );
}

#[test]
fn message_request_rejects_carried_peer_drift() {
    let message = content("progress update");
    let unrelated_peer = session_id(9);
    let sending = named_request(
        2,
        SEND_SESSION_MESSAGE_TOOL_NAME,
        serde_json::json!({
            "content": message.as_str(),
            "peer_session_id": session_id(1).as_uuid().to_string(),
        }),
    );

    assert!(
        DelegationMessageRequest::parse(sending, unrelated_peer, message.as_str().into()).is_err()
    );
}

#[test]
fn message_request_rejects_carried_content_drift() {
    let message = content("progress update");
    let sending = named_request(
        2,
        SEND_SESSION_MESSAGE_TOOL_NAME,
        serde_json::json!({
            "content": message.as_str(),
            "peer_session_id": session_id(1).as_uuid().to_string(),
        }),
    );

    assert!(DelegationMessageRequest::parse(sending, session_id(1), "drift".into()).is_err());
}

/// a live completed call seals its own nonempty result.
#[test]
fn live_completed_call_proves_returned_content() {
    let expected = content("completed child result");
    let completed = completed_turn_fixture(&[expected.as_str()]);
    let terminal = TerminalChildTurn::from_completed(&completed)
        .expect("live completed call seals terminal child evidence");
    let outcome = DelegationOutcome::from_terminal_child(terminal, Some(expected.clone()))
        .expect("sealed live result authenticates its exact content");
    let directly_derived = DelegationOutcome::from_completed_child(&completed);

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
    assert_eq!(outcome.content(), Some(&expected));
    assert_eq!(directly_derived, outcome);
}

#[test]
fn provider_compaction_metadata_does_not_hide_a_delegated_child_result() {
    let expected = content("completed child result");
    let completed =
        crate::model_execution::completed_turn_with_provider_compaction_fixture(expected.as_str());

    let outcome = DelegationOutcome::from_completed_child(&completed);

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
    assert_eq!(outcome.content(), Some(&expected));
}

/// empty live completion is a typed unavailable result.
#[test]
fn empty_live_completion_produces_unavailable_outcome() {
    let completed = completed_turn_fixture(&[]);
    let terminal = TerminalChildTurn::from_completed(&completed)
        .expect("empty live completion remains terminal child evidence");
    let outcome = DelegationOutcome::from_terminal_child(terminal, None)
        .expect("empty completion derives an explicit failed outcome");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
    assert_eq!(
        outcome.reason(),
        DelegationOutcomeReason::ChildResultUnavailable
    );
}

/// failure evidence can name a delegated-task-origin turn.
#[test]
fn failed_turn_proves_its_exact_origin_agnostic_identity() {
    let failed = failed_turn_fixture();
    let expected_identity = (failed.session(), failed.turn());
    let terminal = TerminalChildTurn::from_failed(&failed);
    let outcome = DelegationOutcome::from_terminal_child(terminal, None)
        .expect("sealed failed turn derives a child failure");
    let directly_derived = DelegationOutcome::from_failed_child(&failed);

    assert_eq!((terminal.session(), terminal.turn()), expected_identity);
    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
    assert_eq!(directly_derived, outcome);
}

/// cancellation evidence can name a delegated-task-origin turn.
#[test]
fn cancelled_turn_proves_its_exact_origin_agnostic_identity() {
    let cancelled = cancelled_turn_fixture();
    let expected_identity = (cancelled.session(), cancelled.turn());
    let terminal = TerminalChildTurn::from_cancelled(&cancelled);
    let outcome = DelegationOutcome::from_terminal_child(terminal, None)
        .expect("sealed cancelled turn derives child cancellation");
    let directly_derived = DelegationOutcome::from_cancelled_child(&cancelled);

    assert_eq!((terminal.session(), terminal.turn()), expected_identity);
    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildCancelled);
    assert_eq!(directly_derived, outcome);
}

/// oversized aggregate live completion is typed unavailable.
#[test]
fn oversized_live_completion_produces_unavailable_outcome() {
    let part = "x".repeat(DelegationContent::MAX_UTF8_BYTES / 2 + 1);
    let completed = completed_turn_fixture(&[part.as_str(), part.as_str()]);
    let terminal = TerminalChildTurn::from_completed(&completed)
        .expect("oversized live completion remains terminal child evidence");
    let outcome = DelegationOutcome::from_terminal_child(terminal, None)
        .expect("oversized completion derives an explicit failed outcome");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildFailed);
    assert_eq!(
        outcome.reason(),
        DelegationOutcomeReason::ChildResultUnavailable
    );
}

/// terminal proof authenticates returned content.
#[test]
fn terminal_proof_rejects_fabricated_content() {
    let expected = content("stored result");
    let returned = TerminalChildTurn {
        session: session_id(3),
        turn: turn_id(4),
        kind: TerminalChildTurnKind::Returned,
        reason: DelegationOutcomeReason::ChildCompleted,
        result_digest: Some(delegation_content_digest(&expected)),
    };

    assert_eq!(
        DelegationOutcome::from_terminal_child(returned, Some(content("fabricated"))),
        None
    );
}

/// child-origin terminal proof cannot select stopped.
#[test]
fn terminal_proof_cannot_construct_stopped_outcome() {
    let cancelled = TerminalChildTurn {
        session: session_id(3),
        turn: turn_id(4),
        kind: TerminalChildTurnKind::Cancelled,
        reason: DelegationOutcomeReason::ChildCancelled,
        result_digest: None,
    };

    assert_eq!(
        DelegationOutcome::from_terminal_child(cancelled, None)
            .expect("cancelled proof derives one outcome")
            .kind(),
        DelegationOutcomeKind::ChildCancelled
    );
}

/// parent-alone authority cannot disposition descendants.
#[test]
fn parent_alone_cannot_construct_descendant_outcome() {
    let authority = parent_termination_authority(DescendantTerminationScope::ParentAlone);

    assert_eq!(
        DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop),
        None
    );
}

/// parent-and-descendants authority admits bound policy.
#[test]
fn parent_and_descendants_constructs_policy_outcome() {
    let authority = parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);
    let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
        .expect("descendant-scoped authority may apply bound policy");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildStopped);
    assert_eq!(
        outcome.reason(),
        DelegationOutcomeReason::ParentStopped {
            scope: authority.scope(),
        }
    );
}

/// an already-terminal edge records its evaluating command.
#[test]
fn already_terminal_edge_has_typed_command_disposition() {
    let authority = parent_termination_authority(DescendantTerminationScope::ParentAndDescendants);
    let outcome = DelegationOutcome::from_parent_already_terminal(authority)
        .expect("descendant-scoped authority records terminal-edge evaluation");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::AlreadyTerminal);
    assert_eq!(
        outcome.reason(),
        DelegationOutcomeReason::ParentStopped {
            scope: authority.scope(),
        }
    );
    assert_eq!(outcome.provenance().parent_command(), Some(authority));
}

/// descendant-scoped cancellation selects its exact reason.
#[test]
fn parent_and_descendants_cancel_constructs_policy_outcome() {
    let authority = ParentTerminationAuthority {
        parent: session_id(1),
        source: ParentTerminationCommandSource::Turn { turn: turn_id(2) },
        command: command_id(3),
        kind: ParentTerminationKind::Cancelled,
        scope: DescendantTerminationScope::ParentAndDescendants,
    };
    let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
        .expect("descendant-scoped cancellation may apply bound policy");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ChildCancelled);
    assert_eq!(
        outcome.reason(),
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }
    );
}

/// goal-stop authority names a generation without a turn.
#[test]
fn goal_termination_authority_never_fabricates_a_turn() {
    let generation = GoalGeneration::new(std::num::NonZeroU64::MIN);
    let authority = ParentTerminationAuthority {
        parent: session_id(1),
        source: ParentTerminationCommandSource::Goal { generation },
        command: command_id(3),
        kind: ParentTerminationKind::Stopped,
        scope: DescendantTerminationScope::ParentAndDescendants,
    };
    let provenance = DelegationProvenance::from_parent_termination(authority);

    assert_eq!(authority.turn(), None);
    assert_eq!(authority.goal_generation(), Some(generation));
    assert_eq!(provenance.parent_command(), Some(authority));
}

#[test]
fn content_bound_reports_exact_utf8_length() {
    let oversized_utf8_bytes = DelegationContent::MAX_UTF8_BYTES + 1;
    let value = "x".repeat(oversized_utf8_bytes);
    let expected_failure = DelegationContentFailure::Oversized {
        utf8_byte_length: oversized_utf8_bytes,
    };
    let error = DelegationContent::try_new(value.clone())
        .expect_err("oversized content must fail without consuming the input");

    assert_eq!(error.failure(), expected_failure);
    assert_eq!(error.value(), value);
    expect!["delegation content is 1048577 bytes; maximum is 1048576"]
        .assert_eq(&error.to_string());
    let (rejected_value, failure) = error.into_parts();
    assert_eq!(rejected_value, value);
    assert_eq!(failure, expected_failure);
}
