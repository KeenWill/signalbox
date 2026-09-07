//! Session delegation aggregate tests for `docs/spec/sessions-and-transcript.md`.

use super::content::delegation_content_digest;
use super::request::{
    AWAIT_SESSION_TOOL_NAME, SEND_SESSION_MESSAGE_TOOL_NAME, SPAWN_SESSION_TOOL_NAME,
    relationship_argument, wait_mode_argument,
};
use super::terminal_turn::TerminalChildTurnKind;
use crate::{DelegationMessageId, GoalGeneration, SessionId, ToolRequest, ToolRequestId, TurnId};
use std::num::NonZeroU64;

use super::*;
use crate::{
    ApprovedToolRequest, InitialToolApproval, ModelCallId, NormalizedToolArguments,
    ToolApprovalResolutionReconstitutionInput, ToolCallProposal, ToolEffectClass, ToolName,
    ToolRequestOrdinal,
    model_execution::completed_turn_fixture,
    test_support::{
        command_id, delegation_message_id, model_call_id, session_id, tool_attempt_id,
        tool_request_id, turn_attempt_id, turn_id,
    },
};

const TEST_TASK: &str = "inspect aggregate work";

#[derive(Clone, Copy)]
enum RequestFixture {
    Spawn,
    Await,
    ParentMessage,
    ChildMessage,
    AnotherParentMessage,
}

impl RequestFixture {
    /// Canonical request identities are deliberately decorrelated across
    /// each named fixture. Reusing one fixture is the explicit way a test
    /// requests the same tool-request authority.
    fn identities(self) -> (ToolRequestId, TurnId, ModelCallId) {
        match self {
            Self::Spawn => (tool_request_id(101), turn_id(43), model_call_id(89)),
            Self::Await => (tool_request_id(103), turn_id(41), model_call_id(83)),
            Self::ParentMessage => (tool_request_id(107), turn_id(37), model_call_id(79)),
            Self::ChildMessage => (tool_request_id(109), turn_id(31), model_call_id(73)),
            Self::AnotherParentMessage => (tool_request_id(113), turn_id(29), model_call_id(71)),
        }
    }

    fn source(self) -> SessionId {
        match self {
            Self::ChildMessage => session_id(3),
            Self::Spawn | Self::Await | Self::ParentMessage | Self::AnotherParentMessage => {
                session_id(2)
            }
        }
    }
}

/// Canonical aggregate request fixture. Each named logical purpose owns its
/// source and fixed, decorrelated identity.
fn named_request(fixture: RequestFixture, name: &str, arguments: serde_json::Value) -> ToolRequest {
    let (request, turn, call) = fixture.identities();
    ToolRequest::from_model_proposal(
        request,
        fixture.source(),
        turn,
        call,
        ToolRequestOrdinal::from_u32(0),
        ToolCallProposal::new(
            ToolName::try_new(name.into()).expect("valid fixture name"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                .expect("valid fixture arguments"),
        ),
        InitialToolApproval::Confirm,
    )
}

fn dispatch_for(request: &ToolRequest) -> crate::ToolDispatchAuthority {
    let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        .reconstitute()
        .expect("fixture policy approves its exact request");
    let approved = ApprovedToolRequest::try_from_resolution(request.clone(), approval)
        .expect("fixture approval binds its exact request");
    let authorized = approved
        .prepare_attempt(
            tool_attempt_id(127),
            turn_attempt_id(131),
            ToolEffectClass::EffectFree,
        )
        .authorize()
        .expect("prepared fixture dispatch authorizes");
    crate::ToolDispatchAuthority::try_new(request.clone(), &authorized)
        .expect("fixture dispatch binds its exact request")
}

fn spawn_request(policy: ChildRelationshipPolicy) -> DelegatedSpawnRequest {
    let relationship = relationship_argument(policy);
    DelegatedSpawnRequest::parse(
        named_request(
            RequestFixture::Spawn,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": relationship,
                "task": TEST_TASK,
            }),
        ),
        TEST_TASK.into(),
        policy,
    )
    .expect("canonical aggregate spawn request")
}

fn await_request(
    fixture: RequestFixture,
    child: SessionId,
    mode: DelegationWaitMode,
) -> DelegationAwaitRequest {
    DelegationAwaitRequest::parse(
        named_request(
            fixture,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": child.as_uuid().to_string(),
                "mode": wait_mode_argument(mode),
            }),
        ),
        child,
        mode,
    )
    .expect("canonical aggregate await request")
}

fn message_request(
    fixture: RequestFixture,
    peer: SessionId,
    value: &str,
) -> DelegationMessageRequest {
    DelegationMessageRequest::parse(
        named_request(
            fixture,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": value,
                "peer_session_id": peer.as_uuid().to_string(),
            }),
        ),
        peer,
        value.into(),
    )
    .expect("canonical aggregate message request")
}

fn content(value: &str) -> DelegationContent {
    DelegationContent::try_new(value.into()).expect("bounded fixture content")
}

