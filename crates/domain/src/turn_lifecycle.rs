//! Turn-lifecycle phase, ambiguity, reconciliation, and disposition values.
//!
//! ADR-0004, ADR-0027, ADR-0030, and ADR-0031 are normative. This module
//! deliberately stops at value constructibility: authoritative eligibility
//! and terminal aggregate transitions require complete evidence boundaries
//! that are not yet implemented. The sealed fatal-mismatch binding can
//! construct a marker only from its exact derived ambiguity remainder and
//! causes, but that marker remains part of an uncommitted candidate.
//! Standalone values are not proof that aggregate guards hold.

use std::collections::BTreeSet;

use crate::{
    AppliedInterruptProof, ContextFrontier, CurrentTurnAttempt, DurableCommandId,
    FatalMismatchStopCauses, ModelCallId, ToolAttemptId, ToolRequestId, TurnId,
    fatal_mismatch::lifecycle::FatalMismatchReconciliationMarkerCandidate,
};

/// The immutable lineage category selected when accepted-input work starts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AcceptedInputStartingLineage {
    /// No earlier turn exists in this session's durable total order.
    FirstInSession,
    /// Start after the exact immediately preceding terminal turn.
    After {
        /// The predecessor fixed from durable total order at eligibility.
        immediate_predecessor: TurnId,
    },
}

/// The exact starting lineage and frontier fixed together for an
/// accepted-input-origin turn.
///
/// This value is intentionally opaque. The crate-private producer is consumed
/// only by checked scheduling reconstitution and live eligibility after they
/// derive both fields from complete queue, slot, ancestry, predecessor, and
/// semantic-entry facts.
///
/// Raw values are not an eligibility proof:
///
/// ```compile_fail
/// use signalbox_domain::{
///     AcceptedInputStartingLineage, AcceptedInputTurnStart, ContextFrontier,
/// };
///
/// fn raw_values_are_not_a_turn_start(
///     lineage: AcceptedInputStartingLineage,
///     frontier: ContextFrontier,
/// ) {
///     let _ = AcceptedInputTurnStart { lineage, frontier };
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AcceptedInputTurnStart {
    lineage: AcceptedInputStartingLineage,
    frontier: ContextFrontier,
}

impl AcceptedInputTurnStart {
    pub(crate) const fn from_validated_eligibility(
        lineage: AcceptedInputStartingLineage,
        frontier: ContextFrontier,
    ) -> Self {
        Self { lineage, frontier }
    }

    /// Returns the eligibility-selected starting lineage.
    pub const fn lineage(&self) -> AcceptedInputStartingLineage {
        self.lineage
    }

    /// Returns the exact immutable starting frontier fixed with the lineage.
    pub const fn frontier(&self) -> ContextFrontier {
        self.frontier
    }
}

/// One exact issued physical operation that remains ambiguous.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IssuedOperationRef {
    /// A provider interaction authorized by the hub.
    ModelCall(ModelCallId),
    /// A physical effort to execute one logical tool request.
    ToolAttempt(ToolAttemptId),
}

/// A canonical nonempty set of exact issued-operation references.
///
/// Empty or duplicate input is rejected by [`Self::try_from_operations`].
/// S04 / S06 / INV-006 / INV-025 / INV-026: the private field also prevents
/// bypassing that boundary:
///
/// ```compile_fail
/// use std::collections::BTreeSet;
/// use signalbox_domain::NonEmptyIssuedOperationRefs;
///
/// let _ = NonEmptyIssuedOperationRefs {
///     operations: BTreeSet::new(),
/// };
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonEmptyIssuedOperationRefs {
    operations: BTreeSet<IssuedOperationRef>,
}

impl NonEmptyIssuedOperationRefs {
    /// Canonicalizes distinct references and rejects empty or duplicate input.
    pub fn try_from_operations(
        operations: impl IntoIterator<Item = IssuedOperationRef>,
    ) -> Result<Self, NonEmptyIssuedOperationRefsError> {
        let mut canonical = BTreeSet::new();
        for operation in operations {
            if !canonical.insert(operation) {
                return Err(NonEmptyIssuedOperationRefsError::Duplicate { operation });
            }
        }
        if canonical.is_empty() {
            return Err(NonEmptyIssuedOperationRefsError::Empty);
        }
        Ok(Self {
            operations: canonical,
        })
    }

