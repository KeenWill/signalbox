//! Durable accepted-input queue-order facts and pure order derivation.
//!
//! ADR-0027 (`docs/decisions/0027-input-delivery-lifecycle.md`) is the
//! normative specification. Accepted origin work records one immutable
//! acceptance position and either ordinary priority or the typed interrupt
//! relation that places it immediately after an exact predecessor. Deriving
//! the total order requires the complete currently known fact set for one
//! session; no individual queue-order value can name a starting predecessor.

use std::collections::{BTreeMap, BTreeSet};

use crate::{SessionId, TurnId};

/// One input's immutable position in its session's acceptance order.
///
/// This private ordinal representation is a domain value, not a storage or
/// wire encoding. Session scope is supplied by the aggregate that owns the
/// queue facts rather than repeated inside each position.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionInputPosition(u64);

impl SessionInputPosition {
    /// Reconstitutes a position from its positive ordinal value.
    ///
    /// Returns `None` for zero, which is not an acceptance position. Storage
    /// and protocol boundaries remain responsible for decoding their own
    /// representations into a `u64` before calling this domain-owned check.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns this position's positive ordinal value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the first position in a session's acceptance order.
    pub const fn first() -> Self {
        Self(1)
    }

    /// Returns the next position, or `None` when the ordinal is exhausted.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// The typed priority fact attached to accepted origin work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AcceptedInputQueuePriority {
    /// Order this work with other ordinary work by acceptance position.
    Ordinary,
    /// Place this work immediately after the exact interrupted turn.
    ///
    /// This is an interrupt-priority relation, not the turn's eventual
    /// starting-lineage predecessor.
    InterruptImmediatelyAfter {
        /// The active turn interrupted when this work was accepted.
        predecessor: TurnId,
    },
}

/// The immutable queue-order facts for one accepted-input-origin turn.
///
/// Ordinary and reclassified-steering work carry only their original
/// acceptance position. Interrupt work additionally carries its typed
/// priority relation. Neither form can carry a direct starting predecessor:
///
/// INV-009 construction proof:
///
/// ```compile_fail
/// use signalbox_domain::{AcceptedInputQueueOrder, SessionInputPosition, TurnId};
///
/// fn ordinary_work_cannot_freeze_a_predecessor(predecessor: TurnId) {
///     let _ = AcceptedInputQueueOrder::ordinary(
///         SessionInputPosition::first(),
///         predecessor,
///     );
/// }
/// ```
///
/// Starting lineage is fixed only by the later eligibility transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AcceptedInputQueueOrder {
    acceptance_position: SessionInputPosition,
    priority: AcceptedInputQueuePriority,
}

impl AcceptedInputQueueOrder {
    /// Creates ordinary queue-order facts at the input's original position.
    pub const fn ordinary(acceptance_position: SessionInputPosition) -> Self {
        Self {
            acceptance_position,
            priority: AcceptedInputQueuePriority::Ordinary,
        }
    }

    /// Creates interrupt-priority facts at the input's original position.
    pub const fn interrupt_immediately_after(
        acceptance_position: SessionInputPosition,
        predecessor: TurnId,
    ) -> Self {
        Self {
            acceptance_position,
            priority: AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor },
        }
    }

    /// Returns the input's immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the ordinary or interrupt priority fact.
    pub const fn priority(&self) -> AcceptedInputQueuePriority {
        self.priority
    }
}

/// One session-scoped turn and its immutable queue-order facts.
///
/// This is a pure domain projection used to derive order, not a persistence
/// record. Repeating the session association at this boundary lets derivation
/// reject a mixed-session fact collection without adding `SessionId` to the
/// normative [`AcceptedInputQueueOrder`] value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AcceptedInputQueueWork {
    session: SessionId,
    turn: TurnId,
    order: AcceptedInputQueueOrder,
}

impl AcceptedInputQueueWork {
    /// Associates one accepted-input-origin turn and its order facts with the
    /// session whose complete currently known work is being derived.
    pub const fn new(session: SessionId, turn: TurnId, order: AcceptedInputQueueOrder) -> Self {
        Self {
            session,
            turn,
            order,
        }
    }

    /// Returns the associated session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the ordered turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the turn's immutable queue-order facts.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }
}