fn relation(policy: ChildRelationshipPolicy) -> SessionDelegation {
    SessionDelegation::spawn_fixture(spawn_request(policy), session_id(3), turn_id(7))
        .expect("fixture parent and child are distinct")
}

fn completed_child_relation(policy: ChildRelationshipPolicy) -> SessionDelegation {
    SessionDelegation::spawn_fixture(spawn_request(policy), session_id(1), turn_id(3))
        .expect("completed-turn fixture child is distinct from parent")
}

/// restart restores one complete active history.
#[test]
fn relation_reconstitution_round_trips_complete_active_history() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let relation = SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
        .expect("fixture parent and child are distinct");
    let sending = message_request(RequestFixture::ParentMessage, relation.child(), "progress");
    let dispatch = dispatch_for(sending.request());
    let (relation, _) = relation
        .deliver_message(sending, delegation_message_id(5), &dispatch)
        .expect("canonical message extends the relation");
    let input = SessionDelegationReconstitutionInput::new(
        spawning,
        relation.child(),
        relation.child_turn(),
        relation.events().to_vec(),
    );

    let reconstituted = input
        .reconstitute()
        .expect("complete stored history reconstitutes");

    assert_eq!(reconstituted, relation);
}

/// restart derives lifecycle from terminal history.
#[test]
fn relation_reconstitution_derives_terminal_lifecycle_from_history() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let relation = completed_child_relation(policy)
        .record_outcome(returned_outcome("done"))
        .expect("the exact child result terminalizes the relation");
    let input = SessionDelegationReconstitutionInput::new(
        spawning,
        relation.child(),
        relation.child_turn(),
        relation.events().to_vec(),
    );

    let reconstituted = input
        .reconstitute()
        .expect("complete terminal history reconstitutes");

    assert_eq!(reconstituted, relation);
    assert_eq!(reconstituted.lifecycle(), relation.lifecycle());
}

/// restart rejects a gap in relationship history.
#[test]
fn relation_reconstitution_rejects_noncontiguous_history() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let relation = SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
        .expect("fixture parent and child are distinct");
    let sending = message_request(RequestFixture::ParentMessage, relation.child(), "progress");
    let message = DelegationMessage::reconstitute(
        &sending,
        delegation_message_id(5),
        DelegationMessageDirection::ParentToChild,
        DelegationMessageEndpoints {
            parent: relation.parent(),
            child: relation.child(),
        },
    )
    .expect("fixture endpoints match the stored direction");
    let events = vec![
        relation.events()[0].clone(),
        DelegationEvent::MessageDelivered {
            ordinal: DelegationEventOrdinal::new(
                NonZeroU64::new(3).expect("fixture ordinal is positive"),
            ),
            message,
        },
    ];
    let input = SessionDelegationReconstitutionInput::new(
        spawning,
        relation.child(),
        relation.child_turn(),
        events,
    );

    let error = input
        .reconstitute()
        .expect_err("a skipped event ordinal is corrupt");

    assert_eq!(
        error.failure(),
        SessionDelegationReconstitutionFailure::NoncontiguousEventOrdinal
    );
}

/// restart rejects cross-wired message provenance.
#[test]
fn relation_reconstitution_rejects_cross_wired_message_direction() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let relation = SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
        .expect("fixture parent and child are distinct");
    let sending = message_request(RequestFixture::ChildMessage, relation.parent(), "progress");
    let message = DelegationMessage::reconstitute(
        &sending,
        delegation_message_id(5),
        DelegationMessageDirection::ParentToChild,
        DelegationMessageEndpoints {
            parent: relation.child(),
            child: relation.parent(),
        },
    )
    .expect("the swapped endpoint fixture admits its own direction");
    let input = SessionDelegationReconstitutionInput::new(
        spawning,
        relation.child(),
        relation.child_turn(),
        vec![
            relation.events()[0].clone(),
            DelegationEvent::MessageDelivered {
                ordinal: DelegationEventOrdinal::new(
                    NonZeroU64::new(2).expect("fixture ordinal is positive"),
                ),
                message,
            },
        ],
    );

    let error = input
        .reconstitute()
        .expect_err("the stored direction must match both endpoints");

    assert_eq!(
        error.failure(),
        SessionDelegationReconstitutionFailure::InvalidMessageProvenance
    );
}