    /// Returns the number of exact references in this nonempty set.
    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    /// Returns whether this exact issued operation is present.
    pub fn contains(&self, operation: IssuedOperationRef) -> bool {
        self.operations.contains(&operation)
    }

    /// Iterates over every exact reference in this set.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = IssuedOperationRef> + '_ {
        self.operations.iter().copied()
    }
}

/// Reports why an ambiguity-reference set could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonEmptyIssuedOperationRefsError {
    /// No issued-operation reference was supplied.
    Empty,
    /// The same exact operation appeared more than once.
    Duplicate {
        /// The duplicated reference.
        operation: IssuedOperationRef,
    },
}

/// Authority from one applied exact-set owner decision to stop for
/// reconciliation.
///
/// S06 / INV-006 / INV-026: raw command and turn identities cannot construct
/// this proof:
///
/// ```compile_fail
/// use signalbox_domain::{AppliedStopForReconciliationProof, DurableCommandId, TurnId};
///
/// fn raw_ids_are_not_owner_stop_authority(command: DurableCommandId, turn: TurnId) {
///     let _ = AppliedStopForReconciliationProof {
///         decision_command: command,
///         turn,
///     };
/// }
/// ```
///
/// A later exact-set command-result slice supplies the trusted producer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppliedStopForReconciliationProof {
    decision_command: DurableCommandId,
    turn: TurnId,
}

impl AppliedStopForReconciliationProof {
    /// Returns the applied owner-decision command identity.
    pub const fn decision_command(&self) -> DurableCommandId {
        self.decision_command
    }

    /// Returns the exact turn named by the applied decision.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
}

#[cfg(test)]
pub(crate) const fn test_applied_stop_for_reconciliation_proof(
    decision_command: DurableCommandId,
    turn: TurnId,
) -> AppliedStopForReconciliationProof {
    AppliedStopForReconciliationProof {
        decision_command,
        turn,
    }
}

/// The typed reason an exact ambiguity set requires reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconciliationReason {
    /// The owner applied an exact-set decision to stop.
    OwnerChoseReconciliation {
        /// Purpose-specific authority from the applied owner decision.
        decision: AppliedStopForReconciliationProof,
    },
    /// An applied interrupt cannot honestly resolve remaining ambiguity.
    InterruptRequiresReconciliation {
        /// The exact interrupt authority for this predecessor.
        interrupt: AppliedInterruptProof,
    },
    /// Fatal mismatch dominates while ambiguity remains.
    FatalMismatchRequiresReconciliation {
        /// The complete fatal failures and retained interrupt state.
        causes: FatalMismatchStopCauses,
    },
}

/// Complete immutable evidence named by a reconciliation-required turn.
///
/// S04 / S06 / S07 / INV-006 / INV-025 / INV-026 / INV-029: fields remain
/// private because only the later aggregate can validate that the set is exact
/// and unacknowledged and that the reason matches its durable evidence:
///
/// ```compile_fail
/// use signalbox_domain::{NonEmptyIssuedOperationRefs, ReconciliationMarker, ReconciliationReason};
///
/// fn candidate_values_are_not_a_marker(
///     ambiguous_operations: NonEmptyIssuedOperationRefs,
///     reason: ReconciliationReason,
/// ) {
///     let _ = ReconciliationMarker {
///         ambiguous_operations,
///         reason,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationMarker {
    ambiguous_operations: NonEmptyIssuedOperationRefs,
    reason: ReconciliationReason,
}

impl ReconciliationMarker {
    /// Constructs the fatal marker from the sealed post-evidence binding.
    pub(crate) fn from_fatal_mismatch_candidate(
        candidate: FatalMismatchReconciliationMarkerCandidate,
    ) -> Self {
        let (ambiguous_operations, causes) = candidate.into_parts();
        Self {
            ambiguous_operations,
            reason: ReconciliationReason::FatalMismatchRequiresReconciliation { causes },
        }
    }

    /// Borrows the exact canonical nonempty ambiguity set.
    pub const fn ambiguous_operations(&self) -> &NonEmptyIssuedOperationRefs {
        &self.ambiguous_operations
    }

