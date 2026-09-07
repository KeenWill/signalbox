//! Turn scheduling snapshot rejections tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, FailedTerminalReconstitutionFacts, OriginRecordFacts,
    ReconstitutionFailureRow, accepted_origin, assert_input_rejects_unchanged,
    assert_reconstitution_rejects_unchanged, current_session, frontier, matching_active_attempt,
    queued_input, semantic_entry,
};
use super::*;

/// a supplied acceptance tail requires an active turn; a
/// tail alongside a queued-only projection fails closed.
#[test]
fn reconstitution_rejects_tail_without_active_turn() {
    let session = current_session();
    let queued = accepted_origin(1);
    let mut input = queued_input(&session, queued);
    input.active_acceptance_tail = Some(queued.active_tail(&session));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail
    );
}

/// every tail entry belongs to the
/// scheduling session and appears exactly once; a cross-session entry or
/// a repeated accepted-input identity fails closed.
#[test]
fn active_reconstitution_rejects_cross_session_or_repeated_tail_entries() {
    let session = current_session();
    let active = accepted_origin(1);
    let second = accepted_origin(2);

    let other_session = session_id(2);
    let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    cross_session_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .entries[0]
        .session = other_session;
    let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
    assert_eq!(
        cross_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
            accepted_input: active.accepted_input(),
        }
    );

    let mut repeated_facts = ActiveReconstitutionFacts::matching(&session, active);
    let repeated_tail = repeated_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    repeated_tail.observed_last_position = second.position();
    repeated_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                active.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            second.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let repeated = assert_reconstitution_rejects_unchanged(repeated_facts);
    assert_eq!(
        repeated,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
            accepted_input: active.accepted_input(),
        }
    );

    expect![[r#"
            ┌────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact          │ failure                                                                                                      │
            ├────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ tail entry session cross-wired │ AcceptanceTailEntrySessionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) } │
            │ tail entry identity repeated   │ DuplicateAcceptanceTailEntry { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) }       │
            └────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry session cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry identity repeated",
                failure: format!("{repeated:?}"),
            },
        ]));
}

/// every stored snapshot is owned by the
/// scheduling session, unique, duplicate-free, and backed by supplied
/// entries; each malformed snapshot collection fails closed.
#[test]
fn reconstitution_rejects_malformed_snapshot_collection() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();

    let other_session = session_id(2);
    let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    cross_session_facts.snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
        other_session,
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    );
    let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
    assert_eq!(
        cross_session,
        AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
            snapshot: starting_frontier.id(),
        }
    );

    let mut duplicate_facts = ActiveReconstitutionFacts::matching(&session, active);
    duplicate_facts
        .snapshots
        .push(starting_frontier.snapshot(&session, &[origin_entry]));
    let duplicate = assert_reconstitution_rejects_unchanged(duplicate_facts);
    assert_eq!(
        duplicate,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot {
            snapshot: starting_frontier.id(),
        }
    );

    let mut membership_facts = ActiveReconstitutionFacts::matching(&session, active);
    membership_facts.snapshots[0] =
        starting_frontier.snapshot(&session, &[origin_entry, origin_entry]);
    let membership = assert_reconstitution_rejects_unchanged(membership_facts);
    assert_eq!(
        membership,
        AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership {
            snapshot: starting_frontier.id(),
        }
    );

    let absent_entry = semantic_entry(99);
    let mut unbacked_facts = ActiveReconstitutionFacts::matching(&session, active);
    unbacked_facts.snapshots[0] = starting_frontier.snapshot(&session, &[absent_entry]);
    let unbacked = assert_reconstitution_rejects_unchanged(unbacked_facts);
    assert_eq!(
        unbacked,
        AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
            snapshot: starting_frontier.id(),
            entry: absent_entry.reference(&session),
        }
    );

    expect![[r#"
            ┌────────────────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                                                                                                                   │
            ├────────────────────────────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ snapshot owner cross-wired         │ SnapshotOwningSessionMismatch { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                       │
            │ snapshot identity repeated         │ DuplicateSnapshot { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                                   │
            │ snapshot membership entry repeated │ InvalidSnapshotMembership { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                           │
            │ snapshot entry unsupplied          │ SnapshotEntryMissing { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028), entry: SemanticTranscriptEntryRef { source_session: SessionId(00000000-0000-0000-0000-000000000001), entry: SemanticTranscriptEntryId(00000000-0000-0000-0000-000000000063) } } │
            └────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot owner cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot identity repeated",
                failure: format!("{duplicate:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot membership entry repeated",
                failure: format!("{membership:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot entry unsupplied",
                failure: format!("{unbacked:?}"),
            },
        ]));
}

/// a stored start or failed terminal must
/// name a snapshot present in the complete supplied set; an absent
/// snapshot fails closed. Together with the frontier-exactness
/// rejections, this validated precondition backs eligibility's
/// failed-terminal-prefix expectation when preparing a successor.
#[test]
fn reconstitution_rejects_absent_starting_or_terminal_snapshot() {
    let session = current_session();
    let absent_frontier = frontier(99);

    let active = accepted_origin(1);
    let mut starting_facts = ActiveReconstitutionFacts::matching(&session, active);
    starting_facts.replace_starting_frontier(absent_frontier.id());
    let starting = assert_reconstitution_rejects_unchanged(starting_facts);
    assert_eq!(
        starting,
        AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing {
            turn: active.turn(),
        }
    );

    let failed = accepted_origin(1);
    let mut terminal_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    terminal_facts.replace_terminal_frontier(absent_frontier.id());
    let terminal = assert_input_rejects_unchanged(terminal_facts.input());
    assert_eq!(
        terminal,
        AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing {
            turn: failed.turn(),
        }
    );

    expect![[r#"
            ┌─────────────────────────────────┬────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact           │ failure                                                                        │
            ├─────────────────────────────────┼────────────────────────────────────────────────────────────────────────────────┤
            │ stored starting snapshot absent │ StartingSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ stored terminal snapshot absent │ TerminalSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            └─────────────────────────────────┴────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored starting snapshot absent",
                failure: format!("{starting:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored terminal snapshot absent",
                failure: format!("{terminal:?}"),
            },
        ]));
}