/// restart rejects two parent authorities that reuse one durable command identity.
#[test]
fn reconstitution_rejects_reused_parent_command_identity() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let relation = SessionDelegation::spawn_fixture(spawning.clone(), session_id(3), turn_id(7))
        .expect("fixture parent and child are distinct");
    let first_authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let conflicting_authority = ParentTerminationAuthority {
        parent: first_authority.parent(),
        source: ParentTerminationCommandSource::Turn { turn: turn_id(11) },
        command: first_authority.command(),
        kind: ParentTerminationKind::Cancelled,
        scope: first_authority.scope(),
    };
    let first =
        DelegationOutcome::from_parent_policy(first_authority, BoundChildAction::KeepRunning)
            .expect("background policy records typed survival");
    let conflicting =
        DelegationOutcome::from_parent_policy(conflicting_authority, BoundChildAction::KeepRunning)
            .expect("background policy records typed survival");
    let input = SessionDelegationReconstitutionInput::new(
        spawning,
        relation.child(),
        relation.child_turn(),
        vec![
            relation.events()[0].clone(),
            DelegationEvent::OutcomeRecorded {
                ordinal: DelegationEventOrdinal::new(
                    NonZeroU64::new(2).expect("fixture ordinal is positive"),
                ),
                outcome: first,
            },
            DelegationEvent::OutcomeRecorded {
                ordinal: DelegationEventOrdinal::new(
                    NonZeroU64::new(3).expect("fixture ordinal is positive"),
                ),
                outcome: conflicting,
            },
        ],
    );

    let error = input
        .reconstitute()
        .expect_err("one command identity cannot denote two authorities");

    assert_eq!(
        error.failure(),
        SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority
    );
}

/// restart binds waits to one request and relation.
#[test]
fn wait_reconstitution_uses_exact_relation_and_request() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Foreground,
    );
    let dispatch = dispatch_for(awaiting.request());
    let recorded = relation
        .register_wait(&awaiting, &dispatch)
        .expect("live registration validates");

    let reconstituted = DelegationWait::reconstitute(&relation, &awaiting)
        .expect("stored exact request validates without dispatch authority");

    assert_eq!(reconstituted, recorded);
}

/// stored waits cannot reconstitute a self relationship.
#[test]
fn wait_reconstitution_rejects_same_session_endpoints() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.parent(),
        DelegationWaitMode::Foreground,
    );

    assert_eq!(
        DelegationWait::reconstitute_stored(
            &awaiting,
            relation.spawning_request(),
            relation.parent(),
            relation.parent(),
            DelegationWaitMode::Foreground,
        ),
        None
    );
}

/// immutable endpoint facts reconstitute an exact wait without loading the relationship event
/// stream.
#[test]
fn wait_reconstitution_uses_stored_endpoints_and_mode() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Foreground,
    );
    let expected = DelegationWait::reconstitute(&relation, &awaiting)
        .expect("aggregate-backed reconstitution validates");

    let reconstituted = DelegationWait::reconstitute_stored(
        &awaiting,
        relation.spawning_request(),
        relation.parent(),
        relation.child(),
        DelegationWaitMode::Foreground,
    )
    .expect("stored endpoint facts validate");

    assert_eq!(reconstituted, expected);
}

#[derive(Clone, Copy)]
enum TerminationAuthoritySource {
    Parent,
    ForeignParent,
}

impl TerminationAuthoritySource {
    fn session(self) -> SessionId {
        match self {
            Self::Parent => session_id(2),
            Self::ForeignParent => session_id(9),
        }
    }
}

fn parent_authority(
    authority_source: TerminationAuthoritySource,
    kind: ParentTerminationKind,
    scope: DescendantTerminationScope,
) -> ParentTerminationAuthority {
    let command_source = match kind {
        ParentTerminationKind::Stopped => ParentTerminationCommandSource::Goal {
            generation: GoalGeneration::new(NonZeroU64::MIN),
        },
        ParentTerminationKind::Cancelled => {
            ParentTerminationCommandSource::Turn { turn: turn_id(5) }
        }
    };
    ParentTerminationAuthority {
        parent: authority_source.session(),
        source: command_source,
        command: command_id(6),
        kind,
        scope,
    }
}

fn later_parent_authority(
    kind: ParentTerminationKind,
    scope: DescendantTerminationScope,
) -> ParentTerminationAuthority {
    let command_source = match kind {
        ParentTerminationKind::Stopped => ParentTerminationCommandSource::Goal {
            generation: GoalGeneration::new(NonZeroU64::new(2).expect("two is positive")),
        },
        ParentTerminationKind::Cancelled => {
            ParentTerminationCommandSource::Turn { turn: turn_id(11) }
        }
    };
    ParentTerminationAuthority {
        parent: session_id(2),
        source: command_source,
        command: command_id(13),
        kind,
        scope,
    }
}

fn returned_outcome(value: &str) -> DelegationOutcome {
    let content = content(value);
    let completed = completed_turn_fixture(&[value]);
    let terminal = TerminalChildTurn::from_completed(&completed)
        .expect("live completion seals terminal evidence");
    DelegationOutcome::from_terminal_child(terminal, Some(content))
        .expect("exact live content authenticates its outcome")
}

fn cancelled_outcome(child: SessionId) -> DelegationOutcome {
    let terminal = TerminalChildTurn {
        session: child,
        turn: turn_id(7),
        kind: TerminalChildTurnKind::Cancelled,
        reason: DelegationOutcomeReason::ChildCancelled,
        result_digest: None,
    };
    DelegationOutcome::from_terminal_child(terminal, None)
        .expect("cancelled terminal evidence seals its outcome")
}

