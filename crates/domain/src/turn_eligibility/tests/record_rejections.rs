//! Turn scheduling record rejections tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, FailedTerminalReconstitutionFacts, ReconstitutionFailureRow,
    accepted_origin, assert_input_rejects_unchanged, assert_reconstitution_rejects_unchanged,
    current_session, imported_session, imported_session_for, queued_input, semantic_entry,
};
use super::*;

/// imported ancestry is admitted only together
/// with its exact complete independently checked seed projection.
#[test]
fn imported_scheduling_requires_exact_seed_projection() {
    let imported = imported_session();
    let session = imported.session().clone();
    let queued = accepted_origin(1);

    let missing = assert_input_rejects_unchanged(queued_input(&session, queued));
    assert_eq!(
        missing,
        AcceptedInputSchedulingReconstitutionFailure::MissingImportedSession
    );

    let mismatched = assert_input_rejects_unchanged(
        queued_input(&session, queued).with_imported_session(imported_session_for(2)),
    );
    assert_eq!(
        mismatched,
        AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch
    );

    let unexpected = assert_input_rejects_unchanged(
        queued_input(&current_session(), queued).with_imported_session(imported),
    );
    assert_eq!(
        unexpected,
        AcceptedInputSchedulingReconstitutionFailure::UnexpectedImportedSession
    );
}

/// this closed slice still cannot resolve a first frontier
/// from native session ancestry, so an otherwise-valid queued projection
/// for a native ancestral session fails closed.
#[test]
fn reconstitution_rejects_ancestral_session() {
    let ancestral = session_id(1);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
    let session = SessionReconstitutionInput::new(
        ancestral,
        ancestral,
        SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(9),
                source_frontier: transcript_frontier(9),
            },
        ),
        ancestral,
        version,
        ancestral,
        version,
        defaults,
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: ancestral,
            current_pointer_version: crate::SessionPlacementVersion::INITIAL,
            selected_event_session: ancestral,
            selected_event: crate::VersionedSessionPlacement::initial(
                crate::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("ancestral session facts are fully correlated");
    let queued = accepted_origin(1);

    let failure = assert_input_rejects_unchanged(queued_input(&session, queued));

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry
    );
}

/// every stored session and turn correlation on one
/// scheduling record must repeat the owning identities exactly; each
/// cross-wired stored identity fails closed with its own failure.
#[test]
fn reconstitution_rejects_cross_wired_record_identities() {
    let session = current_session();
    let queued = accepted_origin(1);
    let other_session = session_id(2);
    let other_turn = turn_id(99);

    let mut turn_session_facts = queued_input(&session, queued);
    turn_session_facts.turns[0].stored_session = other_session;
    let turn_session = assert_input_rejects_unchanged(turn_session_facts);
    assert_eq!(
        turn_session,
        AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut accepted_input_session_facts = queued_input(&session, queued);
    accepted_input_session_facts.turns[0].accepted_input_session = other_session;
    let accepted_input_session = assert_input_rejects_unchanged(accepted_input_session_facts);
    assert_eq!(
        accepted_input_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut queue_session_facts = queued_input(&session, queued);
    queue_session_facts.turns[0].queue_session = other_session;
    let queue_session = assert_input_rejects_unchanged(queue_session_facts);
    assert_eq!(
        queue_session,
        AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut queue_turn_facts = queued_input(&session, queued);
    queue_turn_facts.turns[0].queue_turn = other_turn;
    let queue_turn = assert_input_rejects_unchanged(queue_turn_facts);
    assert_eq!(
        queue_turn,
        AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch {
            turn: queued.turn(),
        }
    );

    expect![[r#"
            ┌───────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                     │ failure                                                                             │
            ├───────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────┤
            │ turn record session cross-wired           │ TurnSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }          │
            │ accepted-input record session cross-wired │ AcceptedInputSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ queue record session cross-wired          │ QueueSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }         │
            │ queue record turn cross-wired             │ QueueTurnMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }            │
            └───────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "turn record session cross-wired",
                failure: format!("{turn_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "accepted-input record session cross-wired",
                failure: format!("{accepted_input_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record session cross-wired",
                failure: format!("{queue_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record turn cross-wired",
                failure: format!("{queue_turn:?}"),
            },
        ]));
}

/// two turn records cannot both claim one
/// accepted input as their typed durable origin.
#[test]
fn reconstitution_rejects_shared_accepted_input_identity() {
    let session = current_session();
    let first = accepted_origin(1);
    let second = accepted_origin(2);
    let mut input = queued_input(&session, first);
    input
        .turns
        .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    input.turns[1].accepted_input = AcceptedInputLifecycle::new(
        first.accepted_input(),
        AcceptedInputDisposition::OriginOf(second.turn()),
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
            accepted_input: first.accepted_input(),
        }
    );
}

/// a delegation-origin turn fact cannot also be represented
/// by an accepted-input lifecycle record.
#[test]
fn reconstitution_rejects_delegated_accepted_turn_fact() {
    let session = current_session();
    let queued = accepted_origin(1);
    let input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            queued.turn(),
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch {
            turn: queued.turn(),
        }
    );
}