/// a supplied snapshot that no stored lifecycle
/// fact references cannot ride along; the complete collection fails
/// closed. This is the read-side rejection recorded for orphan committed
/// snapshot headers.
#[test]
fn reconstitution_rejects_unreferenced_snapshot() {
    let session = current_session();
    let active = accepted_origin(1);
    let stray_frontier = frontier(90);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.snapshots.push(stray_frontier.snapshot(
        &session,
        &[ActiveReconstitutionFacts::matching_origin_entry()],
    ));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot {
            snapshot: stray_frontier.id(),
        }
    );
}

/// durable total order admits only a failed-terminal
/// prefix, at most one active slot, and a queued suffix; every
/// out-of-order stored lifecycle fails closed on the first offending
/// turn.
#[test]
fn reconstitution_rejects_out_of_order_lifecycle_states() {
    let session = current_session();
    let earlier = accepted_origin(1);
    let later = accepted_origin(2);

    let mut active_after_queued_facts = ActiveReconstitutionFacts::matching(&session, later);
    active_after_queued_facts
        .turns
        .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let active_after_queued = assert_reconstitution_rejects_unchanged(active_after_queued_facts);
    assert_eq!(
        active_after_queued,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    let mut terminal_after_queued_facts =
        FailedTerminalReconstitutionFacts::matching(&session, later);
    terminal_after_queued_facts
        .turns
        .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let terminal_after_queued = assert_input_rejects_unchanged(terminal_after_queued_facts.input());
    assert_eq!(
        terminal_after_queued,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    // The ordering check rejects a second active slot before any
    // duplicate current-attempt bookkeeping, which is why the stored
    // attempt identity may repeat here: DuplicateCurrentAttempt is
    // unreachable behind this rejection.
    let mut second_active_facts = ActiveReconstitutionFacts::matching(&session, earlier);
    second_active_facts.turns.push(later.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                later.turn(),
                matching_active_attempt(),
            ),
        },
    ));
    let second_active = assert_reconstitution_rejects_unchanged(second_active_facts);
    assert_eq!(
        second_active,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    // The ordering check rejects the record before consulting its
    // frontier facts, so the claimed snapshots need not be supplied.
    let mut terminal_after_active_facts = ActiveReconstitutionFacts::matching(&session, earlier);
    terminal_after_active_facts.turns.push(later.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: earlier.turn(),
            },
            starting_frontier: frontier(98).id(),
            terminal_execution: None,
            terminal_frontier: frontier(99).id(),
        },
    ));
    let terminal_after_active =
        assert_reconstitution_rejects_unchanged(terminal_after_active_facts);
    assert_eq!(
        terminal_after_active,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    expect![[r#"
            ┌───────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                 │ failure                                                                      │
            ├───────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────┤
            │ active slot after queued work         │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after queued work     │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ second active slot                    │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after the active slot │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            └───────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active slot after queued work",
                failure: format!("{active_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after queued work",
                failure: format!("{terminal_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "second active slot",
                failure: format!("{second_active:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after the active slot",
                failure: format!("{terminal_after_active:?}"),
            },
        ]));
}

/// the stored starting lineage must equal the
/// lineage derived from durable total order; a first-in-session active
/// turn cannot claim a predecessor.
#[test]
fn reconstitution_rejects_stored_lineage_disagreeing_with_order() {
    let session = current_session();
    let active = accepted_origin(1);
    let claimed_lineage = AcceptedInputStartingLineage::After {
        immediate_predecessor: turn_id(99),
    };
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_starting_lineage(claimed_lineage);

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
            turn: active.turn(),
            expected: AcceptedInputStartingLineage::FirstInSession,
            actual: claimed_lineage,
        }
    );
}