    /// Borrows the exact typed reconciliation reason.
    pub const fn reason(&self) -> &ReconciliationReason {
        &self.reason
    }
}

#[cfg(test)]
pub(crate) fn test_reconciliation_marker(
    ambiguous_operations: NonEmptyIssuedOperationRefs,
    reason: ReconciliationReason,
) -> ReconciliationMarker {
    ReconciliationMarker {
        ambiguous_operations,
        reason,
    }
}

/// One active phase; every value retains the session's progressing-turn slot.
///
/// Variant fields make a running phase own exactly one current attempt and
/// each wait own its exact subject with no optional attempt. S04 / S06 /
/// INV-006 / INV-009: a current attempt cannot be omitted from `Running`:
///
/// ```compile_fail
/// use signalbox_domain::ActiveTurnPhase;
/// let _ = ActiveTurnPhase::Running;
/// ```
///
/// S10 / INV-006 / INV-009 / INV-010: nor can an approval wait carry an
/// independent attempt:
///
/// ```compile_fail
/// use signalbox_domain::{ActiveTurnPhase, CurrentTurnAttempt, ToolRequestId};
///
/// fn wait_has_no_attempt(request: ToolRequestId, current_attempt: CurrentTurnAttempt) {
///     let _ = ActiveTurnPhase::AwaitingApproval {
///         request,
///         current_attempt,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActiveTurnPhase {
    /// Physical orchestration has one exact current attempt.
    Running {
        /// The sole nonterminal attempt owned by this phase.
        current_attempt: CurrentTurnAttempt,
    },
    /// Orchestration waits on one exact logical tool request.
    AwaitingApproval {
        /// The request whose approval dependency remains durable.
        request: ToolRequestId,
    },
    /// Orchestration waits on an exact nonempty ambiguity set.
    AwaitingRecoveryDecision {
        /// The operations still blocking turn-level disposition.
        ambiguous_operations: NonEmptyIssuedOperationRefs,
    },
}

impl ActiveTurnPhase {
    /// Returns true because every active phase retains the progressing slot.
    pub const fn retains_progressing_slot(&self) -> bool {
        true
    }
}