/// Reports why currently known queue facts cannot form the accepted total
/// order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputQueueOrderError {
    /// The derivation input mixed work associated with different sessions.
    MixedSessions {
        /// The lower canonical session identity present in the fact set.
        first_session: SessionId,
        /// The next canonical session identity present in the fact set.
        second_session: SessionId,
    },
    /// The same turn appeared more than once in the complete fact set.
    DuplicateTurn {
        /// The duplicated turn.
        turn: TurnId,
    },
    /// Two turns claimed the same immutable acceptance position.
    DuplicateAcceptancePosition {
        /// The duplicated position.
        position: SessionInputPosition,
        /// The lower canonical turn identity claiming the position.
        first_turn: TurnId,
        /// The higher canonical turn identity claiming the position.
        second_turn: TurnId,
    },
    /// An interrupt relation named work absent from the complete fact set.
    MissingInterruptPredecessor {
        /// The interrupt-origin turn.
        turn: TurnId,
        /// The absent predecessor.
        predecessor: TurnId,
    },
    /// An interrupt relation named its own turn as predecessor.
    SelfInterruptPredecessor {
        /// The self-referencing turn.
        turn: TurnId,
    },
    /// More than one interrupt claimed to be one turn's immediate successor.
    MultipleInterruptSuccessors {
        /// The multiply interrupted predecessor.
        predecessor: TurnId,
        /// The lower canonical successor identity.
        first_successor: TurnId,
        /// The higher canonical successor identity.
        second_successor: TurnId,
    },
    /// Interrupt relations formed a cycle with no ordinary root.
    InterruptCycle {
        /// The lowest canonical turn identity in the unrooted cycle.
        turn: TurnId,
    },
    /// An interrupt claimed an acceptance position no later than its target.
    ///
    /// This check is an interpretation, not a quoted rule: ADR-0027 accepts
    /// the active-work modes "only when `expected_active_turn` is the
    /// session's current active turn", and a turn that is already active has
    /// an origin input accepted at an earlier position, so interrupt facts
    /// violating this chronology cannot have been produced by valid
    /// acceptance.
    InterruptPositionNotAfterPredecessor {
        /// The interrupt-origin turn.
        turn: TurnId,
        /// The exact interrupted turn.
        predecessor: TurnId,
        /// The interrupt input's acceptance position.
        position: SessionInputPosition,
        /// The predecessor input's earlier acceptance position.
        predecessor_position: SessionInputPosition,
    },
    /// Later-accepted interrupt work targeted an earlier active predecessor
    /// than a previously accepted interrupt had already reached.
    ///
    /// This check is an interpretation, not a quoted rule: it formalizes
    /// ADR-0027's "a later request must target the new authoritative active
    /// state" as monotonic advancement of interrupt targets through the
    /// derived total order.
    InterruptPredecessorChronologyReversed {
        /// The earlier-accepted interrupt-origin turn.
        earlier_interrupt: TurnId,
        /// The predecessor targeted by the earlier interrupt.
        earlier_predecessor: TurnId,
        /// The later-accepted interrupt-origin turn.
        later_interrupt: TurnId,
        /// The predecessor improperly targeted by the later interrupt.
        later_predecessor: TurnId,
    },
}

