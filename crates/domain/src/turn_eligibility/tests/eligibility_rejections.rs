//! Turn scheduling eligibility rejections tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ConsumedSteeringReconstitutionFacts, FailedTerminalReconstitutionFacts, accepted_origin,
    activation, assert_eligibility_rejects_unchanged, assert_input_rejects_unchanged,
    current_session,
};
use super::*;

/// an all-terminal projection holds no queued work;
/// eligibility rejects instead of manufacturing a candidate.
#[test]
fn eligibility_rejects_projection_without_queued_work() {
    let session = current_session();
    let failed = accepted_origin(1);
    let activation = activation(1);
    let projection = FailedTerminalReconstitutionFacts::matching(&session, failed)
        .input()
        .reconstitute()
        .expect("the complete failed-terminal record is valid");

    let failure = assert_eligibility_rejects_unchanged(projection, activation.identities());

    assert_eq!(failure, AcceptedInputEligibilityFailure::NoQueuedTurn);
}

/// a proposed origin-entry identity colliding with
/// a committed semantic entry fails closed before any candidate is
/// prepared.
#[test]
fn eligibility_rejects_committed_origin_entry_identity() {
    let session = current_session();
    let failed = accepted_origin(1);
    let queued = accepted_origin(2);
    let activation = activation(1);
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts
        .turns
        .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let projection = facts
        .input()
        .reconstitute()
        .expect("a failed-terminal prefix with one queued successor is valid");
    let committed_origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();

    let failure = assert_eligibility_rejects_unchanged(
        projection,
        activation.identities_with_origin_entry(committed_origin_entry.id()),
    );

    assert_eq!(
        failure,
        AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists
    );
}

/// a proposed starting-snapshot identity
/// colliding with a committed session-scoped snapshot fails closed
/// before any candidate is prepared.
#[test]
fn eligibility_rejects_committed_starting_frontier_identity() {
    let session = current_session();
    let failed = accepted_origin(1);
    let queued = accepted_origin(2);
    let activation = activation(1);
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts
        .turns
        .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let projection = facts
        .input()
        .reconstitute()
        .expect("a failed-terminal prefix with one queued successor is valid");
    let committed_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();

    let failure = assert_eligibility_rejects_unchanged(
        projection,
        activation.identities_with_starting_frontier(committed_frontier.id()),
    );

    assert_eq!(
        failure,
        AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists
    );
}

/// a prepared standalone compaction call survives complete
/// reconstitution and prevents queued-turn activation until recovery.
#[test]
fn prepared_compaction_call_blocks_activation_after_reconstitution() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(701),
        Vec::new(),
    );
    let call = model_call_id(702);
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(
        vec![crate::ContextCompactionModelCallReconstitutionInput::new(
            call,
            session.id(),
            direct(703),
            ResolvedProviderTarget::naming(provider_model_identity(704)),
            context_frontier_id(701),
            crate::ContextCompactionModelCallState::Prepared,
            crate::ContextCompactionTokenUsage::unreported(),
        )],
        Vec::new(),
    );
    let projection = input
        .reconstitute()
        .expect("prepared compaction evidence remains recoverable");
    let error = projection
        .prepare_earliest_queued_activation(activation(705).identities())
        .expect_err("unfinished compaction owns the execution slot");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
    );
}

/// an authorized standalone compaction call remains
/// recoverable and owns the execution slot after restart reconstitution.
#[test]
fn in_flight_compaction_call_blocks_activation_after_reconstitution() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(706),
        Vec::new(),
    );
    let call = model_call_id(707);
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(
        vec![crate::ContextCompactionModelCallReconstitutionInput::new(
            call,
            session.id(),
            direct(708),
            ResolvedProviderTarget::naming(provider_model_identity(709)),
            context_frontier_id(706),
            crate::ContextCompactionModelCallState::InFlight,
            crate::ContextCompactionTokenUsage::unreported(),
        )],
        Vec::new(),
    );
    let projection = input
        .reconstitute()
        .expect("in-flight compaction evidence remains recoverable");
    let error = projection
        .prepare_earliest_queued_activation(activation(710).identities())
        .expect_err("authorized compaction owns the execution slot");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
    );
}

/// a terminal non-completed dedicated call is retained as
/// historical recovery evidence without requiring a compaction result.
#[test]
fn known_failed_compaction_call_is_legal_standalone_evidence() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(711),
        Vec::new(),
    );
    let call = crate::ContextCompactionModelCallReconstitutionInput::new(
        model_call_id(712),
        session.id(),
        direct(713),
        ResolvedProviderTarget::naming(provider_model_identity(714)),
        context_frontier_id(711),
        crate::ContextCompactionModelCallState::Terminal(ModelCallDisposition::KnownFailed),
        crate::ContextCompactionTokenUsage::unreported(),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(vec![call], Vec::new());

    let projection = input
        .reconstitute()
        .expect("known-failed compaction evidence is complete without a summary");
    let error = projection
        .prepare_earliest_queued_activation(activation(715).identities())
        .expect_err("the fixture contains no queued turn");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::NoQueuedTurn
    );
}

/// ordinary and compaction call maps cannot claim the
/// same identity even when both purpose-specific records are valid alone.
#[test]
fn reconstitution_rejects_cross_kind_model_call_identity() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let call = facts.model_calls[0].id();
    let source = facts.model_calls[0].frontier();
    let collision = crate::ContextCompactionModelCallReconstitutionInput::new(
        call,
        session.id(),
        direct(1),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        source,
        crate::ContextCompactionModelCallState::Prepared,
        crate::ContextCompactionTokenUsage::unreported(),
    );
    let input = facts
        .input()
        .with_context_compaction_facts(vec![collision], Vec::new());

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCallIdentityAcrossKinds {
            call,
        }
    );
}
