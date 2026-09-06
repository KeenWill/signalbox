//! Turn-lifecycle phase, ambiguity, reconciliation, and disposition values.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md is normative. This module
//! deliberately stops at value constructibility: authoritative eligibility
//! and terminal aggregate transitions require complete evidence boundaries
//! that are not yet implemented. The sealed fatal-mismatch binding can
//! construct a marker only from its exact derived ambiguity remainder and
//! causes, but that marker remains part of an uncommitted candidate.
//! Standalone values are not proof that aggregate guards hold.

use std::{collections::BTreeSet, num::NonZeroU32};

use crate::{
    AppliedInterruptProof, ChildWait, ContextFrontier, CurrentTurnAttempt, DurableCommandId,
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
/// S04 / S06: the private field also prevents
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
    pub(crate) fn singleton(operation: IssuedOperationRef) -> Self {
        Self {
            operations: BTreeSet::from([operation]),
        }
    }

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

/// Authority from one applied exact-set user decision to stop for
/// reconciliation.
///
/// S06: raw command and turn identities cannot construct
/// this proof:
///
/// ```compile_fail
/// use signalbox_domain::{AppliedStopForReconciliationProof, DurableCommandId, TurnId};
///
/// fn raw_ids_are_not_user_stop_authority(command: DurableCommandId, turn: TurnId) {
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
    /// Returns the applied user-decision command identity.
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
    /// The user applied an exact-set decision to stop.
    UserChoseReconciliation {
        /// Purpose-specific authority from the applied user decision.
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
    /// The daemon spent one durable bounded recovery attempt on an ambiguity.
    AutomaticRecovery {
        /// One-based attempt recorded before the terminalization transaction.
        attempt: NonZeroU32,
    },
}

/// Complete immutable evidence named by a reconciliation-required turn.
///
/// S04 / S06 / S07: fields remain
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
    /// Constructs an interrupt marker after the model-execution aggregate has
    /// proven the exact issued operation remains ambiguous.
    pub(crate) fn from_interrupt_ambiguity(
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        interrupt: AppliedInterruptProof,
    ) -> Self {
        Self {
            ambiguous_operations,
            reason: ReconciliationReason::InterruptRequiresReconciliation { interrupt },
        }
    }

    /// Constructs a daemon-owned marker after the execution aggregate proves
    /// the exact ambiguous operation and ended attempt still own the wait.
    pub(crate) fn from_automatic_recovery(
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        attempt: NonZeroU32,
    ) -> Self {
        Self {
            ambiguous_operations,
            reason: ReconciliationReason::AutomaticRecovery { attempt },
        }
    }

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
/// a current attempt cannot be omitted from `Running`:
///
/// ```compile_fail
/// use signalbox_domain::ActiveTurnPhase;
/// let _ = ActiveTurnPhase::Running;
/// ```
///
/// S10: nor can an approval wait carry an
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
    /// Orchestration waits on one exact foreground child result.
    AwaitingChild {
        /// The checked await request, spawning request, and child identity.
        wait: ChildWait,
    },
    /// Orchestration waits on an exact nonempty ambiguity set.
    AwaitingRecoveryDecision {
        /// The operations still blocking turn-level disposition.
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        /// The exact interrupt still stopping the turn, when one was applied.
        applied_interrupt: Option<crate::AppliedInterruptProof>,
    },
    /// Orchestration waits for replacement of one exact lost runner placement.
    AwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner: crate::RunnerId,
        /// Placement revision against which loss was projected.
        placement_revision: crate::RunnerGeneration,
        /// Physical tool attempt interrupted by loss, when one exists.
        optional_tool_attempt: Option<crate::ToolAttemptId>,
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
/// S07: cancellation cannot omit its purpose-specific
/// proof:
///
/// ```compile_fail
/// use signalbox_domain::TurnDisposition;
/// let _ = TurnDisposition::Cancelled;
/// ```
///
/// S04 / S06 / S07: reconciliation
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
    /// Unacknowledged physical ambiguity requires user reconciliation.
    ReconciliationRequired {
        /// The exact nonempty ambiguity set and typed reason.
        marker: ReconciliationMarker,
    },
    /// A queued turn that never activated was retired from the queue.
    ///
    /// It contributes no terminal frontier and is excluded from queue
    /// predecessor selection.
    Retired,
}

/// The mandatory typed reason one turn reached `terminal`.
///
/// The set is closed and every terminalization names exactly one member.
/// `UnclassifiedFailure` is the only catch-all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnTerminalCause {
    /// The turn produced its conversational outcome.
    Completed,
    /// The provider produced an explicit refusal.
    ModelRefusal,
    /// An applied interrupt ended the turn.
    InterruptApplied,
    /// Unacknowledged physical model-call ambiguity requires reconciliation.
    ModelCallAmbiguous,
    /// Unacknowledged physical tool-attempt ambiguity requires reconciliation.
    ToolAttemptAmbiguous,
    /// A model call the turn owned ended failed.
    ModelCallFailed,
    /// No resolved provider target admitted the call the turn needed.
    ModelTargetUnavailable,
    /// Attachment preparation could not produce the call's input.
    AttachmentPreparationFailed,
    /// Provider capability preparation reported a trustworthy local failure.
    CapabilityPreparationFailed,
    /// The turn already held the maximum admitted automatic tool rounds.
    ToolRoundLimitReached,
    /// A tool attempt the turn owned was lost with its executing process.
    ToolAttemptLost,
    /// Every member of the turn's credential pool was exhausted.
    CredentialPoolExhausted,
    /// A tool request needed an approval decision no attended surface could give.
    HeadlessApprovalEscalation,
    /// A restart found the turn's work with no live process owning it.
    AbandonedAtRestart,
    /// The liveness watchdog closed the turn on repeated staleness evidence.
    WatchdogStaleTurn,
    /// Reserved context headroom could not admit the turn's continuation.
    ContextHeadroomExhausted,
    /// Context compaction could not fit the input it was asked to compact.
    ContextCompactionWall,
    /// Context compaction failed for a reason other than an unfittable input.
    ContextCompactionFailed,
    /// The turn's bounded automatic compaction attempt was already spent.
    ReportedUsageContextCompactionExhausted,
    /// Automatic compaction did not restore reserved context headroom.
    ReportedUsageContextStillExceeded,
    /// Durable evidence supports failure and classifies no reason.
    UnclassifiedFailure,
    /// A queued goal turn became ineligible under its goal's lineage.
    GoalTurnIneligible,
    /// A queued turn retired by its session's closure.
    SessionClosed,
}