#[test]
fn delegation_outcome_reconstitution_round_trips_child_evidence() {
    let returned = returned_outcome("checked result");
    let reconstituted_returned = DelegationOutcome::reconstitute(
        returned.kind(),
        returned.content().cloned(),
        returned.reason(),
        returned.reconstitution_provenance(),
    );
    let cancelled = cancelled_outcome(session_id(3));
    let reconstituted_cancelled = DelegationOutcome::reconstitute(
        cancelled.kind(),
        cancelled.content().cloned(),
        cancelled.reason(),
        cancelled.reconstitution_provenance(),
    );
    assert_eq!(reconstituted_returned, Some(returned));
    assert_eq!(reconstituted_cancelled, Some(cancelled));
}

#[test]
fn delegation_outcome_reconstitution_round_trips_parent_command_evidence() {
    let stopped_authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let stopped =
        DelegationOutcome::from_parent_policy(stopped_authority, BoundChildAction::KeepRunning)
            .expect("descendant-scoped stopped authority evaluates policy");
    let cancelled_authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Cancelled,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let cancelled =
        DelegationOutcome::from_parent_policy(cancelled_authority, BoundChildAction::Cancel)
            .expect("descendant-scoped cancelled authority evaluates policy");
    assert_eq!(
        DelegationOutcome::reconstitute(
            stopped.kind(),
            stopped.content().cloned(),
            stopped.reason(),
            stopped.reconstitution_provenance(),
        ),
        Some(stopped)
    );
    assert_eq!(
        DelegationOutcome::reconstitute(
            cancelled.kind(),
            cancelled.content().cloned(),
            cancelled.reason(),
            cancelled.reconstitution_provenance(),
        ),
        Some(cancelled)
    );
}

#[test]
fn delegation_outcome_reconstitution_rejects_cross_wired_shapes() {
    let child = DelegationProvenanceReconstitutionInput::ChildTurn {
        session: session_id(3),
        turn: turn_id(7),
    };
    let parent = DelegationProvenanceReconstitutionInput::ParentTurnCommand {
        session: session_id(2),
        turn: turn_id(5),
        command: command_id(6),
    };
    assert_eq!(
        DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ResultReturned,
            Some(content("result")),
            DelegationOutcomeReason::ChildCompleted,
            parent,
        ),
        None
    );
    assert_eq!(
        DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ChildStopped,
            None,
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            },
            parent,
        ),
        None
    );
    assert_eq!(
        DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ChildFailed,
            None,
            DelegationOutcomeReason::ChildCancelled,
            child,
        ),
        None
    );
}

#[track_caller]
fn rejected_spawn(error: DelegationTransitionError) -> (DelegatedSpawnRequest, SessionId, TurnId) {
    let Some(RejectedDelegationTransition::Spawn {
        request,
        child,
        child_turn,
    }) = error.into_rejected()
    else {
        panic!("spawn rejection retains typed request, child, and delegated-task turn");
    };
    (request, child, child_turn)
}

#[track_caller]
fn rejected_message(
    error: DelegationTransitionError,
) -> (
    SessionDelegation,
    DelegationMessageRequest,
    DelegationMessageId,
) {
    let Some(RejectedDelegationTransition::DeliverMessage {
        relation,
        request,
        id,
    }) = error.into_rejected()
    else {
        panic!("message rejection retains aggregate and typed request");
    };
    (relation, request, id)
}

#[track_caller]
fn rejected_outcome(error: DelegationTransitionError) -> (SessionDelegation, DelegationOutcome) {
    let Some(RejectedDelegationTransition::RecordOutcome { relation, outcome }) =
        error.into_rejected()
    else {
        panic!("outcome rejection retains aggregate and opaque outcome");
    };
    (relation, outcome)
}

#[track_caller]
fn rejected_parent_termination(
    error: DelegationTransitionError,
) -> (SessionDelegation, ParentTerminationAuthority) {
    let Some(RejectedDelegationTransition::RecordParentTermination {
        relation,
        authority,
    }) = error.into_rejected()
    else {
        panic!("parent-termination rejection retains aggregate and authority");
    };
    (relation, authority)
}

fn assert_standard_error<T: std::error::Error>() {}

#[test]
fn delegation_public_errors_implement_standard_error_contract() {
    assert_standard_error::<DelegationContentError>();
    assert_standard_error::<DelegationRequestError>();
    assert_standard_error::<DelegationTransitionError>();
}

/// spawn retains the exact sealed request facts and derives delegated creation without ancestry.
#[test]
fn aggregate_spawn_retains_policy_task_and_provenance() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let request = spawn_request(policy);
    let spawning_request = request.request().id();
    let task = request.task().clone();
    let child_turn = turn_id(7);
    let relation = SessionDelegation::spawn_fixture(request, session_id(3), child_turn)
        .expect("distinct child");

    assert_eq!(relation.spawning_request(), spawning_request);
    assert_eq!(relation.child_turn(), child_turn);
    assert_eq!(relation.task(), &task);
    assert_eq!(relation.policy(), policy);
    assert_eq!(relation.events()[0].ordinal().get(), 1);
    assert_eq!(
        relation.child_creation_provenance().cause(),
        crate::SessionCreationCause::Delegated { spawning_request }
    );
    assert_eq!(
        relation.child_creation_provenance().ancestry(),
        crate::TranscriptAncestry::None
    );
}