/// Derives the durable total order of currently known accepted-input work.
///
/// The input must be the complete currently known fact set. Derivation first
/// validates that every item names the same session. Ordinary roots are
/// ordered by acceptance position; each root is followed by its unique
/// recursive `InterruptImmediatelyAfter` successor chain before the next
/// ordinary root. Interrupt targets must advance monotonically through that
/// derived order as later inputs are accepted, so a later interrupt cannot
/// target a turn that an earlier target's activation already required to be
/// terminal; this monotonicity and the position-chronology guard are
/// interpretations documented on their
/// [`AcceptedInputQueueOrderError`] variants. The returned turn identities
/// are derived order only: no direct-predecessor pointer is written back
/// into queued work.
pub fn derive_accepted_input_total_order(
    currently_known_work: impl IntoIterator<Item = AcceptedInputQueueWork>,
) -> Result<Vec<TurnId>, AcceptedInputQueueOrderError> {
    let mut currently_known_work: Vec<_> = currently_known_work.into_iter().collect();
    let sessions: BTreeSet<_> = currently_known_work
        .iter()
        .map(AcceptedInputQueueWork::session)
        .collect();
    let mut sessions = sessions.into_iter();
    if let Some(first_session) = sessions.next()
        && let Some(second_session) = sessions.next()
    {
        return Err(AcceptedInputQueueOrderError::MixedSessions {
            first_session,
            second_session,
        });
    }

    currently_known_work.sort_unstable_by_key(AcceptedInputQueueWork::turn);
    if let Some(duplicates) = currently_known_work
        .windows(2)
        .find(|pair| pair[0].turn() == pair[1].turn())
    {
        return Err(AcceptedInputQueueOrderError::DuplicateTurn {
            turn: duplicates[0].turn(),
        });
    }
    let mut work_by_turn = BTreeMap::new();
    let mut turn_by_position = BTreeMap::new();

    for work in currently_known_work {
        let turn = work.turn();
        let order = work.order();
        if let Some(existing) = turn_by_position.get(&order.acceptance_position()) {
            let (first_turn, second_turn) = canonical_pair(*existing, turn);
            return Err(AcceptedInputQueueOrderError::DuplicateAcceptancePosition {
                position: order.acceptance_position(),
                first_turn,
                second_turn,
            });
        }

        turn_by_position.insert(order.acceptance_position(), turn);
        work_by_turn.insert(turn, order);
    }

    let mut successor_by_predecessor = BTreeMap::new();
    for (turn, order) in &work_by_turn {
        let AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } =
            order.priority()
        else {
            continue;
        };

        if predecessor == *turn {
            return Err(AcceptedInputQueueOrderError::SelfInterruptPredecessor { turn: *turn });
        }
        if !work_by_turn.contains_key(&predecessor) {
            return Err(AcceptedInputQueueOrderError::MissingInterruptPredecessor {
                turn: *turn,
                predecessor,
            });
        }
        if let Some(existing) = successor_by_predecessor.get(&predecessor) {
            let (first_successor, second_successor) = canonical_pair(*existing, *turn);
            return Err(AcceptedInputQueueOrderError::MultipleInterruptSuccessors {
                predecessor,
                first_successor,
                second_successor,
            });
        }

        successor_by_predecessor.insert(predecessor, *turn);
    }

    let mut ordinary_roots: Vec<_> = work_by_turn
        .iter()
        .filter_map(|(turn, order)| {
            (order.priority() == AcceptedInputQueuePriority::Ordinary)
                .then_some((order.acceptance_position(), *turn))
        })
        .collect();
    ordinary_roots.sort_unstable_by_key(|(position, _)| *position);

    let mut ordered = Vec::with_capacity(work_by_turn.len());
    let mut visited = BTreeSet::new();
    for (_, root) in ordinary_roots {
        let mut current = root;
        loop {
            if !visited.insert(current) {
                return Err(AcceptedInputQueueOrderError::InterruptCycle { turn: current });
            }
            ordered.push(current);

            match successor_by_predecessor.get(&current) {
                Some(successor) => current = *successor,
                None => break,
            }
        }
    }

    if ordered.len() != work_by_turn.len()
        && let Some(turn) = work_by_turn
            .keys()
            .copied()
            .find(|turn| !visited.contains(turn))
    {
        return Err(AcceptedInputQueueOrderError::InterruptCycle { turn });
    }

    for (turn, order) in &work_by_turn {
        let AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } =
            order.priority()
        else {
            continue;
        };
        let Some(predecessor_order) = work_by_turn.get(&predecessor) else {
            return Err(AcceptedInputQueueOrderError::MissingInterruptPredecessor {
                turn: *turn,
                predecessor,
            });
        };
        let predecessor_position = predecessor_order.acceptance_position();
        if order.acceptance_position() <= predecessor_position {
            return Err(
                AcceptedInputQueueOrderError::InterruptPositionNotAfterPredecessor {
                    turn: *turn,
                    predecessor,
                    position: order.acceptance_position(),
                    predecessor_position,
                },
            );
        }
    }

    let order_index_by_turn: BTreeMap<_, _> = ordered
        .iter()
        .enumerate()
        .map(|(index, turn)| (*turn, index))
        .collect();
    let mut prior_interrupt = None;
    for turn in turn_by_position.values() {
        let Some(order) = work_by_turn.get(turn) else {
            continue;
        };
        let AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } =
            order.priority()
        else {
            continue;
        };
        let Some(predecessor_index) = order_index_by_turn.get(&predecessor).copied() else {
            continue;
        };

        if let Some((earlier_interrupt, earlier_predecessor, earlier_predecessor_index)) =
            prior_interrupt
            && predecessor_index <= earlier_predecessor_index
        {
            return Err(
                AcceptedInputQueueOrderError::InterruptPredecessorChronologyReversed {
                    earlier_interrupt,
                    earlier_predecessor,
                    later_interrupt: *turn,
                    later_predecessor: predecessor,
                },
            );
        }
        prior_interrupt = Some((*turn, predecessor, predecessor_index));
    }

    Ok(ordered)
}

