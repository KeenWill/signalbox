//! Turn scheduling origin and delivery tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, FailedPredecessorPostAnchorOrigins, OriginRecordFacts,
    PostAnchorOrigins, accepted_origin, active_input,
    active_input_after_failed_predecessor_with_post_anchor_origin,
    active_input_after_historical_interrupt, active_input_with_post_anchor_origin,
    assert_input_rejects_unchanged, assert_reconstitution_rejects_unchanged, current_session,
    matching_active_attempt,
};
use super::*;

/// S03 / S09: a scheduler-gap start remains
/// a valid ordinary origin after an earlier queued turn becomes active.
#[test]
fn active_reconstitution_preserves_post_anchor_scheduler_gap_start() {
    let session = current_session();
    let origins = FailedPredecessorPostAnchorOrigins {
        predecessor: accepted_origin(1),
        active: accepted_origin(2),
        queued: accepted_origin(3),
    };
    active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect("the later origin was accepted during a valid scheduler gap");
}

/// S03 / S09: an ordinary queued origin
/// retains the historical active target named at acceptance.
#[test]
fn active_reconstitution_preserves_post_anchor_historical_target() {
    let session = current_session();
    let origins = FailedPredecessorPostAnchorOrigins {
        predecessor: accepted_origin(1),
        active: accepted_origin(2),
        queued: accepted_origin(3),
    };
    active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: origins.predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect("the later origin retains its exact previously active target");
}

/// S03 / S09: after-current delivery must
/// name an earlier nonqueued target in the complete turn inventory.
#[test]
fn active_reconstitution_rejects_missing_historical_delivery_target() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let missing_target_turn = turn_id(99);
    let missing_target = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: missing_target_turn,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect_err("after-current delivery requires its historical target record");
    assert_eq!(
        missing_target.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: origins.queued.turn(),
        }
    );
}

/// S03 / S07: an interrupt delivery must
/// agree with the origin record's durable interrupt-priority relation.
#[test]
fn active_reconstitution_rejects_delivery_priority_mismatch() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let wrong_priority = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::Interrupt {
            expected_active_turn: origins.active.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect_err("interrupt delivery cannot carry ordinary queue priority");
    assert_eq!(
        wrong_priority.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: origins.queued.turn(),
        }
    );
}

/// S01: origin delivery and queue facts
/// are validated even when no active turn requires an acceptance tail.
#[test]
fn s01_queued_reconstitution_rejects_delivery_order_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(99),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("steering-only delivery cannot reconstruct queued turn work");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// S01: a configured origin's
/// accepted defaults version must equal its frozen provenance version.
#[test]
fn s01_queued_origin_rejects_defaults_version_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let mismatched_version = SessionConfigurationDefaultsVersion::try_from_u64(2)
        .expect("the mismatched test version is positive");
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        mismatched_version,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("accepted delivery and frozen provenance versions must agree");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// S01: an explicit accepted
/// model request must equal the request retained by frozen provenance.
#[test]
fn s01_queued_origin_rejects_explicit_request_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let requested = ModelSelectionRequest::Direct(direct(99));
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::ReplaceWith(requested),
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("explicit delivery request and frozen provenance must agree");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// S03: the tail repeats the exact
/// immutable versioned delivery stored for its origin rather than
/// supplying an independently plausible configuration choice.
#[test]
fn active_reconstitution_rejects_origin_delivery_configuration_mismatch() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the active tail")
        .entries[0]
        .delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::try_from_u64(2)
                .expect("the mismatched test version is positive"),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };

    let error = assert_reconstitution_rejects_unchanged(facts);
    assert_eq!(
        error,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: active.accepted_input(),
        }
    );
}

/// S03 / S07: an accepted interrupt
/// against the current owner prevents evidence-free phase reconstruction.
#[test]
fn active_reconstitution_rejects_interrupt_evidence_for_evidence_free_phase() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let delivery = DeliveryRequest::Interrupt {
        expected_active_turn: origins.active.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut input = active_input_with_post_anchor_origin(&session, origins, delivery);
    input.turns[1] = origins.queued.record_with(
        &session,
        OriginRecordFacts {
            order: AcceptedInputQueueOrder::interrupt_immediately_after(
                origins.queued.position(),
                origins.active.turn(),
            ),
            delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );

    let error = input
        .reconstitute()
        .expect_err("applied interrupt evidence requires a proof-bearing phase projection");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
            turn: origins.active.turn(),
            accepted_input: origins.queued.accepted_input(),
        }
    );
}

