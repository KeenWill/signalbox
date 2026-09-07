//! Turn scheduling delegated activation tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{configuration, current_session};
use super::*;

#[test]
fn delegated_activation_preserves_task_origin_and_first_session_lineage() {
    let child = current_session();
    let spawning_request = tool_request_id(401);
    let child_turn = turn_id(402);
    let task = DelegationContent::try_new(String::from("inspect delegated work"))
        .expect("fixture task is valid");
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(403),
        child.id(),
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: session_id(404),
            parent_turn: turn_id(405),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session: child.id(),
        turn: child_turn,
        spawning_request,
        task: task.clone(),
        task_entry,
        configuration: configuration(&child),
        starting_frontier: context_frontier_id(406),
        initial_attempt: turn_attempt_id(407),
    })
    .expect("exact delegated task facts prepare activation");
    let (active, origin, snapshot) = prepared.into_parts();

    assert_eq!(active.session(), child.id());
    assert_eq!(active.turn(), child_turn);
    assert_eq!(active.spawning_request(), Some(spawning_request));
    assert_eq!(active.task(), Some(&task));
    assert_eq!(
        active.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(snapshot.entry_count(), 1);
    assert_eq!(origin.len(), 1);
    assert_eq!(
        origin.first().unwrap().reference(),
        snapshot.ordered_entries().next().unwrap()
    );
}

#[test]
fn delegated_activation_reconstitutes_consumed_steering() {
    let child = current_session();
    let child_turn = turn_id(408);
    let task = DelegationContent::try_new(String::from("inspect delegated steering"))
        .expect("fixture task is valid");
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(409),
        child.id(),
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request: tool_request_id(410),
            parent_session: session_id(411),
            parent_turn: turn_id(412),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session: child.id(),
        turn: child_turn,
        spawning_request: tool_request_id(410),
        task,
        task_entry,
        configuration: configuration(&child),
        starting_frontier: context_frontier_id(413),
        initial_attempt: turn_attempt_id(414),
    })
    .expect("exact delegated task facts prepare activation");
    let consumed_input = accepted_input_id(415);
    let consuming_call = model_call_id(416);
    let position = SessionInputPosition::try_from_u64(2).unwrap();
    let consumed = ConsumedSteeringReconstitutionInput::new(
        child.id(),
        AcceptedInputLifecycle::new(
            consumed_input,
            AcceptedInputDisposition::ConsumedAsSteering {
                call: consuming_call,
            },
        ),
        position,
        child_turn,
    );

    let active = prepared
        .into_parts()
        .0
        .with_consumed_steering(vec![consumed])
        .expect("stored steering targets the delegated turn");

    assert_eq!(active.consumed_steering().len(), 1);
    assert_eq!(
        active.consumed_steering()[0].accepted_input(),
        consumed_input
    );
    assert_eq!(
        active.consumed_steering()[0].acceptance_position(),
        position
    );
    assert_eq!(active.consumed_steering()[0].source_turn(), child_turn);
}

#[test]
fn delegated_wake_activation_preserves_delivery_range_and_predecessor_lineage() {
    let recipient = current_session();
    let predecessor = turn_id(411);
    let predecessor_entry =
        SemanticTranscriptEntryRef::from_source(recipient.id(), semantic_transcript_entry_id(412));
    let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        recipient.id(),
        context_frontier_id(413),
        vec![predecessor_entry],
    )
    .expect("fixture predecessor snapshot is valid");
    let first_sequence = NonZeroU64::new(1).unwrap();
    let through_sequence = NonZeroU64::new(2).unwrap();
    let first_delivery = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(414),
        recipient.id(),
        SemanticTranscriptEntryPayload::DelegationMessage {
            spawning_request: tool_request_id(415),
            message: delegation_message_id(416),
            sender: session_id(417),
            recipient: recipient.id(),
            delivery_sequence: first_sequence,
            content: DelegationContent::try_new(String::from("first wake message")).unwrap(),
        },
    );
    let through_delivery = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(418),
        recipient.id(),
        SemanticTranscriptEntryPayload::DelegationMessage {
            spawning_request: tool_request_id(415),
            message: delegation_message_id(419),
            sender: session_id(417),
            recipient: recipient.id(),
            delivery_sequence: through_sequence,
            content: DelegationContent::try_new(String::from("second wake message")).unwrap(),
        },
    );
    let prepared =
        PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
            session: recipient.id(),
            turn: turn_id(420),
            first_delivery_sequence: first_sequence,
            through_delivery_sequence: through_sequence,
            deliveries: vec![first_delivery, through_delivery],
            predecessor,
            predecessor_snapshot,
            configuration: configuration(&recipient),
            starting_frontier: context_frontier_id(421),
            initial_attempt: turn_attempt_id(422),
        })
        .expect("contiguous checked deliveries prepare a wake activation");
    let (active, entries, snapshot) = prepared.into_parts();

    assert_eq!(active.spawning_request(), None);
    assert_eq!(active.task(), None);
    assert_eq!(
        active.delivery_range(),
        Some((first_sequence, through_sequence))
    );
    assert_eq!(
        active.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor
        }
    );
    assert_eq!(entries.len(), 2);
    assert_eq!(snapshot.entry_count(), 3);
    assert_eq!(
        snapshot.immediate_semantic_prefix().unwrap().snapshot(),
        context_frontier_id(413)
    );
}