/// attachment origins hidden by completed context
/// compaction do not contribute to the rendered frontier bound.
#[test]
fn rendered_frontier_origins_exclude_compacted_input() {
    let session = current_session();
    let hidden_input = accepted_input_id(1);
    let hidden_origin = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(1),
        session.id(),
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: hidden_input,
        },
    );
    let terminal = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(2),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted { turn: turn_id(1) },
    );
    let summary = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(3),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: model_call_id(4),
            summarized: crate::ContextCompactionRange::inclusive(
                hidden_origin.reference(),
                terminal.reference(),
            ),
            value: AssistantText::try_new(String::from("summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(5),
        vec![
            hidden_origin.reference(),
            terminal.reference(),
            summary.reference(),
        ],
    )
    .expect("the complete frontier retains compacted entries");
    let semantic_entries = BTreeMap::from([
        (hidden_origin.reference(), hidden_origin),
        (terminal.reference(), terminal),
        (summary.reference(), summary),
    ]);

    assert_eq!(
        AcceptedInputSchedulingProjection::rendered_frontier_origins(
            Some(&snapshot),
            &semantic_entries,
        ),
        Some(Vec::new())
    );
}

/// the stored starting snapshot must be exactly
/// the predecessor prefix plus the turn's origin entry; a snapshot
/// omitting the origin fails closed.
#[test]
fn reconstitution_rejects_starting_snapshot_omitting_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.snapshots =
        vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );
}

