//! Applied-interrupt command-result correlation and cancellation authority.
//!
//! ADR-0001, ADR-0004, and ADR-0027 are the normative specifications. A raw
//! command identity is not authority: only the correlated applied result of
//! the exact interrupt command can carry the proof consumed by later turn and
//! attempt lifecycle transitions.

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueueOrderError, AcceptedInputQueuePriority, AcceptedInputQueueWork,
    DeliveryRequest, DurableCommandId, SessionId, SessionInputPosition, TurnId,
    derive_accepted_input_total_order,
};

/// Purpose-specific authority created by one exact applied interrupt.
///
/// The field shape is the accepted ADR-0004 algebra. Both fields are private,
/// and no raw constructor or conversion from [`DurableCommandId`] exists:
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
        AcceptedInputQueueOrderError, AcceptedInputQueueWork, DeliveryRequest,
        ModelSelectionOverride, PerInputConfigurationChoices, SessionConfigurationDefaultsVersion,
        SessionInputPosition, SteeringBinding, SteeringReclassificationReason, TurnId,
    };

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

    fn ordinary(turn: u128, position: SessionInputPosition) -> AcceptedInputQueueWork {
        AcceptedInputQueueWork::new(
            session_id(100),
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

    /// Applied facts correlating one command with the exact successor work it
    /// created immediately after `predecessor`; the accepted-input identity,
    /// disposition, delivery, and position all derive from those two knobs.
    fn applied_facts(
        command: u128,
        predecessor: TurnId,
        successor: AcceptedInputQueueWork,
    ) -> AppliedSubmitInputFacts {
        let delivery = interrupt_delivery(predecessor);
        AppliedSubmitInputFacts {
            command: command_id(command),
            command_session: session_id(100),
            command_delivery: delivery,
            predecessor_session: session_id(100),
            predecessor,
            accepted_input_session: session_id(100),
            accepted_input: AcceptedInputLifecycle::new(
                accepted_input_id(command),
                AcceptedInputDisposition::OriginOf(successor.turn()),
            ),
            accepted_delivery: delivery,
            accepted_position: successor.order().acceptance_position(),
            successor,
        }
    }

    fn correlate(
        facts: AppliedSubmitInputFacts,
        known_work: &[AcceptedInputQueueWork],
    ) -> Result<super::AppliedInterruptCommandResult, AppliedInterruptConstructionError> {
        correlate_applied_interrupt(
            &HandledSubmitInputProjection::Applied(Box::new(facts)),
            known_work,
        )
    }

    /// S07 / INV-001 / INV-029: the exact applied interrupt result alone
    /// supplies proof tied to its command, predecessor, input, and successor.
    #[test]
    fn s07_inv001_inv029_exact_applied_interrupt_constructs_correlated_authority() {
        let position = positions(3);
        let known_work = [ordinary(1, position[0]), ordinary(2, position[1])];
        let successor = interrupt(3, position[2], 1);
        let facts = applied_facts(10, turn_id(1), successor);

        let result = correlate(facts, &known_work)
            .expect("the exact correlated applied interrupt constructs authority");

        assert_eq!(result.proof().command(), command_id(10));
        assert_eq!(result.proof().predecessor(), turn_id(1));
        assert_eq!(result.session(), session_id(100));
        assert_eq!(result.accepted_input(), accepted_input_id(10));
        assert_eq!(result.successor(), successor.turn());
        assert_eq!(result.successor_order(), successor.order());
    }

    /// S07 / INV-001 / INV-029: nested applications produce structurally
    /// exact proof values for their distinct commands and active predecessors.
    #[test]
    fn s07_inv001_inv029_nested_interrupt_proofs_preserve_exact_identity() {
        let position = positions(3);
        let root = ordinary(1, position[0]);
        let first_successor = interrupt(2, position[1], 1);
        let nested_successor = interrupt(3, position[2], 2);
        let first = correlate(applied_facts(10, root.turn(), first_successor), &[root])
            .expect("the first interrupt is correlated");
        let nested = correlate(
            applied_facts(11, first_successor.turn(), nested_successor),
            &[root, first_successor],
        )
        .expect("the nested interrupt is correlated");

        assert_eq!(first.proof().command(), command_id(10));
        assert_eq!(first.proof().predecessor(), root.turn());
        assert_eq!(nested.proof().command(), command_id(11));
        assert_eq!(nested.proof().predecessor(), first_successor.turn());
        assert_ne!(first.proof(), nested.proof());
    }

    /// S07 / INV-001 / INV-029: an authoritative rejection contains no
    /// applied work facts and cannot supply cancellation authority.
    #[test]
    fn s07_inv001_inv029_rejected_command_cannot_construct_proof() {
        let no_known_work: [AcceptedInputQueueWork; 0] = [];
        let handled = HandledSubmitInputProjection::Rejected {
            command: command_id(10),
            command_session: session_id(100),
            command_delivery: interrupt_delivery(turn_id(1)),
        };

        assert_eq!(
            correlate_applied_interrupt(&handled, &no_known_work),
            Err(AppliedInterruptConstructionError::RejectedCommand {
                command: command_id(10),
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
        let position = positions(2);
        let predecessor = ordinary(1, position[0]);
        let mut facts = applied_facts(10, predecessor.turn(), interrupt(2, position[1], 1));
        facts.command_delivery = command_delivery;
        facts.accepted_delivery = command_delivery;

        assert_eq!(
            correlate(facts, &[predecessor]),
            Err(AppliedInterruptConstructionError::NonInterruptCommand {
                command: command_id(10),
            })
        );
    }

    /// S07 / INV-001 / INV-029: the stored accepted treatment and exact
    /// authoritative predecessor must match the applied command payload.
    #[test]
    fn s07_inv001_inv029_cross_wired_delivery_or_target_is_rejected() {
        let position = positions(2);
        let known_work = [ordinary(1, position[0])];
        let successor = interrupt(2, position[1], 1);
        let mut delivery_mismatch = applied_facts(10, turn_id(1), successor);
        delivery_mismatch.accepted_delivery = interrupt_delivery(turn_id(9));
        let mut target_mismatch = applied_facts(10, turn_id(1), successor);
        target_mismatch.predecessor = turn_id(9);

        assert!(matches!(
            correlate(delivery_mismatch, &known_work),
            Err(AppliedInterruptConstructionError::AcceptedDeliveryMismatch { .. })
        ));
        assert_eq!(
            correlate(target_mismatch, &known_work),
            Err(AppliedInterruptConstructionError::TargetMismatch {
                requested: turn_id(1),
                authoritative: turn_id(9),
            })
        );
    }

    /// S07 / INV-029: predecessor, accepted input, and successor associations
    /// must all remain in the command's session.
    #[test]
    fn s07_inv029_every_cross_session_association_is_rejected() {
        let position = positions(2);
        let successor_work = interrupt(2, position[1], 1);
        let base = applied_facts(10, turn_id(1), successor_work);
        let mut predecessor = base.clone();
        predecessor.predecessor_session = session_id(200);
        let mut accepted_input = base.clone();
        accepted_input.accepted_input_session = session_id(200);
        let mut successor = base;
        successor.successor = AcceptedInputQueueWork::new(
            session_id(200),
            successor_work.turn(),
            successor_work.order(),
        );

        assert_cross_session_association_rejected(
            predecessor,
            InterruptSessionAssociation::Predecessor,
            &[ordinary(1, position[0])],
        );
        assert_cross_session_association_rejected(
            accepted_input,
            InterruptSessionAssociation::AcceptedInput,
            &[ordinary(1, position[0])],
        );
        assert_cross_session_association_rejected(
            successor,
            InterruptSessionAssociation::Successor,
            &[ordinary(1, position[0])],
        );
    }

    #[track_caller]
    fn assert_cross_session_association_rejected(
        facts: AppliedSubmitInputFacts,
        association: InterruptSessionAssociation,
        known_work: &[AcceptedInputQueueWork],
    ) {
        assert_eq!(
            correlate(facts, known_work),
            Err(AppliedInterruptConstructionError::SessionMismatch {
                association,
                command_session: session_id(100),
                associated_session: session_id(200),
            })
        );
    }

    /// S07 / INV-029: interrupt work must create a distinct successor turn.
    #[test]
    fn s07_inv029_predecessor_cannot_be_its_own_successor() {
        let position = positions(2);
        let mut facts = applied_facts(10, turn_id(1), interrupt(2, position[1], 1));
        facts.accepted_input = AcceptedInputLifecycle::new(
            accepted_input_id(10),
            AcceptedInputDisposition::OriginOf(turn_id(1)),
        );
        facts.successor = interrupt(1, position[1], 1);

        assert_eq!(
            correlate(facts, &[ordinary(1, position[0])]),
            Err(
                AppliedInterruptConstructionError::SuccessorMatchesPredecessor { turn: turn_id(1) }
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
        let position = positions(2);
        let successor = interrupt(2, position[1], 1);
        let mut facts = applied_facts(10, turn_id(1), successor);
        facts.accepted_input =
            AcceptedInputLifecycle::new(accepted_input_id(10), disposition.clone());

        assert_eq!(
            correlate(facts, &[ordinary(1, position[0])]),
            Err(
                AppliedInterruptConstructionError::AcceptedInputNotSuccessorOrigin {
                    disposition,
                    successor: successor.turn(),
                }
            )
        );
    }

    /// S07 / INV-029: the accepted position and typed successor priority must
    /// describe the same exact interrupt-created work.
    #[test]
    fn s07_inv029_cross_wired_position_or_priority_is_rejected() {
        let position = positions(3);
        let known_work = [ordinary(1, position[0])];
        let successor = interrupt(2, position[1], 1);
        let mut position_mismatch = applied_facts(10, turn_id(1), successor);
        position_mismatch.accepted_position = position[2];
        let mut ordinary_priority = applied_facts(10, turn_id(1), successor);
        ordinary_priority.successor = ordinary(2, position[1]);
        let mut wrong_target = applied_facts(10, turn_id(1), successor);
        wrong_target.successor = interrupt(2, position[1], 9);

        assert!(matches!(
            correlate(position_mismatch, &known_work),
            Err(AppliedInterruptConstructionError::AcceptedPositionMismatch { .. })
        ));
        assert_eq!(
            correlate(ordinary_priority, &known_work),
            Err(AppliedInterruptConstructionError::SuccessorHasOrdinaryPriority)
        );
        assert_eq!(
            correlate(wrong_target, &known_work),
            Err(
                AppliedInterruptConstructionError::SuccessorTargetsDifferentPredecessor {
                    expected: turn_id(1),
                    actual: turn_id(9),
                }
            )
        );
    }

    /// S07 / INV-009 / INV-029: the successor must be new and its target must
    /// exist in the complete pre-application queue projection.
    #[test]
    fn s07_inv009_inv029_preexisting_successor_or_missing_predecessor_is_rejected() {
        let position = positions(2);
        let no_known_work: [AcceptedInputQueueWork; 0] = [];
        let facts = applied_facts(10, turn_id(1), interrupt(2, position[1], 1));

        assert_eq!(
            correlate(
                facts.clone(),
                &[ordinary(1, position[0]), ordinary(2, position[1])]
            ),
            Err(AppliedInterruptConstructionError::SuccessorAlreadyKnown {
                successor: turn_id(2),
            })
        );
        assert_eq!(
            correlate(facts, &no_known_work),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::MissingInterruptPredecessor {
                    turn: turn_id(2),
                    predecessor: turn_id(1),
                },
            })
        );
    }

    /// S07 / INV-009 / INV-029: existing priority facts cannot already claim
    /// another immediate interrupt successor for the same predecessor.
    #[test]
    fn s07_inv009_inv029_competing_interrupt_successor_is_rejected() {
        let position = positions(3);

        assert_eq!(
            correlate(
                applied_facts(10, turn_id(1), interrupt(3, position[2], 1)),
                &[ordinary(1, position[0]), interrupt(2, position[1], 1)]
            ),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::MultipleInterruptSuccessors {
                    predecessor: turn_id(1),
                    first_successor: turn_id(2),
                    second_successor: turn_id(3),
                },
            })
        );
    }

    /// S07 / INV-009 / INV-029: priority cannot move an input ahead of a
    /// predecessor that was accepted later.
    #[test]
    fn s07_inv009_inv029_time_inverted_interrupt_successor_is_rejected() {
        let position = positions(2);

        assert_eq!(
            correlate(
                applied_facts(10, turn_id(1), interrupt(2, position[0], 1)),
                &[ordinary(1, position[1])]
            ),
            Err(AppliedInterruptConstructionError::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::InterruptPositionNotAfterPredecessor {
                    turn: turn_id(2),
                    predecessor: turn_id(1),
                    position: position[0],
                    predecessor_position: position[1],
                },
            })
        );
    }
}
