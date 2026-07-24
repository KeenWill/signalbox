//! Applied-interrupt command-result correlation and cancellation authority.
//!
//! docs/spec/identity-and-commands.md and
//! docs/spec/turn-lifecycle-and-scheduling.md are the normative
//! specifications. A raw command identity is not authority: only the
//! correlated applied result of the exact interrupt command can carry the
//! proof consumed by later turn and attempt lifecycle transitions.

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueueOrderError, AcceptedInputQueuePriority, AcceptedInputQueueWork,
    DeliveryRequest, DurableCommandId, SessionId, SessionInputPosition, TurnId,
    derive_accepted_input_total_order,
};

/// Purpose-specific authority created by one exact applied interrupt.
///
/// The field shape is the accepted algebra in
/// docs/spec/turn-lifecycle-and-scheduling.md. Both fields are private, and
/// no raw constructor or conversion from [`DurableCommandId`] exists:
/// INV-001 / INV-029 construction proofs:
///
/// ```compile_fail
/// use signalbox_domain::{AppliedInterruptProof, DurableCommandId, TurnId};
///
/// fn raw_parts_are_not_cancellation_authority(
///     command: DurableCommandId,
///     predecessor: TurnId,
/// ) {
///     let _ = AppliedInterruptProof {
///         command,
///         predecessor,
///     };
/// }
/// ```
///
/// ```compile_fail
/// use signalbox_domain::{AppliedInterruptProof, DurableCommandId};
///
/// fn a_command_id_alone_is_not_a_proof(command: DurableCommandId) {
///     let _: AppliedInterruptProof = command.into();
/// }
/// ```
///
/// The sole public producer is [`AppliedInterruptCommandResult::proof`]. The
/// applied result itself is opaque; its module-private correlation seam is
/// reserved for a later transaction-owning child adapter in this module.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppliedInterruptProof {
    command: DurableCommandId,
    predecessor: TurnId,
}

impl AppliedInterruptProof {
    /// Returns the owner-global command identity whose applied result supplied
    /// this authority.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the exact turn whose interrupt transition was applied.
    pub const fn predecessor(&self) -> TurnId {
        self.predecessor
    }
}

#[cfg(test)]
pub(crate) const fn test_applied_interrupt_proof(
    command: DurableCommandId,
    predecessor: TurnId,
) -> AppliedInterruptProof {
    AppliedInterruptProof {
        command,
        predecessor,
    }
}

/// The correlated domain result of one applied interrupt command.
///
/// This value groups the purpose-specific proof with the accepted input and
/// immediate-successor facts created by the same aggregate transition. Its
/// private fields have no public constructor. The module-private correlation
/// boundary validates pure domain facts; it does not itself claim that a
/// database commit occurred or define a persistence record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppliedInterruptCommandResult {
    proof: AppliedInterruptProof,
    session: SessionId,
    accepted_input: AcceptedInputId,
    successor: TurnId,
    successor_order: AcceptedInputQueueOrder,
}

impl AppliedInterruptCommandResult {
    pub(crate) fn from_correlated_submit(
        command: DurableCommandId,
        session: SessionId,
        predecessor: TurnId,
        accepted_input: AcceptedInputId,
        successor: TurnId,
        successor_order: AcceptedInputQueueOrder,
    ) -> Option<Self> {
        if successor == predecessor
            || successor_order.priority()
                != (AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor })
        {
            return None;
        }
        Some(Self {
            proof: AppliedInterruptProof {
                command,
                predecessor,
            },
            session,
            accepted_input,
            successor,
            successor_order,
        })
    }

    /// Returns the cancellation authority supplied by this exact applied
    /// result.
    pub const fn proof(&self) -> AppliedInterruptProof {
        self.proof
    }

    /// Returns the session in which predecessor and successor are correlated.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted input created by the applied command.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the immediate-successor turn created by the applied command.
    pub const fn successor(&self) -> TurnId {
        self.successor
    }

    /// Returns the successor's immutable interrupt-priority facts.
    pub const fn successor_order(&self) -> AcceptedInputQueueOrder {
        self.successor_order
    }
}