/// complete delegation-origin turn facts cannot duplicate
/// the same stored turn identity.
#[test]
fn reconstitution_rejects_duplicate_delegated_turn_fact() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let fact = DelegatedTurnSchedulingFact::new(
        delegated,
        SessionConfigurationDefaultsVersion::first(),
        direct(1),
        DelegatedTurnSchedulingState::Active,
    );
    let input = queued_input(&session, queued).with_delegated_turn_facts(vec![fact, fact]);

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch { turn: delegated }
    );
}

/// a delegated model-identity entry must match
/// the exact configuration frozen by its stored turn origin.
#[test]
fn delegated_model_identity_requires_stored_configuration() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let identity_entry = semantic_entry(99);
    let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            delegated,
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            identity_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn: delegated,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selected: direct(2),
            },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: identity_entry.id(),
        }
    );
}

/// a delegated terminal semantic entry must match the
/// independently stored delegated lifecycle state.
#[test]
fn delegated_terminal_entry_requires_stored_lifecycle() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let failure_entry = semantic_entry(99);
    let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            delegated,
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: delegated },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: failure_entry.id(),
        }
    );
}

/// immutable queue facts that cannot form one
/// durable total order fail closed with the exact derivation error.
#[test]
fn reconstitution_rejects_underivable_queue_order() {
    let session = current_session();
    let first = accepted_origin(1);
    let second = accepted_origin(2);
    let mut input = queued_input(&session, first);
    input
        .turns
        .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    input.turns[1].order = AcceptedInputQueueOrder::ordinary(first.position());

    let failure = assert_input_rejects_unchanged(input);

    // Turn identities descend as acceptance ordinals ascend, so the
    // second fixture holds the lower canonical turn identity.
    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder {
            error: AcceptedInputQueueOrderError::DuplicateAcceptancePosition {
                position: first.position(),
                first_turn: second.turn(),
                second_turn: first.turn(),
            },
        }
    );
}

/// a stored semantic entry must name the scheduling
/// session as its source session.
#[test]
fn reconstitution_rejects_cross_session_semantic_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let other_session = session_id(2);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
        origin_entry.id(),
        other_session,
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: active.accepted_input(),
        },
    );

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
            entry: origin_entry.id(),
        }
    );
}

/// the same source-qualified semantic entry cannot appear
/// twice in the complete entry collection.
#[test]
fn reconstitution_rejects_duplicate_semantic_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .semantic_entries
        .push(active.entry(&session, origin_entry));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
            entry: origin_entry.reference(&session),
        }
    );
}

/// a failed marker naming a turn absent from the complete
/// scheduling inventory fails closed.
#[test]
fn reconstitution_rejects_semantic_entry_without_subject() {
    let session = current_session();
    let queued = accepted_origin(1);
    let unknown_turn = turn_id(99);
    let stray_entry = semantic_entry(31);
    let mut input = queued_input(&session, queued);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            stray_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: unknown_turn },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
            entry: stray_entry.id(),
        }
    );
}

/// an origin entry for a turn whose stored lifecycle is
/// still queued contradicts that turn's state and fails closed.
#[test]
fn reconstitution_rejects_origin_entry_for_queued_turn() {
    let session = current_session();
    let queued = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let mut input = queued_input(&session, queued);
    input
        .semantic_entries
        .push(queued.entry(&session, origin_entry));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: origin_entry.id(),
        }
    );
}

/// one started turn owns exactly one origin entry; a
/// second origin entry naming the same accepted input fails closed.
#[test]
fn reconstitution_rejects_second_origin_entry_for_one_turn() {
    let session = current_session();
    let active = accepted_origin(1);
    let second_origin_entry = semantic_entry(31);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .semantic_entries
        .push(active.entry(&session, second_origin_entry));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
            entry: second_origin_entry.id(),
        }
    );
}

/// a started turn requires its exact origin entry; an
/// absent origin fails closed instead of deriving a start without one.
#[test]
fn reconstitution_rejects_started_turn_without_origin_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    // The starting snapshot must stop referencing the removed entry, or
    // the snapshot-reference check would mask the origin-entry check.
    facts.semantic_entries.clear();
    facts.snapshots =
        vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry {
            turn: active.turn(),
        }
    );
}

/// a failed turn requires its exact failed
/// marker; an absent marker fails closed instead of accepting the
/// stored terminal frontier on faith.
#[test]
fn reconstitution_rejects_failed_turn_without_failure_marker() {
    let session = current_session();
    let failed = accepted_origin(1);
    let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    // The terminal snapshot must stop referencing the removed marker, or
    // the snapshot-reference check would mask the failed-marker check.
    facts.semantic_entries = vec![failed.entry(&session, origin_entry)];
    facts.snapshots = vec![
        FailedTerminalReconstitutionFacts::matching_starting_frontier()
            .snapshot(&session, &[origin_entry]),
        FailedTerminalReconstitutionFacts::matching_terminal_frontier()
            .snapshot(&session, &[origin_entry]),
    ];

    let failure = assert_input_rejects_unchanged(facts.input());

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry {
            turn: failed.turn(),
        }
    );
}