#[cfg(test)]
mod tests {
    use expect_test::expect;

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

    fn user_stop(value: u128) -> AppliedStopForReconciliationProof {
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

    /// baseline operation kinds remain tagged and distinct.
    #[test]
    fn issued_operation_reference_kinds_do_not_collapse() {
        let model = IssuedOperationRef::ModelCall(model_call_id(1));
        let tool = IssuedOperationRef::ToolAttempt(tool_attempt_id(1));

        assert_ne!(model, tool);
    }

    /// S04 / S06: empty and duplicate caller
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

    /// S04 / S06: valid reorderings construct
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

    /// S01 / S07 / S09: starting lineage remains a closed typed
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

    /// S04 / S06 / S10: every active phase
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
        let child_wait =
            ChildWait::from_checked_parts(tool_request_id(2), tool_request_id(3), session_id(1));
        let awaiting_child = ActiveTurnPhase::AwaitingChild { wait: child_wait };
        let awaiting_recovery = ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: ambiguous.clone(),
            applied_interrupt: None,
        };

        assert!(running.retains_progressing_slot());
        assert!(awaiting_approval.retains_progressing_slot());
        assert!(awaiting_child.retains_progressing_slot());
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
            &awaiting_child,
            ActiveTurnPhase::AwaitingChild { wait } if *wait == child_wait
        ));
        assert!(matches!(
            &awaiting_recovery,
            ActiveTurnPhase::AwaitingRecoveryDecision {
                ambiguous_operations,
                ..
            }
                if ambiguous_operations == &ambiguous
        ));
    }

    /// S04 / S06 / S07: every marker
    /// reason retains the exact canonical ambiguity set and typed authority.
    #[test]
    fn reconciliation_markers_preserve_exact_sets_and_reasons() {
        let ambiguous_operations = operations(&[1, 2]);
        assert_marker_preserves_set_and_reason(
            ambiguous_operations.clone(),
            ReconciliationReason::UserChoseReconciliation {
                decision: user_stop(1),
            },
        );
        assert_marker_preserves_set_and_reason(
            ambiguous_operations.clone(),
            ReconciliationReason::InterruptRequiresReconciliation {
                interrupt: interrupt(1),
            },
        );
        assert_marker_preserves_set_and_reason(
            ambiguous_operations.clone(),
            ReconciliationReason::FatalMismatchRequiresReconciliation {
                causes: fatal_causes(),
            },
        );
        assert_marker_preserves_set_and_reason(
            ambiguous_operations,
            ReconciliationReason::AutomaticRecovery {
                attempt: NonZeroU32::new(3).expect("the fixture attempt is nonzero"),
            },
        );
    }

    #[track_caller]
    fn assert_marker_preserves_set_and_reason(
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        reason: ReconciliationReason,
    ) {
        let marker = marker(ambiguous_operations.clone(), reason.clone());

        assert_eq!(marker.ambiguous_operations(), &ambiguous_operations);
        assert_eq!(marker.reason(), &reason);
    }

    /// S07: cancellation and reconciliation terminal
    /// values retain their exact proof-bearing payloads.
    #[test]
    fn terminal_dispositions_preserve_exact_payloads() {
        let expected_cause = interrupt(1);
        let cancelled = TurnDisposition::Cancelled {
            cause: expected_cause,
        };

        let expected = marker(
            operations(&[1, 2]),
            ReconciliationReason::InterruptRequiresReconciliation {
                interrupt: interrupt(1),
            },
        );
        let reconciliation = TurnDisposition::ReconciliationRequired { marker: expected };

        expect![[r#"
            (
                Cancelled {
                    cause: AppliedInterruptProof {
                        command: DurableCommandId(
                            00000000-0000-0000-0000-000000000001,
                        ),
                        predecessor: TurnId(
                            00000000-0000-0000-0000-000000000064,
                        ),
                    },
                },
                ReconciliationRequired {
                    marker: ReconciliationMarker {
                        ambiguous_operations: NonEmptyIssuedOperationRefs {
                            operations: {
                                ModelCall(
                                    ModelCallId(
                                        00000000-0000-0000-0000-000000000001,
                                    ),
                                ),
                                ModelCall(
                                    ModelCallId(
                                        00000000-0000-0000-0000-000000000002,
                                    ),
                                ),
                            },
                        },
                        reason: InterruptRequiresReconciliation {
                            interrupt: AppliedInterruptProof {
                                command: DurableCommandId(
                                    00000000-0000-0000-0000-000000000001,
                                ),
                                predecessor: TurnId(
                                    00000000-0000-0000-0000-000000000064,
                                ),
                            },
                        },
                    },
                },
            )
        "#]]
        .assert_debug_eq(&(cancelled, reconciliation));
    }

    /// the user-stop proof exposes only its exact applied
    /// command and turn while raw identities cannot construct it publicly.
    #[test]
    fn user_stop_proof_preserves_exact_identity() {
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