/// a session cannot delegate to itself, and rejection is lossless.
#[test]
fn same_session_spawn_rejection_returns_exact_inputs() {
    let request = spawn_request(ChildRelationshipPolicy::Background);
    let child = session_id(2);
    let child_turn = turn_id(7);
    let error = SessionDelegation::spawn_fixture(request.clone(), child, child_turn)
        .expect_err("a child must be distinct from its parent");

    assert_eq!(error.failure(), DelegationTransitionFailure::SameSession);
    let (returned_request, returned_child, returned_child_turn) = rejected_spawn(error);
    assert_eq!(returned_request, request);
    assert_eq!(returned_child, child);
    assert_eq!(returned_child_turn, child_turn);
}

/// foreground wait retains the exact child subject.
#[test]
fn foreground_registration_yields_exact_child_wait() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Foreground,
    );
    let expected_request = awaiting.request().id();
    let dispatch = dispatch_for(awaiting.request());
    let wait = relation
        .register_wait(&awaiting, &dispatch)
        .expect("parent may await its exact child");
    let subject = wait
        .foreground_subject()
        .expect("foreground wait retains turn subject");

    assert_eq!(subject.awaiting_request(), expected_request);
    assert_eq!(subject.spawning_request(), relation.spawning_request());
    assert_eq!(subject.child(), relation.child());
}

/// background wait releases the parent turn subject.
#[test]
fn background_registration_has_no_child_wait() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let dispatch = dispatch_for(awaiting.request());
    let wait = relation
        .register_wait(&awaiting, &dispatch)
        .expect("parent may await its exact child");

    assert_eq!(wait.mode(), DelegationWaitMode::Background);
    assert_eq!(wait.foreground_subject(), None);
}

/// a typed await for another child cannot cross relations.
#[test]
fn wait_registration_rejects_relation_child_cross_wiring() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        session_id(9),
        DelegationWaitMode::Background,
    );
    let dispatch = dispatch_for(awaiting.request());
    let error = relation
        .register_wait(&awaiting, &dispatch)
        .expect_err("another child cannot reuse this relation");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::InvalidProvenance
    );
}

/// one request cannot both spawn and await a child.
#[test]
fn wait_registration_requires_distinct_parent_work() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Spawn,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let dispatch = dispatch_for(awaiting.request());
    let error = relation
        .register_wait(&awaiting, &dispatch)
        .expect_err("spawn request identity cannot also register a wait");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::InvalidProvenance
    );
}

/// wait registration requires its exact in-flight dispatch.
#[test]
fn wait_registration_rejects_foreign_dispatch_authority() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let other_request = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "other dispatched work",
    );
    let foreign_dispatch = dispatch_for(other_request.request());
    let error = relation
        .register_wait(&awaiting, &foreign_dispatch)
        .expect_err("another dispatch cannot authorize this wait");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::InvalidProvenance
    );
}

/// dispatch authority binds the complete await request.
#[test]
fn wait_registration_rejects_same_identity_argument_drift() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let dispatched = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let drifted = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Foreground,
    );
    let dispatch = dispatch_for(dispatched.request());
    let error = relation
        .register_wait(&drifted, &dispatch)
        .expect_err("same identities cannot substitute different await arguments");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::InvalidProvenance
    );
}

/// each relation peer derives one exact message direction.
#[test]
fn messages_are_bidirectional() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let parent_message = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "parent update",
    );
    let child_message = message_request(
        RequestFixture::ChildMessage,
        relation.parent(),
        "child update",
    );
    let parent_dispatch = dispatch_for(parent_message.request());
    let child_dispatch = dispatch_for(child_message.request());
    let (relation, first) = relation
        .deliver_message(parent_message, delegation_message_id(5), &parent_dispatch)
        .expect("parent message is related");
    let (_relation, second) = relation
        .deliver_message(child_message, delegation_message_id(6), &child_dispatch)
        .expect("child message is related");

    assert_eq!(
        first.message().expect("message event").direction(),
        DelegationMessageDirection::ParentToChild
    );
    assert_eq!(
        second.message().expect("message event").direction(),
        DelegationMessageDirection::ChildToParent
    );
}

/// distinct message deliveries receive contiguous ordinals.
#[test]
fn message_delivery_ordinals_are_contiguous() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let parent_message = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "parent update",
    );
    let child_message = message_request(
        RequestFixture::ChildMessage,
        relation.parent(),
        "child update",
    );
    let parent_dispatch = dispatch_for(parent_message.request());
    let child_dispatch = dispatch_for(child_message.request());
    let (relation, first) = relation
        .deliver_message(parent_message, delegation_message_id(5), &parent_dispatch)
        .expect("parent message is related");
    let (_relation, second) = relation
        .deliver_message(child_message, delegation_message_id(6), &child_dispatch)
        .expect("child message is related");

    assert_eq!(first.ordinal().get(), 2);
    assert_eq!(second.ordinal().get(), 3);
}

