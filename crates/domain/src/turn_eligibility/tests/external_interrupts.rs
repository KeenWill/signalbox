//! Turn scheduling external interrupts tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::*;

/// checked relational runner-loss facts reconstitute the
/// exact closed active phase without a live turn attempt.
#[test]
fn runner_recovery_phase_reconstitutes_exact_loss_subject() {
    let owning_turn = turn_id(801);
    let runner = crate::RunnerId::from_uuid(uuid::Uuid::from_u128(802));
    let revision = crate::RunnerGeneration::try_from_u64(3)
        .expect("the fixture placement revision is positive");
    let interrupted_tool_attempt = Some(tool_attempt_id(803));
    let input = ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
        owning_turn,
        runner,
        revision,
        interrupted_tool_attempt,
        None,
    );

    assert_eq!(
        input.canonical_evidence_free_phase(),
        Some(ActiveTurnPhase::AwaitingRunnerRecovery {
            runner,
            placement_revision: revision,
            optional_tool_attempt: interrupted_tool_attempt,
        })
    );
}

/// an interrupt successor authenticated against an external
/// terminal predecessor remains ahead of older ordinary queued work.
#[test]
fn external_interrupt_chain_is_the_first_accepted_order_root() {
    let older_ordinary = turn_id(811);
    let external_successor = turn_id(812);
    let interrupt_descendant = turn_id(813);
    let later_ordinary = turn_id(814);
    let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
    let queued_turns = BTreeSet::from([
        older_ordinary,
        external_successor,
        interrupt_descendant,
        later_ordinary,
    ]);

    let promoted = super::promote_external_interrupt_chains(
        vec![
            older_ordinary,
            external_successor,
            interrupt_descendant,
            later_ordinary,
        ],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![
            external_successor,
            interrupt_descendant,
            older_ordinary,
            later_ordinary,
        ]
    );
}

/// later external chains retain their historical placement once
/// the oldest crossing chain is promoted ahead of queued work.
#[test]
fn multiple_external_interrupt_chains_are_retained_in_order() {
    let older_ordinary = turn_id(821);
    let first_external_successor = turn_id(822);
    let first_descendant = turn_id(823);
    let second_external_successor = turn_id(824);
    let second_descendant = turn_id(825);
    let later_ordinary = turn_id(826);
    let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
    let queued_turns = BTreeSet::from([
        older_ordinary,
        first_external_successor,
        first_descendant,
        second_external_successor,
        second_descendant,
        later_ordinary,
    ]);

    let promoted = super::promote_external_interrupt_chains(
        vec![
            older_ordinary,
            first_external_successor,
            first_descendant,
            second_external_successor,
            second_descendant,
            later_ordinary,
        ],
        BTreeSet::from([first_external_successor, second_external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![
            first_external_successor,
            first_descendant,
            older_ordinary,
            second_external_successor,
            second_descendant,
            later_ordinary,
        ]
    );
}

/// an external interrupt chain does not cross a completed
/// accepted-input terminal prefix.
#[test]
fn external_interrupt_chain_retains_terminal_prefix() {
    let terminal = turn_id(831);
    let external_successor = turn_id(832);
    let ordinary_roots = BTreeSet::from([terminal]);

    let promoted = super::promote_external_interrupt_chains(
        vec![terminal, external_successor],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &BTreeSet::from([external_successor]),
    );

    assert_eq!(promoted, vec![terminal, external_successor]);
}

/// an external interrupt chain crosses queued ordinary work but
/// retains the completed accepted-input terminal prefix.
#[test]
fn external_interrupt_chain_precedes_only_queued_prefix() {
    let terminal = turn_id(841);
    let older_queued = turn_id(842);
    let external_successor = turn_id(843);
    let ordinary_roots = BTreeSet::from([terminal, older_queued]);
    let queued_turns = BTreeSet::from([older_queued, external_successor]);

    let promoted = super::promote_external_interrupt_chains(
        vec![terminal, older_queued, external_successor],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(promoted, vec![terminal, external_successor, older_queued]);
}

/// a historical external terminal does not hide the later
/// external interrupt chain that actually crosses queued ordinary work.
#[test]
fn later_external_interrupt_chain_crosses_queued_work() {
    let historical_external = turn_id(851);
    let older_queued = turn_id(852);
    let crossing_external = turn_id(853);
    let ordinary_roots = BTreeSet::from([older_queued]);
    let queued_turns = BTreeSet::from([older_queued, crossing_external]);

    let promoted = super::promote_external_interrupt_chains(
        vec![older_queued, historical_external, crossing_external],
        BTreeSet::from([historical_external, crossing_external]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![historical_external, crossing_external, older_queued]
    );
}