/// A narrow projection supplied by the future transaction-owning aggregate.
///
/// This is deliberately not the canonical `SubmitInput` command or a storage
/// result. It exists only to make the proof-correlation checks implementable
/// before command handling and persistence arrive in later slices.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 adds the trusted adapter")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum HandledSubmitInputProjection {
    /// Authoritative handling recorded a domain rejection and no semantic
    /// work identities.
    Rejected {
        command: DurableCommandId,
        command_session: SessionId,
        command_delivery: DeliveryRequest,
    },
    /// Authoritative handling applied and supplied candidate correlated facts.
    Applied(Box<AppliedSubmitInputFacts>),
}

/// Candidate facts from an already-applied submit-input result.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 adds the trusted adapter")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedSubmitInputFacts {
    command: DurableCommandId,
    command_session: SessionId,
    command_delivery: DeliveryRequest,
    predecessor_session: SessionId,
    predecessor: TurnId,
    accepted_input_session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    accepted_delivery: DeliveryRequest,
    accepted_position: SessionInputPosition,
    successor: AcceptedInputQueueWork,
}

/// Identifies which applied-result association crossed a session boundary.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 adds the trusted adapter")
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptSessionAssociation {
    Predecessor,
    AcceptedInput,
    Successor,
}

/// Reports why candidate handled-result facts cannot construct interrupt
/// authority.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 adds the trusted adapter")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum AppliedInterruptConstructionError {
    RejectedCommand {
        command: DurableCommandId,
    },
    NonInterruptCommand {
        command: DurableCommandId,
    },
    AcceptedDeliveryMismatch {
        command_delivery: DeliveryRequest,
        accepted_delivery: DeliveryRequest,
    },
    TargetMismatch {
        requested: TurnId,
        authoritative: TurnId,
    },
    SessionMismatch {
        association: InterruptSessionAssociation,
        command_session: SessionId,
        associated_session: SessionId,
    },
    SuccessorMatchesPredecessor {
        turn: TurnId,
    },
    AcceptedInputNotSuccessorOrigin {
        disposition: AcceptedInputDisposition,
        successor: TurnId,
    },
    AcceptedPositionMismatch {
        accepted_position: SessionInputPosition,
        successor_position: SessionInputPosition,
    },
    SuccessorHasOrdinaryPriority,
    SuccessorTargetsDifferentPredecessor {
        expected: TurnId,
        actual: TurnId,
    },
    SuccessorAlreadyKnown {
        successor: TurnId,
    },
    InvalidQueueOrder {
        error: AcceptedInputQueueOrderError,
    },
}