/// nonterminal messages preserve the final relationship ordinal for a typed terminal outcome.
#[test]
fn message_reserves_terminal_event_ordinal() {
    let policy = ChildRelationshipPolicy::Background;
    let spawning = spawn_request(policy);
    let mut relation = relation(policy);
    relation.events = vec![DelegationEvent::Spawned {
        ordinal: DelegationEventOrdinal::new(
            NonZeroU64::new(u64::MAX - 1).expect("fixture ordinal is positive"),
        ),
        provenance: DelegationProvenance::from_spawn(&spawning),
    }];
    let request = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "reserved terminal position",
    );
    let dispatch = dispatch_for(request.request());
    let error = relation
        .deliver_message(request, delegation_message_id(5), &dispatch)
        .expect_err("the final ordinal remains reserved for terminal evidence");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::EventOrdinalExhausted
    );
}

/// a typed message for another peer returns exact inputs.
#[test]
fn message_rejects_relation_peer_cross_wiring_and_returns_input() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let request = message_request(RequestFixture::ParentMessage, session_id(9), "misdirected");
    let id = delegation_message_id(5);
    let dispatch = dispatch_for(request.request());
    let error = relation
        .clone()
        .deliver_message(request.clone(), id, &dispatch)
        .expect_err("another peer cannot cross this relation");
    let (returned_relation, returned_request, returned_id) = rejected_message(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_request, request);
    assert_eq!(returned_id, id);
}

/// message delivery requires its exact in-flight dispatch.
#[test]
fn message_rejects_foreign_dispatch_and_returns_input() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let request = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "dispatched message",
    );
    let other_request = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let foreign_dispatch = dispatch_for(other_request.request());
    let id = delegation_message_id(5);
    let error = relation
        .clone()
        .deliver_message(request.clone(), id, &foreign_dispatch)
        .expect_err("another dispatch cannot authorize this message");
    let (returned_relation, returned_request, returned_id) = rejected_message(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_request, request);
    assert_eq!(returned_id, id);
}

/// dispatch authority binds the complete message request.
#[test]
fn message_rejects_same_identity_content_drift() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let dispatched = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "authorized content",
    );
    let drifted = message_request(
        RequestFixture::ParentMessage,
        relation.child(),
        "substituted content",
    );
    let dispatch = dispatch_for(dispatched.request());
    let id = delegation_message_id(5);
    let error = relation
        .clone()
        .deliver_message(drifted.clone(), id, &dispatch)
        .expect_err("same identities cannot substitute different message content");
    let (returned_relation, returned_request, returned_id) = rejected_message(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_request, drifted);
    assert_eq!(returned_id, id);
}

/// one logical message request appends at most one event.
#[test]
fn message_request_replay_returns_persisted_event() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let request = message_request(RequestFixture::ParentMessage, relation.child(), "once");
    let dispatch = dispatch_for(request.request());
    let (relation, first) = relation
        .deliver_message(request.clone(), delegation_message_id(5), &dispatch)
        .expect("first delivery appends");
    let (relation, replay) = relation
        .deliver_message(request, delegation_message_id(9), &dispatch)
        .expect("equal request replay returns persisted event");

    assert_eq!(replay, first);
    assert_eq!(relation.events().len(), 2);
}

/// a message identity cannot name another logical request.
#[test]
fn duplicate_message_identity_returns_attempted_request() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let first = message_request(RequestFixture::ParentMessage, relation.child(), "first");
    let second = message_request(
        RequestFixture::AnotherParentMessage,
        relation.child(),
        "second",
    );
    let id = delegation_message_id(5);
    let first_dispatch = dispatch_for(first.request());
    let second_dispatch = dispatch_for(second.request());
    let (relation, _) = relation
        .deliver_message(first, id, &first_dispatch)
        .expect("first identity is unused");
    let error = relation
        .clone()
        .deliver_message(second.clone(), id, &second_dispatch)
        .expect_err("identity reuse is rejected");
    let (returned_relation, returned_request, returned_id) = rejected_message(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_request, second);
    assert_eq!(returned_id, id);
}

/// a replay cannot change content under one request authority.
#[test]
fn conflicting_message_replay_reports_code_and_returns_inputs() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let first = message_request(RequestFixture::ParentMessage, relation.child(), "first");
    let conflicting = message_request(RequestFixture::ParentMessage, relation.child(), "changed");
    let first_dispatch = dispatch_for(first.request());
    let conflicting_dispatch = dispatch_for(conflicting.request());
    let conflicting_id = delegation_message_id(6);
    let (relation, _) = relation
        .deliver_message(first, delegation_message_id(5), &first_dispatch)
        .expect("first request authority appends");
    let error = relation
        .clone()
        .deliver_message(conflicting.clone(), conflicting_id, &conflicting_dispatch)
        .expect_err("one request authority cannot carry changed content");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::ConflictingMessageReplay
    );
    let (returned_relation, returned_request, returned_id) = rejected_message(error);
    assert_eq!(returned_relation, relation);
    assert_eq!(returned_request, conflicting);
    assert_eq!(returned_id, conflicting_id);
}