/// The immutable terminal classification carried by a turn.
///
/// S07 / INV-006 / INV-029: cancellation cannot omit its purpose-specific
/// proof:
///
/// ```compile_fail
/// use signalbox_domain::TurnDisposition;
/// let _ = TurnDisposition::Cancelled;
/// ```
///
/// S04 / S06 / S07 / INV-006 / INV-025 / INV-026 / INV-029: reconciliation
/// likewise cannot omit its complete marker:
///
/// ```compile_fail
/// use signalbox_domain::TurnDisposition;
/// let _ = TurnDisposition::ReconciliationRequired;
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnDisposition {
    /// The turn produced its conversational outcome.
    Completed,
    /// The turn produced an explicit refusal.
    Refused,
    /// Durable evidence supports failure.
    Failed,
    /// An applied interrupt and effect-specific evidence support cancellation.
    Cancelled {
        /// The exact applied interrupt authority for this turn.
        cause: AppliedInterruptProof,
    },
    /// Unacknowledged physical ambiguity requires owner reconciliation.
    ReconciliationRequired {
        /// The exact nonempty ambiguity set and typed reason.
        marker: ReconciliationMarker,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppliedInterruptState, ResolvedContextFrontierSnapshot, SemanticTranscriptEntryRef,
        applied_interrupt::test_applied_interrupt_proof,
        test_support::{
            command_id, context_frontier_id, model_call_id, provider_target_evidence_id,
            semantic_transcript_entry_id, session_id, tool_attempt_id, tool_request_id,
            turn_attempt_id, turn_id,
        },
        turn_attempt::test_fatal_mismatch_stop_causes,
    };

    fn operation(value: u128) -> IssuedOperationRef {
        IssuedOperationRef::ModelCall(model_call_id(value))
    }

    fn operations(values: &[u128]) -> NonEmptyIssuedOperationRefs {
        NonEmptyIssuedOperationRefs::try_from_operations(values.iter().copied().map(operation))
            .expect("test ambiguity sets are nonempty and distinct")
    }

    fn interrupt(value: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(value), turn_id(100))
    }

    fn owner_stop(value: u128) -> AppliedStopForReconciliationProof {
        AppliedStopForReconciliationProof {
            decision_command: command_id(value),
            turn: turn_id(100),
        }
    }

    fn fatal_causes() -> FatalMismatchStopCauses {
        test_fatal_mismatch_stop_causes(
            provider_target_evidence_id(1),
            AppliedInterruptState::Applied {
                proof: interrupt(1),
            },
        )
    }

    fn marker(
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        reason: ReconciliationReason,
    ) -> ReconciliationMarker {
        ReconciliationMarker {
            ambiguous_operations,
            reason,
        }
    }

    /// INV-025 / INV-026: baseline operation kinds remain tagged and distinct.
    #[test]
    fn issued_operation_reference_kinds_do_not_collapse() {
        let model = IssuedOperationRef::ModelCall(model_call_id(1));
        let tool = IssuedOperationRef::ToolAttempt(tool_attempt_id(1));

        assert_ne!(model, tool);
    }

    /// S04 / S06 / INV-006 / INV-025 / INV-026: empty and duplicate caller
    /// collections cannot construct the canonical ambiguity set.
    #[test]
    fn ambiguity_set_rejects_empty_and_duplicate_input() {
        assert_eq!(
            NonEmptyIssuedOperationRefs::try_from_operations([]),
            Err(NonEmptyIssuedOperationRefsError::Empty)
        );
        assert_eq!(
            NonEmptyIssuedOperationRefs::try_from_operations([operation(1), operation(1)]),
            Err(NonEmptyIssuedOperationRefsError::Duplicate {
                operation: operation(1),
            })
        );
    }

    /// S04 / S06 / INV-006 / INV-025 / INV-026: valid reorderings construct
    /// equal canonical sets and preserve every exact reference.
    #[test]
    fn ambiguity_set_is_canonical_and_exact() {
        let forward = operations(&[1, 2, 3]);
        let reordered = operations(&[3, 1, 2]);
        let mixed_forward = NonEmptyIssuedOperationRefs::try_from_operations([
            IssuedOperationRef::ModelCall(model_call_id(1)),
            IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
        ])
        .expect("mixed operation references are distinct");
        let mixed_reordered = NonEmptyIssuedOperationRefs::try_from_operations([
            IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
            IssuedOperationRef::ModelCall(model_call_id(1)),
        ])
        .expect("mixed operation references are distinct");

        assert_eq!(forward, reordered);
        assert_eq!(mixed_forward, mixed_reordered);
        assert_eq!(forward.operation_count(), 3);
        assert!(forward.contains(operation(2)));
        assert!(mixed_forward.contains(IssuedOperationRef::ModelCall(model_call_id(1))));
        assert!(mixed_forward.contains(IssuedOperationRef::ToolAttempt(tool_attempt_id(1))));
        assert_eq!(
            forward.iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([operation(1), operation(2), operation(3)])
        );
    }

    /// S01 / S07 / S09 / INV-009: starting lineage remains a closed typed
    /// algebra independently of frontier construction authority.
    #[test]
    fn starting_lineage_distinguishes_first_and_exact_predecessor() {
        let predecessor = turn_id(1);
        let after = AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor,
        };

        assert_ne!(AcceptedInputStartingLineage::FirstInSession, after);
        assert_ne!(
            after,
            AcceptedInputStartingLineage::After {
                immediate_predecessor: turn_id(2),
            }
        );
        assert!(matches!(
            after,
            AcceptedInputStartingLineage::After {
                immediate_predecessor
            } if immediate_predecessor == predecessor
        ));
    }

    /// S01 / S09: the opaque start value retains the exact lineage/frontier
    /// pair, but its module-private construction does not claim the later
    /// eligibility transition is implemented.
    #[test]
    fn s01_s09_turn_start_shape_couples_lineage_and_exact_frontier() {
        let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(1),
            context_frontier_id(1),
            vec![SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(1),
            )],
        )
        .expect("test snapshot entries are ordered and distinct");
        let predecessor = turn_id(1);
        let start = AcceptedInputTurnStart {
            lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor,
            },
            frontier: snapshot.frontier(),
        };

        assert!(matches!(
            start.lineage(),
            AcceptedInputStartingLineage::After {
                immediate_predecessor
            } if immediate_predecessor == predecessor
        ));
        assert_eq!(start.frontier(), snapshot.frontier());
    }

    /// S04 / S06 / S10 / INV-006 / INV-009 / INV-010: every active phase
    /// retains the slot and structurally carries exactly its required subject.
    #[test]
    fn active_phases_retain_slot_with_exact_subjects() {
        let attempt_id = turn_attempt_id(1);
        let request_id = tool_request_id(1);
        let ambiguous = operations(&[1]);
        let running = ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(attempt_id),
        };
        let awaiting_approval = ActiveTurnPhase::AwaitingApproval {
            request: request_id,
        };
        let awaiting_recovery = ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: ambiguous.clone(),
        };

        assert!(running.retains_progressing_slot());
        assert!(awaiting_approval.retains_progressing_slot());
        assert!(awaiting_recovery.retains_progressing_slot());
        assert!(matches!(
            &running,
            ActiveTurnPhase::Running { current_attempt }
                if current_attempt.id() == attempt_id
        ));
        assert!(matches!(
            &awaiting_approval,
            ActiveTurnPhase::AwaitingApproval { request } if *request == request_id
        ));
        assert!(matches!(
            &awaiting_recovery,
            ActiveTurnPhase::AwaitingRecoveryDecision { ambiguous_operations }
                if ambiguous_operations == &ambiguous
        ));
    }

    /// S04 / S06 / S07 / INV-006 / INV-025 / INV-026 / INV-029: every marker
    /// reason retains the exact canonical ambiguity set and typed authority.
    #[test]
    fn reconciliation_markers_preserve_exact_sets_and_reasons() {
        assert_marker_preserves_set_and_reason(ReconciliationReason::OwnerChoseReconciliation {
            decision: owner_stop(1),
        });
        assert_marker_preserves_set_and_reason(
            ReconciliationReason::InterruptRequiresReconciliation {
                interrupt: interrupt(1),
            },
        );
        assert_marker_preserves_set_and_reason(
            ReconciliationReason::FatalMismatchRequiresReconciliation {
                causes: fatal_causes(),
            },
        );
    }

    #[track_caller]
    fn assert_marker_preserves_set_and_reason(reason: ReconciliationReason) {
        let ambiguous_operations = operations(&[1, 2]);
        let marker = marker(ambiguous_operations.clone(), reason.clone());

        assert_eq!(marker.ambiguous_operations(), &ambiguous_operations);
        assert_eq!(marker.reason(), &reason);
    }

    /// S07 / INV-006 / INV-029: cancellation and reconciliation terminal
    /// values retain their exact proof-bearing payloads.
    #[test]
    fn terminal_dispositions_preserve_exact_payloads() {
        assert!(matches!(
            TurnDisposition::Completed,
            TurnDisposition::Completed
        ));
        assert!(matches!(TurnDisposition::Refused, TurnDisposition::Refused));
        assert!(matches!(TurnDisposition::Failed, TurnDisposition::Failed));

        let expected_cause = interrupt(1);
        let cancelled = TurnDisposition::Cancelled {
            cause: expected_cause,
        };
        assert!(matches!(
            cancelled,
            TurnDisposition::Cancelled { cause } if cause == expected_cause
        ));

        let expected = marker(
            operations(&[1, 2]),
            ReconciliationReason::InterruptRequiresReconciliation {
                interrupt: interrupt(1),
            },
        );
        let reconciliation = TurnDisposition::ReconciliationRequired {
            marker: expected.clone(),
        };
        assert!(matches!(
            reconciliation,
            TurnDisposition::ReconciliationRequired { marker } if marker == expected
        ));
    }

    /// INV-006 / INV-026: the owner-stop proof exposes only its exact applied
    /// command and turn while raw identities cannot construct it publicly.
    #[test]
    fn owner_stop_proof_preserves_exact_identity() {
        let decision_command = command_id(1);
        let turn = turn_id(100);
        let proof = AppliedStopForReconciliationProof {
            decision_command,
            turn,
        };

        assert_eq!(proof.decision_command(), decision_command);
        assert_eq!(proof.turn(), turn);
    }
}