/// Correlates an already-handled submit-input result into interrupt authority.
///
/// `known_work_before_application` must be the complete pre-application queue
/// projection supplied by the aggregate. This function validates correlations
/// inside those facts, including the derived post-application adjacency. It
/// cannot independently certify fact-set completeness, authoritative active
/// state, or persistence commit.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 adds the trusted adapter")
)]
fn correlate_applied_interrupt(
    handled: &HandledSubmitInputProjection,
    known_work_before_application: &[AcceptedInputQueueWork],
) -> Result<AppliedInterruptCommandResult, AppliedInterruptConstructionError> {
    let facts = match handled {
        HandledSubmitInputProjection::Rejected { command, .. } => {
            return Err(AppliedInterruptConstructionError::RejectedCommand { command: *command });
        }
        HandledSubmitInputProjection::Applied(facts) => facts,
    };

    let DeliveryRequest::Interrupt {
        expected_active_turn,
        ..
    } = facts.command_delivery
    else {
        return Err(AppliedInterruptConstructionError::NonInterruptCommand {
            command: facts.command,
        });
    };

    if facts.accepted_delivery != facts.command_delivery {
        return Err(
            AppliedInterruptConstructionError::AcceptedDeliveryMismatch {
                command_delivery: facts.command_delivery,
                accepted_delivery: facts.accepted_delivery,
            },
        );
    }
    if expected_active_turn != facts.predecessor {
        return Err(AppliedInterruptConstructionError::TargetMismatch {
            requested: expected_active_turn,
            authoritative: facts.predecessor,
        });
    }

    for (association, associated_session) in [
        (
            InterruptSessionAssociation::Predecessor,
            facts.predecessor_session,
        ),
        (
            InterruptSessionAssociation::AcceptedInput,
            facts.accepted_input_session,
        ),
        (
            InterruptSessionAssociation::Successor,
            facts.successor.session(),
        ),
    ] {
        if associated_session != facts.command_session {
            return Err(AppliedInterruptConstructionError::SessionMismatch {
                association,
                command_session: facts.command_session,
                associated_session,
            });
        }
    }

    let successor = facts.successor.turn();
    if successor == facts.predecessor {
        return Err(
            AppliedInterruptConstructionError::SuccessorMatchesPredecessor { turn: successor },
        );
    }

    match facts.accepted_input.disposition() {
        AcceptedInputDisposition::OriginOf(origin) if *origin == successor => {}
        disposition => {
            return Err(
                AppliedInterruptConstructionError::AcceptedInputNotSuccessorOrigin {
                    disposition: disposition.clone(),
                    successor,
                },
            );
        }
    }

    let successor_position = facts.successor.order().acceptance_position();
    if facts.accepted_position != successor_position {
        return Err(
            AppliedInterruptConstructionError::AcceptedPositionMismatch {
                accepted_position: facts.accepted_position,
                successor_position,
            },
        );
    }

    match facts.successor.order().priority() {
        AcceptedInputQueuePriority::Ordinary => {
            return Err(AppliedInterruptConstructionError::SuccessorHasOrdinaryPriority);
        }
        AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor }
            if predecessor != facts.predecessor =>
        {
            return Err(
                AppliedInterruptConstructionError::SuccessorTargetsDifferentPredecessor {
                    expected: facts.predecessor,
                    actual: predecessor,
                },
            );
        }
        AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. } => {}
    }

    if known_work_before_application
        .iter()
        .any(|work| work.turn() == successor)
    {
        return Err(AppliedInterruptConstructionError::SuccessorAlreadyKnown { successor });
    }

    derive_accepted_input_total_order(
        known_work_before_application
            .iter()
            .copied()
            .chain([facts.successor]),
    )
    .map_err(|error| AppliedInterruptConstructionError::InvalidQueueOrder { error })?;

    Ok(AppliedInterruptCommandResult {
        proof: AppliedInterruptProof {
            command: facts.command,
            predecessor: facts.predecessor,
        },
        session: facts.command_session,
        accepted_input: facts.accepted_input.id(),
        successor,
        successor_order: facts.successor.order(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AppliedInterruptConstructionError, AppliedSubmitInputFacts, HandledSubmitInputProjection,
        InterruptSessionAssociation, correlate_applied_interrupt,
    };
    use crate::test_support::{
        accepted_input_id, command_id, model_call_id as call_id, session_id, turn_id,
    };
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputQueueOrderError, AcceptedInputQueueWork, DeliveryRequest, DurableCommandId,
        ModelSelectionOverride, PerInputConfigurationChoices, SessionConfigurationDefaultsVersion,
        SessionId, SessionInputPosition, SteeringBinding, SteeringReclassificationReason, TurnId,
    };

    fn choices() -> PerInputConfigurationChoices {
        PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        )
    }

    fn interrupt_delivery(predecessor: TurnId) -> DeliveryRequest {
        DeliveryRequest::Interrupt {
            expected_active_turn: predecessor,
            configuration: choices(),
        }
    }

    /// Ordinary work accepted at the given ordinal; its turn seed derives
    /// from that one knob, decorrelated (`docs/agents/testing-style.md`, rule 4).
    fn accepted_ordinary(acceptance: u64) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(100),
            decorrelated_turn(acceptance),
            AcceptedInputQueueOrder::ordinary(nth_position(acceptance)),
        )
    }

    /// Interrupt work accepted at the given ordinal, immediately after the
    /// exact predecessor fixture; its turn seed derives from that one knob,
    /// decorrelated (`docs/agents/testing-style.md`, rule 4).
    fn accepted_interrupt(
        acceptance: u64,
        predecessor: AcceptedInputQueueWork,
    ) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            predecessor.session(),
            decorrelated_turn(acceptance),
            AcceptedInputQueueOrder::interrupt_immediately_after(
                nth_position(acceptance),
                predecessor.turn(),
            ),
        )
    }

    /// A turn identity seed decorrelated from acceptance order by rotating the
    /// low bit into the high bit. The bijection keeps identities distinct
    /// while consecutive acceptance ordinals do not sort as identities.
    fn decorrelated_turn(acceptance: u64) -> TurnId {
        turn_id(u128::from(acceptance.rotate_right(1)))
    }

    /// A command seed decorrelated from successor acceptance: it descends
    /// from the top of the `u128` range as the ordinal ascends.
    fn decorrelated_command_seed(successor_acceptance: u64) -> u128 {
        u128::MAX - u128::from(successor_acceptance)
    }

    /// An accepted-input seed decorrelated from successor acceptance: it
    /// descends from the top of the `u64` range as the ordinal ascends. The
    /// separate range keeps it distinct from the command seed for every
    /// ordinal used by these tests.
    fn decorrelated_accepted_input_seed(successor_acceptance: u64) -> u128 {
        u128::from(u64::MAX - successor_acceptance)
    }

    fn nth_position(ordinal: u64) -> SessionInputPosition {
        SessionInputPosition::try_from_u64(ordinal).expect("test acceptance ordinals are positive")
    }

    /// The complete facts supplied to interrupt correlation, mirroring
    /// [`AppliedSubmitInputFacts`] field for field so a test perturbs exactly
    /// the named relationship it exercises (`docs/agents/testing-style.md`, rule 4).
    #[derive(Clone)]
    struct AppliedFacts {
        command: DurableCommandId,
        command_session: SessionId,
        command_delivery: DeliveryRequest,
        predecessor_session: SessionId,
        predecessor: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        accepted_delivery: DeliveryRequest,
        accepted_position: SessionInputPosition,
        successor: AcceptedInputQueueWork,
    }

    impl AppliedFacts {
        /// Canonically matching applied facts for a successor accepted at the
        /// given ordinal after the exact predecessor fixture. Command and
        /// accepted-input identities derive from the acceptance knob.
        fn matching(successor_acceptance: u64, predecessor: AcceptedInputQueueWork) -> Self {
            let successor = accepted_interrupt(successor_acceptance, predecessor);
            let delivery = interrupt_delivery(predecessor.turn());
            Self {
                command: command_id(decorrelated_command_seed(successor_acceptance)),
                command_session: predecessor.session(),
                command_delivery: delivery,
                predecessor_session: predecessor.session(),
                predecessor: predecessor.turn(),
                accepted_input_session: predecessor.session(),
                accepted_input: AcceptedInputLifecycle::new(
                    accepted_input_id(decorrelated_accepted_input_seed(successor_acceptance)),
                    AcceptedInputDisposition::OriginOf(successor.turn()),
                ),
                accepted_delivery: delivery,
                accepted_position: successor.order().acceptance_position(),
                successor,
            }
        }

        fn input(self) -> AppliedSubmitInputFacts {
            AppliedSubmitInputFacts {
                command: self.command,
                command_session: self.command_session,
                command_delivery: self.command_delivery,
                predecessor_session: self.predecessor_session,
                predecessor: self.predecessor,
                accepted_input_session: self.accepted_input_session,
                accepted_input: self.accepted_input,
                accepted_delivery: self.accepted_delivery,
                accepted_position: self.accepted_position,
                successor: self.successor,
            }
        }
    }

    fn correlate(
        facts: AppliedFacts,
        known_work: &[AcceptedInputQueueWork],
    ) -> Result<super::AppliedInterruptCommandResult, AppliedInterruptConstructionError> {
        correlate_applied_interrupt(
            &HandledSubmitInputProjection::Applied(Box::new(facts.input())),
            known_work,
        )
    }

    /// S07 / INV-001 / INV-029: the exact applied interrupt result alone
    /// supplies proof tied to its command, predecessor, input, and successor.
    #[test]
    fn s07_inv001_inv029_exact_applied_interrupt_constructs_correlated_authority() {
        let predecessor = accepted_ordinary(1);
        let waiting = accepted_ordinary(2);
        let facts = AppliedFacts::matching(3, predecessor);
        let known_work = [predecessor, waiting];

        let result = correlate(facts.clone(), &known_work)
            .expect("the exact correlated applied interrupt constructs authority");

        assert_eq!(result.proof().command(), facts.command);
        assert_eq!(result.proof().predecessor(), facts.predecessor);
        assert_eq!(result.session(), facts.command_session);
        assert_eq!(result.accepted_input(), facts.accepted_input.id());
        assert_eq!(result.successor(), facts.successor.turn());
        assert_eq!(result.successor_order(), facts.successor.order());
    }

    /// S07 / INV-001 / INV-029: nested applications produce structurally
    /// exact proof values for their distinct commands and active predecessors.
    #[test]
    fn s07_inv001_inv029_nested_interrupt_proofs_preserve_exact_identity() {
        let root = accepted_ordinary(1);
        let first_facts = AppliedFacts::matching(2, root);
        let nested_facts = AppliedFacts::matching(3, first_facts.successor);
        let first =
            correlate(first_facts.clone(), &[root]).expect("the first interrupt is correlated");
        let nested = correlate(nested_facts.clone(), &[root, first_facts.successor])
            .expect("the nested interrupt is correlated");

        assert_eq!(first.proof().command(), first_facts.command);
        assert_eq!(first.proof().predecessor(), first_facts.predecessor);
        assert_eq!(nested.proof().command(), nested_facts.command);
        assert_eq!(nested.proof().predecessor(), nested_facts.predecessor);
        assert_ne!(first.proof(), nested.proof());
    }

    /// S07 / INV-001 / INV-029: an authoritative rejection contains no
    /// applied work facts and cannot supply cancellation authority.
    #[test]
    fn s07_inv001_inv029_rejected_command_cannot_construct_proof() {
        let no_known_work: [AcceptedInputQueueWork; 0] = [];
        let rejected_command = command_id(10);
        let command_session = session_id(100);
        let requested_predecessor = turn_id(1);
        let handled = HandledSubmitInputProjection::Rejected {
            command: rejected_command,
            command_session,
            command_delivery: interrupt_delivery(requested_predecessor),
        };

        assert_eq!(
            correlate_applied_interrupt(&handled, &no_known_work),
            Err(AppliedInterruptConstructionError::RejectedCommand {
                command: rejected_command,
            })
        );
    }

    /// S07 / INV-001 / INV-029: no other delivery discriminator can be
    /// cross-wired to applied interrupt work and acquire authority.
    #[test]
    fn s07_inv001_inv029_non_interrupt_commands_cannot_construct_proof() {
        assert_non_interrupt_delivery_rejected(DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(),
        });
        assert_non_interrupt_delivery_rejected(DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(1),
        });
        assert_non_interrupt_delivery_rejected(DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: turn_id(1),
            configuration: choices(),
        });
    }

    #[track_caller]
    fn assert_non_interrupt_delivery_rejected(command_delivery: DeliveryRequest) {
        let predecessor = accepted_ordinary(1);
        let mut facts = AppliedFacts::matching(2, predecessor);
        facts.command_delivery = command_delivery;
        facts.accepted_delivery = command_delivery;

        assert_eq!(
            correlate(facts.clone(), &[predecessor]),
            Err(AppliedInterruptConstructionError::NonInterruptCommand {
                command: facts.command,
            })
        );
    }

    /// S07 / INV-001 / INV-029: the stored accepted treatment and exact
    /// authoritative predecessor must match the applied command payload.
    #[test]
    fn s07_inv001_inv029_cross_wired_delivery_or_target_is_rejected() {
        let predecessor = accepted_ordinary(1);
        let unrelated = accepted_ordinary(9);
        let known_work = [predecessor];
        let matching = AppliedFacts::matching(2, predecessor);
        let mut delivery_mismatch = matching.clone();
        delivery_mismatch.accepted_delivery = interrupt_delivery(unrelated.turn());
        let mut target_mismatch = matching;
        target_mismatch.predecessor = unrelated.turn();

        assert_eq!(
            correlate(delivery_mismatch.clone(), &known_work),
            Err(
                AppliedInterruptConstructionError::AcceptedDeliveryMismatch {
                    command_delivery: delivery_mismatch.command_delivery,
                    accepted_delivery: delivery_mismatch.accepted_delivery,
                }
            )
        );
        assert_eq!(
            correlate(target_mismatch.clone(), &known_work),
            Err(AppliedInterruptConstructionError::TargetMismatch {
                requested: predecessor.turn(),
                authoritative: target_mismatch.predecessor,
            })
        );
    }

    /// S07 / INV-029: predecessor, accepted input, and successor associations
    /// must all remain in the command's session.
    #[test]
    fn s07_inv029_every_cross_session_association_is_rejected() {
        let predecessor_work = accepted_ordinary(1);
        let other_session = session_id(200);
        let base = AppliedFacts::matching(2, predecessor_work);
        let mut predecessor = base.clone();
        predecessor.predecessor_session = other_session;
        let mut accepted_input = base.clone();
        accepted_input.accepted_input_session = other_session;
        let mut successor = base;
        successor.successor = AcceptedInputQueueWork::new(
            other_session,
            successor.successor.turn(),
            successor.successor.order(),
        );

        assert_cross_session_association_rejected(
            predecessor,
            InterruptSessionAssociation::Predecessor,
            other_session,
            &[predecessor_work],
        );
        assert_cross_session_association_rejected(
            accepted_input,
            InterruptSessionAssociation::AcceptedInput,
            other_session,
            &[predecessor_work],
        );
        assert_cross_session_association_rejected(
            successor,
            InterruptSessionAssociation::Successor,
            other_session,
            &[predecessor_work],
        );
    }

    #[track_caller]
    fn assert_cross_session_association_rejected(
        facts: AppliedFacts,
        association: InterruptSessionAssociation,
        associated_session: SessionId,
        known_work: &[AcceptedInputQueueWork],
    ) {
        assert_eq!(
            correlate(facts.clone(), known_work),
            Err(AppliedInterruptConstructionError::SessionMismatch {
                association,
                command_session: facts.command_session,
                associated_session,
            })
        );
    }

    /// S07 / INV-029: interrupt work must create a distinct successor turn.
    #[test]
    fn s07_inv029_predecessor_cannot_be_its_own_successor() {
        let predecessor = accepted_ordinary(1);
        let mut facts = AppliedFacts::matching(2, predecessor);
        facts.accepted_input = AcceptedInputLifecycle::new(
            facts.accepted_input.id(),
            AcceptedInputDisposition::OriginOf(facts.predecessor),
        );
        facts.successor = AcceptedInputQueueWork::new(
            facts.successor.session(),
            facts.predecessor,
            AcceptedInputQueueOrder::interrupt_immediately_after(
                facts.accepted_position,
                facts.predecessor,
            ),
        );

        assert_eq!(
            correlate(facts.clone(), &[predecessor]),
            Err(
                AppliedInterruptConstructionError::SuccessorMatchesPredecessor {
                    turn: facts.predecessor
                }
            )
        );
    }

    /// S07 / INV-029: the newly accepted input must be the exact successor's
    /// origin, never steering or another turn's origin.
    #[test]
    fn s07_inv029_non_origin_and_wrong_origin_dispositions_are_rejected() {
        assert_non_origin_disposition_rejected(AcceptedInputDisposition::OriginOf(turn_id(9)));
        assert_non_origin_disposition_rejected(AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(1)),
        });
        assert_non_origin_disposition_rejected(AcceptedInputDisposition::ConsumedAsSteering {
            call: call_id(8),
        });
        assert_non_origin_disposition_rejected(
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id(2),
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        );
    }

    #[track_caller]
    fn assert_non_origin_disposition_rejected(disposition: AcceptedInputDisposition) {
        let predecessor = accepted_ordinary(1);
        let mut facts = AppliedFacts::matching(2, predecessor);
        facts.accepted_input =
            AcceptedInputLifecycle::new(facts.accepted_input.id(), disposition.clone());

        assert_eq!(
            correlate(facts.clone(), &[predecessor]),
            Err(
                AppliedInterruptConstructionError::AcceptedInputNotSuccessorOrigin {
                    disposition,
                    successor: facts.successor.turn(),
                }
            )
        );
    }

    /// S07 / INV-029: the accepted position and typed successor priority must
    /// describe the same exact interrupt-created work.
    #[test]
    fn s07_inv029_cross_wired_position_or_priority_is_rejected() {
        let predecessor = accepted_ordinary(1);
        let unrelated = accepted_ordinary(9);
        let known_work = [predecessor];
        let matching = AppliedFacts::matching(2, predecessor);
        let mut position_mismatch = matching.clone();
        position_mismatch.accepted_position = nth_position(3);
        let mut ordinary_priority = matching.clone();
        ordinary_priority.successor = AcceptedInputQueueWork::new(
            matching.successor.session(),
            matching.successor.turn(),
            AcceptedInputQueueOrder::ordinary(matching.successor.order().acceptance_position()),
        );
        let mut wrong_target = matching;
        wrong_target.successor = AcceptedInputQueueWork::new(
            wrong_target.successor.session(),
            wrong_target.successor.turn(),
            AcceptedInputQueueOrder::interrupt_immediately_after(
                wrong_target.successor.order().acceptance_position(),
                unrelated.turn(),
            ),
        );

        assert_eq!(
            correlate(position_mismatch.clone(), &known_work),
            Err(
                AppliedInterruptConstructionError::AcceptedPositionMismatch {
                    accepted_position: position_mismatch.accepted_position,
                    successor_position: position_mismatch.successor.order().acceptance_position(),
                }
            )
        );
        assert_eq!(
            correlate(ordinary_priority, &known_work),
            Err(AppliedInterruptConstructionError::SuccessorHasOrdinaryPriority)
        );
        assert_eq!(
            correlate(wrong_target.clone(), &known_work),
            Err(
                AppliedInterruptConstructionError::SuccessorTargetsDifferentPredecessor {
                    expected: wrong_target.predecessor,
                    actual: unrelated.turn(),
                }
            )
        );
    }

    /// S07 / INV-009 / INV-029: the successor must be new and its target must
    /// exist in the complete pre-application queue projection.
    #[test]
    fn s07_inv009_inv029_preexisting_successor_or_missing_predecessor_is_rejected() {
        let predecessor = accepted_ordinary(1);
        let facts = AppliedFacts::matching(2, predecessor);
        let no_known_work: [AcceptedInputQueueWork; 0] = [];

        assert_eq!(
            correlate(facts.clone(), &[predecessor, facts.successor]),
            Err(AppliedInterruptConstructionError::SuccessorAlreadyKnown {
                successor: facts.successor.turn(),
            })
        );
        assert_eq!(
            correlate(facts.clone(), &no_known_work),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::MissingInterruptPredecessor {
                    turn: facts.successor.turn(),
                    predecessor: facts.predecessor,
                },
            })
        );
    }

    /// S07 / INV-009 / INV-029: existing priority facts cannot already claim
    /// another immediate interrupt successor for the same predecessor.
    #[test]
    fn s07_inv009_inv029_competing_interrupt_successor_is_rejected() {
        let predecessor = accepted_ordinary(1);
        let existing_successor = accepted_interrupt(2, predecessor);
        let facts = AppliedFacts::matching(3, predecessor);

        assert_eq!(
            correlate(facts.clone(), &[predecessor, existing_successor]),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::MultipleInterruptSuccessors {
                    predecessor: predecessor.turn(),
                    first_successor: existing_successor.turn(),
                    second_successor: facts.successor.turn(),
                },
            })
        );
    }

    /// S07 / INV-009 / INV-029: priority cannot move an input ahead of a
    /// predecessor that was accepted later.
    #[test]
    fn s07_inv009_inv029_time_inverted_interrupt_successor_is_rejected() {
        let predecessor = accepted_ordinary(2);
        let facts = AppliedFacts::matching(1, predecessor);

        assert_eq!(
            correlate(facts.clone(), &[predecessor]),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::InterruptPositionNotAfterPredecessor {
                    turn: facts.successor.turn(),
                    predecessor: facts.predecessor,
                    position: facts.successor.order().acceptance_position(),
                    predecessor_position: predecessor.order().acceptance_position(),
                },
            })
        );
    }
}