/// returned child result terminalizes exactly once.
#[test]
fn returned_result_terminalizes_and_replays() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background);
    let outcome = returned_outcome("child result");
    let relation = relation
        .record_outcome(outcome.clone())
        .expect("exact child result is related");
    let replayed = relation
        .clone()
        .record_outcome(outcome)
        .expect("equal outcome replay is idempotent");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(replayed, relation);
    assert_eq!(relation.events().len(), 2);
}

/// terminal evidence must name this spawn's delegated-task turn.
#[test]
fn result_rejects_a_later_child_turn() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background);
    let returned = content("later turn result");
    let terminal = TerminalChildTurn {
        session: relation.child(),
        turn: turn_id(7),
        kind: TerminalChildTurnKind::Returned,
        reason: DelegationOutcomeReason::ChildCompleted,
        result_digest: Some(delegation_content_digest(&returned)),
    };
    let outcome = DelegationOutcome::from_terminal_child(terminal, Some(returned))
        .expect("the later turn has independently valid terminal evidence");
    let error = relation
        .clone()
        .record_outcome(outcome.clone())
        .expect_err("another child turn cannot satisfy this spawn");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::OutcomeReasonMismatch
    );
    let (returned_relation, returned_outcome) = rejected_outcome(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, outcome);
}

/// later descendant evaluation is explicit on a child-terminal edge.
#[test]
fn child_terminal_edge_records_parent_command_disposition() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background)
        .record_outcome(returned_outcome("terminal result"))
        .expect("returned result terminalizes relation");
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("a child result authenticates the terminal edge");
    let recorded = relation.events().last().and_then(DelegationEvent::outcome);

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(
        recorded.map(DelegationOutcome::kind),
        Some(DelegationOutcomeKind::AlreadyTerminal)
    );
    assert_eq!(
        recorded.map(DelegationOutcome::provenance),
        Some(DelegationProvenance::from_parent_termination(authority))
    );
    assert_eq!(relation.events().len(), 3);
}

/// a prior policy terminal result remains explicit on re-evaluation.
#[test]
fn policy_terminal_edge_records_later_command_disposition() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let first_authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation(policy)
        .record_parent_termination(first_authority)
        .expect("the first policy disposition terminalizes the edge");
    let later_authority = later_parent_authority(
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(later_authority)
        .expect("a policy terminal result authenticates later evaluation");
    let recorded = relation.events().last().and_then(DelegationEvent::outcome);

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(
        recorded.map(DelegationOutcome::kind),
        Some(DelegationOutcomeKind::AlreadyTerminal)
    );
    assert_eq!(
        recorded.map(DelegationOutcome::provenance),
        Some(DelegationProvenance::from_parent_termination(
            later_authority
        ))
    );
    assert_eq!(relation.events().len(), 3);
}

/// parent-alone scope never evaluates a child edge.
#[test]
fn parent_alone_transition_returns_exact_unevaluated_inputs() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAlone,
    );
    let error = relation
        .clone()
        .record_parent_termination(authority)
        .expect_err("parent-alone scope does not evaluate descendants");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::DescendantsNotSelected
    );
    let (returned_relation, returned_authority) = rejected_parent_termination(error);
    assert_eq!(returned_relation, relation);
    assert_eq!(returned_authority, authority);
}

/// a different authority cannot append after terminalization.
#[test]
fn already_terminal_rejection_reports_code_and_returns_inputs() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background)
        .record_outcome(returned_outcome("terminal result"))
        .expect("returned result terminalizes relation");
    let outcome = cancelled_outcome(relation.child());
    let error = relation
        .clone()
        .record_outcome(outcome.clone())
        .expect_err("another terminal authority cannot append");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::AlreadyTerminal
    );
    let (returned_relation, returned_outcome) = rejected_outcome(error);
    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, outcome);
}

/// one authority cannot replay with a different outcome.
#[test]
fn duplicate_outcome_authority_reports_code_and_returns_inputs() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let relation = relation(policy);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let first = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Stop)
        .expect("descendant authority admits the chosen stop policy");
    let conflicting = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
        .expect("the same authority can express the conflicting attempted action");
    let relation = relation
        .record_outcome(first)
        .expect("chosen stop policy terminalizes relation");
    let error = relation
        .clone()
        .record_outcome(conflicting.clone())
        .expect_err("one authority cannot select two outcomes");

    assert_eq!(
        error.failure(),
        DelegationTransitionFailure::DuplicateOutcomeAuthority
    );
    let (returned_relation, returned_outcome) = rejected_outcome(error);
    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, conflicting);
}

/// another child's sealed result returns unchanged.
#[test]
fn returned_result_rejects_foreign_child_proof() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let outcome = returned_outcome("foreign child result");
    let error = relation
        .clone()
        .record_outcome(outcome.clone())
        .expect_err("terminal proof belongs to fixture child one");
    let (returned_relation, returned_outcome) = rejected_outcome(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, outcome);
}