/// after a completed compaction, the exact compacted
/// result followed by the next turn's origin is a valid starting
/// frontier even though the predecessor frontier remains complete.
#[test]
fn reconstitution_accepts_exact_compaction_result_then_origin() {
    let session = current_session();
    let predecessor_turn = turn_id(1);
    let active_turn = turn_id(2);
    let predecessor_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(1),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted {
            turn: predecessor_turn,
        },
    );
    let origin_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(3),
        session.id(),
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: accepted_input_id(2),
        },
    );
    let range = crate::ContextCompactionRange::inclusive(
        predecessor_entry.reference(),
        predecessor_entry.reference(),
    );
    let compaction_call = model_call_id(4);
    let summary_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(5),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: compaction_call,
            summarized: range,
            value: AssistantText::try_new(String::from("summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(6),
        vec![predecessor_entry.reference()],
    )
    .expect("the predecessor fixture is a unique complete frontier");
    let compacted_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(7),
        vec![predecessor_entry.reference(), summary_entry.reference()],
    )
    .expect("the compaction result appends the summary");
    let starting_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(8),
        vec![
            predecessor_entry.reference(),
            summary_entry.reference(),
            origin_entry.reference(),
        ],
    )
    .expect("the next start appends its origin to the compaction result");
    let call = crate::ContextCompactionModelCallReconstitutionInput::new(
        compaction_call,
        session.id(),
        direct(9),
        ResolvedProviderTarget::naming(provider_model_identity(10)),
        predecessor_snapshot.frontier().snapshot(),
        crate::ContextCompactionModelCallState::Terminal(crate::ModelCallDisposition::Completed),
        crate::ContextCompactionTokenUsage::unreported(),
    )
    .reconstitute(&predecessor_snapshot)
    .expect("the dedicated call exactly names the predecessor frontier");
    let compaction = crate::ContextCompactionReconstitutionInput::new(
        crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(11)),
        session.id(),
        None,
        predecessor_snapshot.frontier().snapshot(),
        compacted_snapshot.frontier().snapshot(),
        compaction_call,
        range,
        summary_entry.identity(),
    )
    .reconstitute(
        &predecessor_snapshot,
        &compacted_snapshot,
        std::slice::from_ref(&predecessor_entry),
        &[predecessor_entry.clone(), summary_entry.clone()],
        &summary_entry,
        &call,
    )
    .expect("the exact compaction facts reconstruct");
    let mut compactions = BTreeMap::from([(compaction.id(), compaction)]);
    let mut snapshots = BTreeMap::from([
        (
            predecessor_snapshot.frontier().snapshot(),
            predecessor_snapshot.clone(),
        ),
        (
            compacted_snapshot.frontier().snapshot(),
            compacted_snapshot.clone(),
        ),
        (
            starting_snapshot.frontier().snapshot(),
            starting_snapshot.clone(),
        ),
    ]);
    let origins = BTreeMap::from([(active_turn, origin_entry.reference())]);
    let mut referenced_snapshots = BTreeSet::new();
    let compaction_chain = compactions.values().collect::<Vec<_>>();

    let start = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        starting_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot.clone())),
        &origins,
        None,
        &compaction_chain,
        &[],
        &snapshots,
        &mut referenced_snapshots,
    )
    .expect("the validated compacted frontier remains an exact start");

    assert_eq!(start.frontier(), starting_snapshot.frontier());

    let successor_range = crate::ContextCompactionRange::inclusive(
        summary_entry.reference(),
        origin_entry.reference(),
    );
    let successor_call = model_call_id(13);
    let successor_summary = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(14),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: successor_call,
            summarized: successor_range,
            value: AssistantText::try_new(String::from("newer summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let successor_result = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(15),
        vec![
            predecessor_entry.reference(),
            summary_entry.reference(),
            origin_entry.reference(),
            successor_summary.reference(),
        ],
    )
    .expect("the successor compaction appends its summary");
    let successor_call_record = crate::ContextCompactionModelCallReconstitutionInput::new(
        successor_call,
        session.id(),
        direct(9),
        ResolvedProviderTarget::naming(provider_model_identity(10)),
        starting_snapshot.frontier().snapshot(),
        crate::ContextCompactionModelCallState::Terminal(crate::ModelCallDisposition::Completed),
        crate::ContextCompactionTokenUsage::unreported(),
    )
    .reconstitute(&starting_snapshot)
    .expect("the successor call names its exact source");
    let predecessor_compaction = compactions
        .values()
        .next()
        .expect("the first compaction is present");
    let successor_compaction = crate::ContextCompactionReconstitutionInput::new(
        crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(16)),
        session.id(),
        Some(predecessor_compaction.id()),
        starting_snapshot.frontier().snapshot(),
        successor_result.frontier().snapshot(),
        successor_call,
        successor_range,
        successor_summary.identity(),
    )
    .reconstitute(
        &starting_snapshot,
        &successor_result,
        &[
            predecessor_entry.clone(),
            summary_entry.clone(),
            origin_entry.clone(),
        ],
        &[
            predecessor_entry.clone(),
            summary_entry.clone(),
            origin_entry.clone(),
            successor_summary.clone(),
        ],
        &successor_summary,
        &successor_call_record,
    )
    .expect("the successor compaction reconstructs");
    compactions.insert(successor_compaction.id(), successor_compaction);
    snapshots.insert(successor_result.frontier().snapshot(), successor_result);
    let compaction_chain = [
        compactions
            .values()
            .find(|compaction| compaction.predecessor().is_none())
            .expect("the root compaction is present"),
        compactions
            .values()
            .find(|compaction| compaction.predecessor().is_some())
            .expect("the successor compaction is present"),
    ];
    let mut historical_references = BTreeSet::new();

    let historical_start = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        starting_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot.clone())),
        &origins,
        None,
        &compaction_chain,
        &[],
        &snapshots,
        &mut historical_references,
    )
    .expect("the intervening historical start retains the earlier summary");

    assert_eq!(historical_start.frontier(), starting_snapshot.frontier());

    let stale_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(12),
        vec![predecessor_entry.reference(), origin_entry.reference()],
    )
    .expect("the stale fixture omits only the required summary append");
    snapshots.insert(stale_snapshot.frontier().snapshot(), stale_snapshot.clone());
    let mut stale_references = BTreeSet::new();

    let stale_failure = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        stale_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot)),
        &origins,
        None,
        &compaction_chain,
        &[],
        &snapshots,
        &mut stale_references,
    )
    .expect_err("a post-compaction start cannot omit the summary result");

    assert_eq!(
        stale_failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active_turn,
        }
    );
}

/// each start owns a distinct snapshot; a
/// successor start naming its predecessor's already-referenced starting
/// snapshot fails closed. With the content-exactness rejection, this
/// backs eligibility's expectation that fresh snapshot identities
/// preserve the validated prefix.
#[test]
fn reconstitution_rejects_starting_frontier_reused_from_predecessor() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let active_origin_entry = semantic_entry(32);
    let active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: predecessor.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, predecessor);
    facts.turns.push(active.record_with(
        &session,
        OriginRecordFacts {
            order: active.ordinary_order(),
            delivery: active_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: predecessor.turn(),
                },
                // The perturbation: the successor claims its
                // predecessor's starting snapshot instead of a distinct
                // successor prefix snapshot.
                starting_frontier:
                    FailedTerminalReconstitutionFacts::matching_starting_frontier().id(),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    active.turn(),
                    matching_active_attempt(),
                ),
            },
        },
    ));
    facts
        .semantic_entries
        .push(active.entry(&session, active_origin_entry));
    facts.acceptance_tail = Some(SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        active.position(),
        vec![SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                active.accepted_input(),
                AcceptedInputDisposition::OriginOf(active.turn()),
            ),
            active.position(),
            active_delivery,
        )],
    ));

    let failure = assert_input_rejects_unchanged(facts.input());

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );
}
