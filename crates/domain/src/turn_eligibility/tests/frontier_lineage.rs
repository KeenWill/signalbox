//! Turn scheduling frontier lineage tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    FailedPredecessorPostAnchorOrigins, accepted_origin, activation,
    active_input_after_failed_predecessor_with_post_anchor_origin, configuration, current_session,
    frontier, semantic_entry,
};
use super::*;

/// S03: eligibility derives the target from complete durable
/// order and cannot be directed to skip earlier queued work.
#[test]
fn s03_eligibility_consumes_the_earliest_queued_origin() {
    let session = current_session();
    let later = accepted_origin(2);
    let earlier = accepted_origin(1);
    let later_record = later.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let earlier_record = earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let activation = activation(1);
    let no_active_acceptance_tail = None;
    let candidate = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![later_record, earlier_record],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the complete queue order is valid")
    .prepare_earliest_queued_activation(activation.identities())
    .expect("no active slot blocks the earliest queued work");

    assert_eq!(candidate.turn().turn(), earlier.turn());
    assert_eq!(
        candidate.turn().accepted_input().id(),
        earlier.accepted_input()
    );
}

/// S09: the earliest queued successor starts only
/// after the exact immediately preceding failed turn and retains its
/// complete origin-then-failure terminal prefix before appending its own
/// origin.
#[test]
fn s09_successor_uses_exact_failed_predecessor_terminal_frontier() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let no_active_acceptance_tail = None;
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record =
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        vec![
            predecessor_failure_entry.failed_turn(&session, predecessor),
            predecessor.entry(&session, predecessor_origin_entry),
        ],
        vec![
            predecessor_terminal_frontier.snapshot(
                &session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
        ],
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the failed predecessor has a complete validated frontier");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the successor is the earliest queued turn with no active slot");

    assert_eq!(candidate.turn().turn(), successor.turn());
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor.turn(),
        }
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            predecessor_origin_entry.reference(&session),
            predecessor_failure_entry.reference(&session),
            activation.origin_entry().reference(&session),
        ]
    );
}

/// S33: an actual frozen direct-model transition
/// inserts exactly one typed identity boundary between the predecessor
/// terminal frontier and the successor origin.
#[test]
fn s33_model_transition_extends_frontier_before_origin() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let successor_selection = direct(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let successor_choices = PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(successor_selection)),
    );
    let successor_configuration = OriginConfiguration::freeze(
        session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                    successor_selection,
                )),
            )
            .expect("the override is derived from current defaults"),
        |_| None,
    )
    .expect("the direct selection needs no alias resolution");
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record = AcceptedInputTurnSchedulingRecord::new(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::OriginOf(successor.turn()),
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: successor_choices,
        },
        successor_configuration,
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        vec![
            predecessor_failure_entry.failed_turn(&session, predecessor),
            predecessor.entry(&session, predecessor_origin_entry),
        ],
        vec![
            predecessor_terminal_frontier.snapshot(
                &session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
        ],
        None,
    )
    .reconstitute()
    .expect("the predecessor and changed successor are fully correlated");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the changed successor is eligible");

    assert_eq!(candidate.starting_entries().len(), 2);
    assert_eq!(
        candidate.starting_entries()[0].identity(),
        activation.model_identity_entry().id()
    );
    assert_eq!(
        candidate.starting_entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ModelIdentityChanged {
            turn: successor.turn(),
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selected: successor_selection,
        }
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            predecessor_origin_entry.reference(&session),
            predecessor_failure_entry.reference(&session),
            activation.model_identity_entry().reference(&session),
            activation.origin_entry().reference(&session),
        ]
    );
}

/// a durable legacy marker admits only a start whose
/// frontier was committed before model-identity boundaries existed.
#[test]
fn legacy_start_grandfathers_its_historical_frontier() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let queued = accepted_origin(3);
    let predecessor_selection = direct(2);
    let predecessor_choices = PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(predecessor_selection)),
    );
    let predecessor_configuration = OriginConfiguration::freeze(
        session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                    predecessor_selection,
                )),
            )
            .expect("the legacy override is derived from current defaults"),
        |_| None,
    )
    .expect("the direct selection needs no alias resolution");
    let mut input = active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        FailedPredecessorPostAnchorOrigins {
            predecessor,
            active,
            queued,
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    input.turns[0].origin_delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: predecessor_choices,
    };
    input.turns[0].origin_configuration = predecessor_configuration.clone();
    input.turns[0].configuration_provenance =
        TurnConfigurationProvenance::ExplicitOrigin(predecessor_configuration);

    let strict = input
        .clone()
        .reconstitute()
        .expect_err("a post-migration start cannot omit its changed-model boundary");
    assert_eq!(
        strict.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );

    input.turns[1] = input.turns[1]
        .clone()
        .without_legacy_model_identity_boundary();
    input
        .reconstitute()
        .expect("the durable legacy bit retains the historical marker-free frontier");
}

/// S08 / S09: terminally reclassified
/// steering becomes ordinary queued work at its original position and
/// inherits the source turn's canonical configuration.
#[test]
fn s08_s09_reclassified_steering_becomes_eligible_work() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let source_configuration = configuration(&session);
    let binding = crate::SteeringBinding::new(predecessor.turn());
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record = AcceptedInputTurnSchedulingRecord::reclassified(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor.turn(),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: predecessor.turn(),
        },
        binding,
        source_configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let mismatched_delivery_record = AcceptedInputTurnSchedulingRecord::reclassified(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor.turn(),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(99),
        },
        binding,
        source_configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let semantic_entries = vec![
        predecessor_failure_entry.failed_turn(&session, predecessor),
        predecessor.entry(&session, predecessor_origin_entry),
    ];
    let snapshots = vec![
        predecessor_terminal_frontier.snapshot(
            &session,
            &[predecessor_origin_entry, predecessor_failure_entry],
        ),
        predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
    ];
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![mismatched_delivery_record, predecessor_record.clone()],
        semantic_entries.clone(),
        snapshots.clone(),
        None,
    )
    .reconstitute()
    .expect_err("stored reclassified delivery must agree with its exact source binding");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: successor.turn(),
        }
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        semantic_entries,
        snapshots,
        None,
    )
    .reconstitute()
    .expect("reclassified steering is correlated to its terminal source");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the reclassified successor is eligible after its source");

    assert_eq!(candidate.turn().turn(), successor.turn());
    assert_eq!(candidate.turn().order(), successor.ordinary_order());
    assert_eq!(candidate.turn().configuration(), &source_configuration);
    assert_eq!(
        candidate.turn().configuration_provenance(),
        &TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding)
    );
}