/// a terminal result still accepts a late wait registration.
#[test]
fn terminal_result_accepts_late_wait() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background)
        .record_outcome(returned_outcome("late result"))
        .expect("child result terminalizes relation");
    let awaiting = await_request(
        RequestFixture::Await,
        relation.child(),
        DelegationWaitMode::Background,
    );
    let dispatch = dispatch_for(awaiting.request());
    let wait = relation
        .register_wait(&awaiting, &dispatch)
        .expect("late wait registration remains valid");

    assert_eq!(wait.mode(), DelegationWaitMode::Background);
    assert_eq!(wait.child(), relation.child());
}

/// messages remain available after child terminalization.
#[test]
fn message_is_recorded_after_child_terminalizes() {
    let relation = completed_child_relation(ChildRelationshipPolicy::Background)
        .record_outcome(returned_outcome("done"))
        .expect("child result terminalizes relation");
    let request = message_request(RequestFixture::ParentMessage, relation.child(), "afterward");
    let dispatch = dispatch_for(request.request());
    let (relation, event) = relation
        .deliver_message(request, delegation_message_id(5), &dispatch)
        .expect("terminal relation still records messages");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(event.ordinal().get(), 3);
}

/// child cancellation retains child-turn provenance.
#[test]
fn child_cancel_records_child_turn_provenance() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let outcome = cancelled_outcome(relation.child());
    let relation = relation
        .record_outcome(outcome)
        .expect("child cancellation belongs to relation");
    let recorded = relation.events()[1].outcome().expect("outcome event");

    assert_eq!(recorded.kind(), DelegationOutcomeKind::ChildCancelled);
    assert_eq!(
        recorded.provenance().child_turn(),
        Some((relation.child(), relation.child_turn()))
    );
}

/// a bound keep-running action remains active and explicit.
#[test]
fn bound_keep_running_records_no_change() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::KeepRunning,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let relation = relation(policy);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("bound keep-running policy matches outcome");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
    assert_eq!(relation.events().len(), 2);
}

/// continue-running replay does not append another event.
#[test]
fn continue_running_replay_is_idempotent() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("first disposition appends");
    let replayed = relation
        .clone()
        .record_parent_termination(authority)
        .expect("equal disposition replay is idempotent");

    assert_eq!(replayed, relation);
    assert_eq!(relation.events().len(), 2);
}

/// background child survives parent stop explicitly.
#[test]
fn background_child_survives_parent_stop_with_typed_outcome() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("background policy records survival");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
    assert_eq!(
        relation.events()[1]
            .outcome()
            .expect("outcome event")
            .kind(),
        DelegationOutcomeKind::ContinueRunning
    );
}

/// background child survives parent cancellation explicitly.
#[test]
fn background_child_survives_parent_cancel_with_typed_outcome() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Cancelled,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("background policy records survival");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
    assert_eq!(
        relation.events()[1]
            .outcome()
            .expect("outcome event")
            .reason(),
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }
    );
}

/// bound child follows its chosen parent-stop policy.
#[test]
fn bound_child_follows_parent_stop_policy() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let relation = relation(policy);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("bound stop policy matches outcome");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(
        relation.events()[1]
            .outcome()
            .expect("outcome event")
            .kind(),
        DelegationOutcomeKind::ChildStopped
    );
}

/// bound child follows its chosen parent-cancel policy.
#[test]
fn bound_child_follows_parent_cancel_policy() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let relation = relation(policy);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Cancelled,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let relation = relation
        .record_parent_termination(authority)
        .expect("bound cancel policy matches outcome");

    assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    assert_eq!(
        relation.events()[1]
            .outcome()
            .expect("outcome event")
            .kind(),
        DelegationOutcomeKind::ChildCancelled
    );
}

/// parent authority cannot override the chosen edge action.
#[test]
fn parent_outcome_rejects_wrong_policy_action() {
    let policy = ChildRelationshipPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let relation = relation(policy);
    let authority = parent_authority(
        TerminationAuthoritySource::Parent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::Cancel)
        .expect("descendant-scoped parent authority");
    let error = relation
        .clone()
        .record_outcome(outcome.clone())
        .expect_err("spawn policy rejects a different action");
    let (returned_relation, returned_outcome) = rejected_outcome(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, outcome);
}

/// foreign parent authority returns aggregate and outcome.
#[test]
fn parent_outcome_rejects_foreign_termination_authority() {
    let relation = relation(ChildRelationshipPolicy::Background);
    let authority = parent_authority(
        TerminationAuthoritySource::ForeignParent,
        ParentTerminationKind::Stopped,
        DescendantTerminationScope::ParentAndDescendants,
    );
    let outcome = DelegationOutcome::from_parent_policy(authority, BoundChildAction::KeepRunning)
        .expect("descendant-scoped foreign authority");
    let error = relation
        .clone()
        .record_outcome(outcome.clone())
        .expect_err("foreign parent cannot disposition relation");
    let (returned_relation, returned_outcome) = rejected_outcome(error);

    assert_eq!(returned_relation, relation);
    assert_eq!(returned_outcome, outcome);
}