fn canonical_pair(first: TurnId, second: TurnId) -> (TurnId, TurnId) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use expect_test::expect;

    use super::{
        AcceptedInputQueueOrder, AcceptedInputQueueOrderError, AcceptedInputQueuePriority,
        AcceptedInputQueueWork, SessionInputPosition, derive_accepted_input_total_order,
    };
    use crate::TurnId;
    use crate::test_support::{session_id, table, turn_id};

    fn positions(count: usize) -> Vec<SessionInputPosition> {
        let mut positions = Vec::with_capacity(count);
        let mut current = SessionInputPosition::first();
        for _ in 0..count {
            positions.push(current);
            current = current
                .checked_next()
                .expect("test position range must remain representable");
        }
        positions
    }

    fn ordinary(turn: u128, position: SessionInputPosition) -> AcceptedInputQueueWork {
        ordinary_in_session(100, turn, position)
    }

    fn ordinary_in_session(
        session: u128,
        turn: u128,
        position: SessionInputPosition,
    ) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(session),
            turn_id(turn),
            AcceptedInputQueueOrder::ordinary(position),
        )
    }

    fn interrupt(
        turn: u128,
        position: SessionInputPosition,
        predecessor: u128,
    ) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(100),
            turn_id(turn),
            AcceptedInputQueueOrder::interrupt_immediately_after(position, turn_id(predecessor)),
        )
    }

    /// Ordinary work accepted at the given ordinal; its turn seed derives
    /// from that one knob, decorrelated (`docs/testing-style.md`, rule 4).
    fn accepted_ordinary(acceptance: u64) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(100),
            decorrelated_turn(acceptance),
            AcceptedInputQueueOrder::ordinary(nth_position(acceptance)),
        )
    }

    /// Interrupt work accepted at the given ordinal, immediately after the
    /// exact predecessor fixture; its turn seed derives from that one knob,
    /// decorrelated (`docs/testing-style.md`, rule 4).
    fn accepted_interrupt(
        acceptance: u64,
        predecessor: AcceptedInputQueueWork,
    ) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(100),
            decorrelated_turn(acceptance),
            AcceptedInputQueueOrder::interrupt_immediately_after(
                nth_position(acceptance),
                predecessor.turn(),
            ),
        )
    }

    /// A turn identity seed descending as the acceptance ordinal ascends, so
    /// identity order and acceptance order disagree by construction and a
    /// derivation ordering by identity cannot accidentally pass.
    fn decorrelated_turn(acceptance: u64) -> TurnId {
        turn_id(u128::from(u64::MAX - acceptance))
    }

    fn nth_position(ordinal: u64) -> SessionInputPosition {
        SessionInputPosition::try_from_u64(ordinal).expect("test acceptance ordinals are positive")
    }

    /// Renders the derived total order for snapshot review: one row per
    /// derived slot, holding only the acceptance ordinal and priority fact
    /// the derivation depends on (`docs/testing-style.md`, rule 12).
    fn derived_order_table(facts: &[AcceptedInputQueueWork]) -> String {
        let derived = derive_accepted_input_total_order(facts.iter().copied())
            .expect("snapshot rendering requires a derivable fact set");
        let order_by_turn: BTreeMap<_, _> = facts
            .iter()
            .map(|work| (work.turn(), work.order()))
            .collect();
        let rows: Vec<Vec<String>> = derived
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                let order = order_by_turn[turn];
                let priority = match order.priority() {
                    AcceptedInputQueuePriority::Ordinary => "ordinary".to_string(),
                    AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } => {
                        format!(
                            "interrupt immediately after input {}",
                            order_by_turn[&predecessor].acceptance_position().as_u64()
                        )
                    }
                };
                vec![
                    (index + 1).to_string(),
                    order.acceptance_position().as_u64().to_string(),
                    priority,
                ]
            })
            .collect();

        table(&["derived", "accepted", "priority"], &rows)
    }

    #[test]
    fn position_successor_is_checked_instead_of_panicking_at_exhaustion() {
        let first = SessionInputPosition::first();
        let second = first
            .checked_next()
            .expect("the second test position is representable");

        assert!(first < second);
        assert_eq!(SessionInputPosition(u64::MAX).checked_next(), None);
    }

    /// INV-002: reconstitution accepts the complete positive `u64` domain and
    /// rejects the zero sentinel without admitting a storage representation.
    #[test]
    fn inv002_input_position_checked_u64_boundary() {
        assert_eq!(SessionInputPosition::try_from_u64(0), None);
        assert_eq!(
            SessionInputPosition::try_from_u64(1),
            Some(SessionInputPosition::first())
        );

        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        assert_eq!(maximum.as_u64(), u64::MAX);
    }

    /// INV-009: queue-order facts expose exactly the immutable acceptance
    /// position and typed priority they were constructed with, and the
    /// derivation projection round-trips its session, turn, and order without
    /// substituting or dropping any identity.
    #[test]
    fn inv009_queue_order_facts_expose_their_construction_inputs() {
        let position = positions(2);

        let ordinary_order = AcceptedInputQueueOrder::ordinary(position[0]);
        assert_eq!(ordinary_order.acceptance_position(), position[0]);
        assert_eq!(
            ordinary_order.priority(),
            AcceptedInputQueuePriority::Ordinary
        );

        let predecessor = turn_id(7);
        let interrupt_order =
            AcceptedInputQueueOrder::interrupt_immediately_after(position[1], predecessor);
        assert_eq!(interrupt_order.acceptance_position(), position[1]);
        assert_eq!(
            interrupt_order.priority(),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor }
        );

        let work = AcceptedInputQueueWork::new(session_id(100), turn_id(3), interrupt_order);
        assert_eq!(work.session(), session_id(100));
        assert_eq!(work.turn(), turn_id(3));
        assert_eq!(work.order(), interrupt_order);
    }

    /// S09 / INV-009: ordinary work is ordered by immutable acceptance
    /// position, independent of fact iteration order.
    #[test]
    fn s09_inv009_ordinary_work_is_fifo_by_acceptance_position() {
        let position = positions(3);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(3, position[2]),
                ordinary(1, position[0]),
                ordinary(2, position[1]),
            ]),
            Ok(vec![turn_id(1), turn_id(2), turn_id(3)])
        );
    }

    /// S07 / INV-009: an interrupt is the immediate successor of its active
    /// predecessor and jumps all then-unstarted ordinary work.
    #[test]
    fn s07_inv009_interrupt_precedes_existing_ordinary_work() {
        let position = positions(3);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(1, position[0]),
                ordinary(2, position[1]),
                interrupt(3, position[2], 1),
            ]),
            Ok(vec![turn_id(1), turn_id(3), turn_id(2)])
        );
    }

    /// S07 / INV-009: nested interrupts recursively compose the same
    /// immediate-successor rule.
    #[test]
    fn s07_inv009_nested_interrupts_form_one_successor_chain() {
        let first = accepted_ordinary(1);
        let second = accepted_ordinary(2);
        let interrupt = accepted_interrupt(3, first);
        let nested = accepted_interrupt(4, interrupt);
        let facts = [nested, second, interrupt, first];

        assert_eq!(
            derive_accepted_input_total_order(facts),
            Ok(vec![
                first.turn(),
                interrupt.turn(),
                nested.turn(),
                second.turn(),
            ])
        );
        expect![[r#"
            derived | accepted | priority
            ------- | -------- | -----------------------------------
            1       | 1        | ordinary
            2       | 3        | interrupt immediately after input 1
            3       | 4        | interrupt immediately after input 3
            4       | 2        | ordinary
        "#]]
        .assert_eq(&derived_order_table(&facts));
    }

    /// S07 / S08 / S09 / INV-009: after an interrupt successor, ordinary and
    /// reclassified work retain their original acceptance order.
    #[test]
    fn s07_s08_s09_inv009_work_after_interrupt_retains_original_positions() {
        let first = accepted_ordinary(1);
        let second = accepted_ordinary(2);
        let interrupt = accepted_interrupt(3, first);
        let fourth = accepted_ordinary(4);
        let facts = [fourth, interrupt, second, first];

        assert_eq!(
            derive_accepted_input_total_order(facts),
            Ok(vec![
                first.turn(),
                interrupt.turn(),
                second.turn(),
                fourth.turn(),
            ])
        );
        expect![[r#"
            derived | accepted | priority
            ------- | -------- | -----------------------------------
            1       | 1        | ordinary
            2       | 3        | interrupt immediately after input 1
            3       | 2        | ordinary
            4       | 4        | ordinary
        "#]]
        .assert_eq(&derived_order_table(&facts));
    }

    /// S03 / INV-009: restart can derive the same order from every iteration
    /// permutation of the same durable fact set.
    #[test]
    fn s03_inv009_total_order_is_deterministic_for_all_fact_permutations() {
        let position = positions(4);
        let mut facts = vec![
            ordinary(1, position[0]),
            ordinary(2, position[1]),
            interrupt(3, position[2], 1),
            interrupt(4, position[3], 3),
        ];
        let expected = vec![turn_id(1), turn_id(3), turn_id(4), turn_id(2)];

        assert_all_permutations_derive(&mut facts, 0, &expected);
    }

    /// S03 / INV-009: the empty and singleton currently-known fact sets each
    /// have one deterministic total order.
    #[test]
    fn s03_inv009_empty_and_singleton_fact_sets_have_total_orders() {
        let position = SessionInputPosition::first();

        assert_eq!(derive_accepted_input_total_order([]), Ok(Vec::new()));
        assert_eq!(
            derive_accepted_input_total_order([ordinary(1, position)]),
            Ok(vec![turn_id(1)])
        );
    }

    /// S03 / INV-009: order derivation rejects facts associated with different
    /// sessions instead of comparing their session-local positions.
    #[test]
    fn s03_inv009_mixed_session_fact_sets_are_rejected() {
        let position = positions(2);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary_in_session(100, 1, position[0]),
                ordinary_in_session(200, 2, position[1]),
            ]),
            Err(AcceptedInputQueueOrderError::MixedSessions {
                first_session: session_id(100),
                second_session: session_id(200),
            })
        );
    }

    /// S03 / INV-009: duplicate identity or position facts cannot be silently
    /// tie-broken into a queue order.
    #[test]
    fn s03_inv009_duplicate_turns_and_positions_are_rejected() {
        let position = positions(2);
        let mut overlapping_duplicates = vec![
            ordinary(1, position[0]),
            ordinary(2, position[0]),
            ordinary(2, position[1]),
        ];
        let duplicate_turn = AcceptedInputQueueOrderError::DuplicateTurn { turn: turn_id(2) };
        assert_all_permutations_reject(&mut overlapping_duplicates, 0, &duplicate_turn);
        let mut duplicate_positions = vec![
            ordinary(3, position[0]),
            ordinary(1, position[0]),
            ordinary(2, position[0]),
        ];
        let expected = AcceptedInputQueueOrderError::DuplicateAcceptancePosition {
            position: position[0],
            first_turn: turn_id(1),
            second_turn: turn_id(2),
        };

        assert_all_permutations_reject(&mut duplicate_positions, 0, &expected);
    }

    /// S07 / INV-009: every interrupt priority fact names one different,
    /// currently known predecessor.
    #[test]
    fn s07_inv009_missing_and_self_interrupt_predecessors_are_rejected() {
        let position = positions(2);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(1, position[0]),
                interrupt(2, position[1], 3),
            ]),
            Err(AcceptedInputQueueOrderError::MissingInterruptPredecessor {
                turn: turn_id(2),
                predecessor: turn_id(3),
            })
        );
        assert_eq!(
            derive_accepted_input_total_order([interrupt(1, position[0], 1)]),
            Err(AcceptedInputQueueOrderError::SelfInterruptPredecessor { turn: turn_id(1) })
        );
    }

    /// S07 / INV-009: the baseline permits only one immediate interrupt
    /// successor for a predecessor.
    #[test]
    fn s07_inv009_multiple_interrupt_successors_are_rejected() {
        let position = positions(3);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(1, position[0]),
                interrupt(2, position[1], 1),
                interrupt(3, position[2], 1),
            ]),
            Err(AcceptedInputQueueOrderError::MultipleInterruptSuccessors {
                predecessor: turn_id(1),
                first_successor: turn_id(2),
                second_successor: turn_id(3),
            })
        );
    }

    /// S03 / S07 / INV-009: unrooted interrupt cycles cannot be interpreted
    /// as durable queue order.
    #[test]
    fn s03_s07_inv009_interrupt_cycles_are_rejected() {
        let position = positions(2);

        assert_eq!(
            derive_accepted_input_total_order([
                interrupt(1, position[0], 2),
                interrupt(2, position[1], 1),
            ]),
            Err(AcceptedInputQueueOrderError::InterruptCycle { turn: turn_id(1) })
        );
    }

    /// S07 / INV-009: an interrupt input must have been accepted after its
    /// active predecessor even though priority moves it ahead of ordinary work.
    #[test]
    fn s07_inv009_time_inverted_interrupt_edges_are_rejected() {
        let position = positions(2);

        assert_eq!(
            derive_accepted_input_total_order([
                interrupt(1, position[0], 2),
                ordinary(2, position[1]),
            ]),
            Err(
                AcceptedInputQueueOrderError::InterruptPositionNotAfterPredecessor {
                    turn: turn_id(1),
                    predecessor: turn_id(2),
                    position: position[0],
                    predecessor_position: position[1],
                }
            )
        );
    }

    /// S07 / INV-009: later interrupt inputs cannot target a predecessor that
    /// must already have terminalized for an earlier interrupt target to run.
    #[test]
    fn s07_inv009_reversed_active_target_chronology_is_rejected() {
        let position = positions(4);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(1, position[0]),
                ordinary(2, position[1]),
                interrupt(3, position[2], 2),
                interrupt(4, position[3], 1),
            ]),
            Err(
                AcceptedInputQueueOrderError::InterruptPredecessorChronologyReversed {
                    earlier_interrupt: turn_id(3),
                    earlier_predecessor: turn_id(2),
                    later_interrupt: turn_id(4),
                    later_predecessor: turn_id(1),
                }
            )
        );
    }

    /// S07 / INV-009: interrupt targets may advance across ordinary roots as
    /// those roots become active in durable order.
    #[test]
    fn s07_inv009_independent_interrupt_chains_follow_active_progress() {
        let position = positions(4);

        assert_eq!(
            derive_accepted_input_total_order([
                ordinary(1, position[0]),
                ordinary(2, position[1]),
                interrupt(3, position[2], 1),
                interrupt(4, position[3], 2),
            ]),
            Ok(vec![turn_id(1), turn_id(3), turn_id(2), turn_id(4)])
        );
    }

    fn assert_all_permutations_derive(
        facts: &mut [AcceptedInputQueueWork],
        index: usize,
        expected: &[TurnId],
    ) {
        if index == facts.len() {
            assert_eq!(
                derive_accepted_input_total_order(facts.iter().copied()),
                Ok(expected.to_vec())
            );
            return;
        }

        for swap_index in index..facts.len() {
            facts.swap(index, swap_index);
            assert_all_permutations_derive(facts, index + 1, expected);
            facts.swap(index, swap_index);
        }
    }

    fn assert_all_permutations_reject(
        facts: &mut [AcceptedInputQueueWork],
        index: usize,
        expected: &AcceptedInputQueueOrderError,
    ) {
        if index == facts.len() {
            assert_eq!(
                derive_accepted_input_total_order(facts.iter().copied()),
                Err(expected.clone())
            );
            return;
        }

        for swap_index in index..facts.len() {
            facts.swap(index, swap_index);
            assert_all_permutations_reject(facts, index + 1, expected);
            facts.swap(index, swap_index);
        }
    }
}