/// S03 / S07: a historical interrupt in the active
/// acceptance tail retains the target terminal's exact stop proof.
#[test]
fn s03_s07_historical_interrupt_requires_target_stop_proof() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let interrupt_successor = accepted_origin(3);
    let matching =
        active_input_after_historical_interrupt(&session, predecessor, active, interrupt_successor);
    matching
        .clone()
        .reconstitute()
        .expect("the exact historical interrupt proof remains admissible");

    let mut missing_proof = matching;
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution, ..
    } = &mut missing_proof.turns[0].state
    else {
        panic!("the historical target fixture is terminal failed");
    };
    *terminal_execution = None;
    assert_eq!(
        assert_input_rejects_unchanged(missing_proof),
        AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
            turn: active.turn(),
            accepted_input: interrupt_successor.accepted_input(),
        }
    );
}

/// S03 / S08: one accepted input cannot
/// be both pending steering and a turn origin in the scheduling inventory.
#[test]
fn active_reconstitution_rejects_pending_identity_that_is_also_an_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));

    active_input(&session, active, Some(tail.clone()))
        .reconstitute()
        .expect("pending steering remains distinct from every origin");

    let mut aliased = active_input(&session, active, Some(tail));
    aliased
        .turns
        .push(pending.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let aliased = aliased
        .reconstitute()
        .expect_err("pending steering cannot reuse a turn-origin identity");
    assert_eq!(
        aliased.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: pending.accepted_input(),
        }
    );
}

/// S02 / S08: a prepared call consumes the complete
/// pending prefix; durable history cannot claim that it skipped an earlier
/// pending input and consumed a later one.
#[test]
fn s02_s08_active_tail_rejects_consumed_after_pending() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let consumed = accepted_origin(3);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = consumed.position();
    tail.entries.extend([
        SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ),
        SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: model_call_id(91),
                },
            ),
            consumed.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ),
    ]);

    let error = active_input(&session, active, Some(tail))
        .reconstitute()
        .expect_err("a later consumed receipt cannot skip earlier pending steering");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// S03 / S08: a pending tail entry cannot
/// replace a different origin that owns the same acceptance position.
#[test]
fn active_reconstitution_rejects_pending_position_owned_by_an_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin = accepted_origin(2);
    let pending = accepted_origin(3);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = origin.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            origin.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let mut input = active_input(&session, active, Some(tail));
    input
        .turns
        .push(origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));

    let error = input
        .reconstitute()
        .expect_err("the complete tail cannot replace an origin at the same position");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: pending.accepted_input(),
        }
    );
}

/// S03: the last represented position must equal
/// the authoritative session tail observed by the same read.
#[test]
fn active_reconstitution_rejects_incomplete_claimed_acceptance_tail() {
    let session = current_session();
    let active = accepted_origin(1);
    let next = accepted_origin(2);
    let mut incomplete = active.active_tail(&session);
    incomplete.observed_last_position = next.position();
    let incomplete = active_input(&session, active, Some(incomplete))
        .reconstitute()
        .expect_err("the represented interval must reach the claimed session tail");
    assert_eq!(
        incomplete.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
            expected: next.position(),
            actual: Some(active.position()),
        }
    );
}

/// S03: the claimed session observation
/// cannot end before a later origin supplied by the same scheduling read.
#[test]
fn s03_active_tail_reaches_every_known_origin() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let mut input = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let tail = input
        .active_acceptance_tail
        .as_mut()
        .expect("the helper supplies an active tail");
    tail.observed_last_position = origins.active.position();
    tail.entries.truncate(1);

    let error = input
        .reconstitute()
        .expect_err("a known later origin disproves the claimed tail observation");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
            expected: origins.active.position(),
            actual: Some(origins.queued.position()),
        }
    );
}

/// S03: a current attempt owned by another turn cannot
/// reconstruct an active aggregate.
#[test]
fn s03_active_reconstitution_rejects_cross_wired_attempt_owner() {
    let session = current_session();
    let active = accepted_origin(1);
    let other_turn = turn_id(99);
    let attempt = matching_active_attempt();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::prepared(
        other_turn, attempt,
    ));
    let error = assert_reconstitution_rejects_unchanged(facts);
    assert_eq!(
        error,
        AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
            turn: active.turn(),
            attempt,
        }
    );
}
