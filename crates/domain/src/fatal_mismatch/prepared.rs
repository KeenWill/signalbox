//! Atomic-only lifecycle candidate for live fatal mismatch during `Prepared`.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md permits no `StopRequested`
//! or reconciliation branch from a prepared attempt. This boundary therefore
//! couples exact completed-call invalidation facts to one direct known-failure
//! candidate while retaining every logical dependency the later aggregate must
//! close in the same commit.

use std::collections::BTreeSet;

use crate::{
    ActiveTurnPhase, CurrentTurnAttemptState, EndedTurnAttempt, FatalMismatchStopDisposition,
    TurnDisposition, turn_attempt::CurrentTurnAttemptTransitionError,
};

use super::{
    FatalMismatchOwnedWorkBlocker, OwnedLogicalDependencyRef, PostEvidenceFatalMismatchFacts,
};

/// One prepared attempt end that is valid only as part of the complete atomic
/// aggregate transition.
///
/// Unlike the Running/StopRequested candidate, this type has no fatal-stop
/// fallback. The required logical closures, ended attempt, failed turn,
/// steering changes, and slot release must all commit or none may commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedFatalMismatchAtomicCandidate {
    ended_attempt: EndedTurnAttempt,
    turn_disposition: TurnDisposition,
    required_logical_closures: BTreeSet<OwnedLogicalDependencyRef>,
}

impl PreparedFatalMismatchAtomicCandidate {
    pub(crate) const fn ended_attempt(&self) -> &EndedTurnAttempt {
        &self.ended_attempt
    }

    pub(crate) const fn turn_disposition(&self) -> &TurnDisposition {
        &self.turn_disposition
    }

    /// Returns every exact logical dependency that must become terminally
    /// non-dispatchable in the candidate's eventual aggregate transaction.
    pub(crate) fn required_logical_closures(
        &self,
    ) -> impl ExactSizeIterator<Item = OwnedLogicalDependencyRef> + DoubleEndedIterator + '_ {
        self.required_logical_closures.iter().copied()
    }
}

/// A sealed prepared-source binding retaining its derivation inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedFatalMismatchAtomicBinding {
    facts: PostEvidenceFatalMismatchFacts,
    source_phase: ActiveTurnPhase,
    candidate: PreparedFatalMismatchAtomicCandidate,
}

impl PreparedFatalMismatchAtomicBinding {
    pub(crate) const fn facts(&self) -> &PostEvidenceFatalMismatchFacts {
        &self.facts
    }

    pub(crate) const fn source_phase(&self) -> &ActiveTurnPhase {
        &self.source_phase
    }

    pub(crate) const fn candidate(&self) -> &PreparedFatalMismatchAtomicCandidate {
        &self.candidate
    }
}

/// Why prepared fatal facts could not form an atomic-only candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedFatalMismatchBindingRejection {
    /// The supplied source phase is a durable wait rather than `Running`.
    SourcePhaseIsNotRunning,
    /// The supplied phase does not own the exact projected current attempt.
    SourceAttemptMismatch,
    /// The projected current attempt is not `Prepared`.
    SourceAttemptIsNotPrepared,
    /// An unclassified operation or blocking ambiguity requires a transition
    /// that `Prepared` does not possess.
    PhysicalClosureIsIncomplete,
    /// The exact prepared attempt unexpectedly rejected known-failure end.
    AttemptEndRejected {
        /// The unchanged attempt and rejected local end transition.
        error: CurrentTurnAttemptTransitionError,
    },
}

/// A rejected prepared binding retaining its exact facts and supplied phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedFatalMismatchBindingError {
    rejected: Box<(
        PostEvidenceFatalMismatchFacts,
        ActiveTurnPhase,
        PreparedFatalMismatchBindingRejection,
    )>,
}

impl PreparedFatalMismatchBindingError {
    fn new(
        facts: PostEvidenceFatalMismatchFacts,
        source_phase: ActiveTurnPhase,
        rejection: PreparedFatalMismatchBindingRejection,
    ) -> Self {
        Self {
            rejected: Box::new((facts, source_phase, rejection)),
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        PostEvidenceFatalMismatchFacts,
        ActiveTurnPhase,
        PreparedFatalMismatchBindingRejection,
    ) {
        *self.rejected
    }
}

impl PostEvidenceFatalMismatchFacts {
    /// Binds a prepared completed-call invalidation to its atomic-only terminal
    /// candidate without claiming that the aggregate transaction committed.
    pub(crate) fn bind_prepared_atomic_candidate(
        self,
        source_phase: ActiveTurnPhase,
    ) -> Result<PreparedFatalMismatchAtomicBinding, PreparedFatalMismatchBindingError> {
        let source_attempt = match &source_phase {
            ActiveTurnPhase::Running { current_attempt } => current_attempt.clone(),
            ActiveTurnPhase::AwaitingApproval { .. }
            | ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
                return Err(PreparedFatalMismatchBindingError::new(
                    self,
                    source_phase,
                    PreparedFatalMismatchBindingRejection::SourcePhaseIsNotRunning,
                ));
            }
        };
        if source_attempt != *self.current_attempt() {
            return Err(PreparedFatalMismatchBindingError::new(
                self,
                source_phase,
                PreparedFatalMismatchBindingRejection::SourceAttemptMismatch,
            ));
        }
        if source_attempt.state() != &CurrentTurnAttemptState::Prepared {
            return Err(PreparedFatalMismatchBindingError::new(
                self,
                source_phase,
                PreparedFatalMismatchBindingRejection::SourceAttemptIsNotPrepared,
            ));
        }

        let mut required_logical_closures = BTreeSet::new();
        for blocker in &self.unfinished_blockers {
            match blocker {
                FatalMismatchOwnedWorkBlocker::LogicalDependency(dependency) => {
                    required_logical_closures.insert(*dependency);
                }
                FatalMismatchOwnedWorkBlocker::UnclassifiedOperation(_) => {
                    return Err(PreparedFatalMismatchBindingError::new(
                        self,
                        source_phase,
                        PreparedFatalMismatchBindingRejection::PhysicalClosureIsIncomplete,
                    ));
                }
            }
        }
        if self.blocking_ambiguities.is_some() {
            return Err(PreparedFatalMismatchBindingError::new(
                self,
                source_phase,
                PreparedFatalMismatchBindingRejection::PhysicalClosureIsIncomplete,
            ));
        }

        let ended_attempt = match source_attempt.end_after_fatal_mismatch(
            self.causes().clone(),
            FatalMismatchStopDisposition::KnownFailure,
        ) {
            Ok(ended_attempt) => ended_attempt,
            Err(error) => {
                return Err(PreparedFatalMismatchBindingError::new(
                    self,
                    source_phase,
                    PreparedFatalMismatchBindingRejection::AttemptEndRejected { error },
                ));
            }
        };
        Ok(PreparedFatalMismatchAtomicBinding {
            facts: self,
            source_phase,
            candidate: PreparedFatalMismatchAtomicCandidate {
                ended_attempt,
                turn_disposition: TurnDisposition::Failed,
                required_logical_closures,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::{
        AppliedInterruptState, AttemptEnd, CurrentTurnAttempt, IssuedOperationRef,
        NonEmptyIssuedOperationRefs, ProviderTargetMismatchFailureRef,
        applied_interrupt::test_applied_interrupt_proof,
        fatal_mismatch::{
            AmbiguousOperationTurnTreatment, CompleteFatalMismatchProjection,
            IssuedOperationClosure, LogicalDependencyClosure,
        },
        provider_evidence::AppliedProviderTargetMismatch,
        test_support::{
            command_id, model_call_id, provider_target_evidence_id, tool_attempt_id,
            tool_request_id, turn_attempt_id, turn_id,
        },
    };

    fn projection(
        attempt: CurrentTurnAttempt,
        dependencies: impl IntoIterator<Item = (OwnedLogicalDependencyRef, LogicalDependencyClosure)>,
        operations: impl IntoIterator<Item = (IssuedOperationRef, IssuedOperationClosure)>,
    ) -> CompleteFatalMismatchProjection {
        CompleteFatalMismatchProjection::new(
            attempt,
            dependencies.into_iter().collect::<BTreeMap<_, _>>(),
            operations.into_iter().collect::<BTreeMap<_, _>>(),
        )
    }

    /// The one open tool request retained by [`prepared_facts`].
    fn open_request() -> OwnedLogicalDependencyRef {
        OwnedLogicalDependencyRef::ToolRequest(tool_request_id(1))
    }

    /// The one open approval dependency retained by [`prepared_facts`].
    fn open_approval() -> OwnedLogicalDependencyRef {
        OwnedLogicalDependencyRef::Approval(tool_request_id(2))
    }

    /// The one already-closed dependency retained by [`prepared_facts`].
    fn closed_request() -> OwnedLogicalDependencyRef {
        OwnedLogicalDependencyRef::ToolRequest(tool_request_id(3))
    }

    fn prepared_facts(
        extra_operations: impl IntoIterator<Item = (IssuedOperationRef, IssuedOperationClosure)>,
    ) -> PostEvidenceFatalMismatchFacts {
        let invalidated_call = model_call_id(1);
        let mut operations = BTreeMap::from([(
            IssuedOperationRef::ModelCall(invalidated_call),
            IssuedOperationClosure::ClassifiedNonAmbiguous,
        )]);
        operations.extend(extra_operations);
        CompleteFatalMismatchProjection::new(
            CurrentTurnAttempt::prepared(turn_attempt_id(1)),
            BTreeMap::from([
                (open_request(), LogicalDependencyClosure::Open),
                (open_approval(), LogicalDependencyClosure::Open),
                (
                    closed_request(),
                    LogicalDependencyClosure::TerminallyNonDispatchable,
                ),
            ]),
            operations,
        )
        .apply(AppliedProviderTargetMismatch::test_completed_invalidation(
            invalidated_call,
        ))
        .expect("prepared completed-call invalidation derives sealed facts")
    }

    /// S21 / INV-006 / INV-014: Prepared ends directly as exact known failure
    /// while every open logical dependency remains an atomic closure requirement.
    #[test]
    fn s21_inv006_inv014_prepared_invalidation_binds_atomic_failure_only() {
        let facts = prepared_facts([
            (
                IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
                IssuedOperationClosure::PhysicallyAmbiguous {
                    turn_treatment: AmbiguousOperationTurnTreatment::ResolvedByEvidence,
                },
            ),
            (
                IssuedOperationRef::ToolAttempt(tool_attempt_id(2)),
                IssuedOperationClosure::PhysicallyAmbiguous {
                    turn_treatment: AmbiguousOperationTurnTreatment::DuplicateRiskAccepted,
                },
            ),
        ]);
        let expected_facts = facts.clone();
        let source_phase = ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(1)),
        };
        let expected_source = source_phase.clone();
        let binding = facts
            .bind_prepared_atomic_candidate(source_phase)
            .expect("exact prepared source binds");

        assert_eq!(binding.facts(), &expected_facts);
        assert_eq!(binding.source_phase(), &expected_source);
        assert_eq!(
            binding
                .candidate()
                .required_logical_closures()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([open_request(), open_approval()])
        );
        assert_eq!(
            binding.candidate().turn_disposition(),
            &TurnDisposition::Failed
        );
        assert!(matches!(
            binding.candidate().ended_attempt().end(),
            AttemptEnd::AfterFatalMismatch {
                causes,
                disposition: FatalMismatchStopDisposition::KnownFailure,
            } if causes == expected_facts.causes()
                && causes.interrupt() == AppliedInterruptState::NoAppliedInterrupt
                && causes.failures().count() == 1
                && causes.contains(expected_facts.applied_mismatch().failure())
        ));
    }

    /// S21 / INV-006 / INV-014: unclassified work or blocking ambiguity rejects
    /// the atomic-only path with exact facts and source phase unchanged.
    #[test]
    fn s21_inv006_inv014_incomplete_prepared_physical_closure_rejects_unchanged() {
        assert_incomplete_physical_closure_rejects_unchanged(IssuedOperationClosure::Unclassified);
        assert_incomplete_physical_closure_rejects_unchanged(
            IssuedOperationClosure::PhysicallyAmbiguous {
                turn_treatment: AmbiguousOperationTurnTreatment::Blocking,
            },
        );
    }

    #[track_caller]
    fn assert_incomplete_physical_closure_rejects_unchanged(open_closure: IssuedOperationClosure) {
        let facts = prepared_facts([(
            IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
            open_closure,
        )]);
        let unchanged_facts = facts.clone();
        let source_phase = ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(1)),
        };
        let unchanged_source = source_phase.clone();
        let error = facts
            .bind_prepared_atomic_candidate(source_phase)
            .expect_err("Prepared has no stop or reconciliation branch");
        assert_eq!(
            error.into_parts(),
            (
                unchanged_facts,
                unchanged_source,
                PreparedFatalMismatchBindingRejection::PhysicalClosureIsIncomplete,
            )
        );
    }

    /// INV-006: phase shape, exact attempt correlation, and Prepared state all
    /// reject without consuming either supplied input.
    #[test]
    fn inv006_prepared_binding_source_rejections_preserve_inputs() {
        let running = CurrentTurnAttempt::prepared(turn_attempt_id(1))
            .begin_running()
            .expect("test attempt can run");
        let cancellation_stopped = running
            .clone()
            .request_cancellation(test_applied_interrupt_proof(command_id(1), turn_id(1)))
            .expect("running attempt accepts test interrupt");
        let fatal_stopped = running
            .clone()
            .request_fatal_mismatch(
                ProviderTargetMismatchFailureRef::nonterminal_call_observation(
                    provider_target_evidence_id(2),
                ),
            )
            .expect("running attempt accepts test fatal cause");
        let facts_for = |attempt: CurrentTurnAttempt| {
            projection(
                attempt,
                [],
                [(
                    IssuedOperationRef::ModelCall(model_call_id(1)),
                    IssuedOperationClosure::ClassifiedNonAmbiguous,
                )],
            )
            .apply(AppliedProviderTargetMismatch::test_completed_invalidation(
                model_call_id(1),
            ))
            .expect("completed-call invalidation derives sealed facts")
        };
        assert_prepared_binding_rejects(
            prepared_facts([]),
            ActiveTurnPhase::AwaitingApproval {
                request: tool_request_id(1),
            },
            PreparedFatalMismatchBindingRejection::SourcePhaseIsNotRunning,
        );
        assert_prepared_binding_rejects(
            prepared_facts([]),
            ActiveTurnPhase::AwaitingRecoveryDecision {
                ambiguous_operations: NonEmptyIssuedOperationRefs::try_from_operations([
                    IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
                ])
                .expect("one operation is nonempty"),
                applied_interrupt: None,
            },
            PreparedFatalMismatchBindingRejection::SourcePhaseIsNotRunning,
        );
        assert_prepared_binding_rejects(
            prepared_facts([]),
            ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(2)),
            },
            PreparedFatalMismatchBindingRejection::SourceAttemptMismatch,
        );
        assert_prepared_binding_rejects(
            prepared_facts([]),
            ActiveTurnPhase::Running {
                current_attempt: running.clone(),
            },
            PreparedFatalMismatchBindingRejection::SourceAttemptMismatch,
        );
        assert_prepared_binding_rejects(
            facts_for(running.clone()),
            ActiveTurnPhase::Running {
                current_attempt: running,
            },
            PreparedFatalMismatchBindingRejection::SourceAttemptIsNotPrepared,
        );
        assert_prepared_binding_rejects(
            facts_for(cancellation_stopped.clone()),
            ActiveTurnPhase::Running {
                current_attempt: cancellation_stopped,
            },
            PreparedFatalMismatchBindingRejection::SourceAttemptIsNotPrepared,
        );
        assert_prepared_binding_rejects(
            facts_for(fatal_stopped.clone()),
            ActiveTurnPhase::Running {
                current_attempt: fatal_stopped,
            },
            PreparedFatalMismatchBindingRejection::SourceAttemptIsNotPrepared,
        );
    }

    #[track_caller]
    fn assert_prepared_binding_rejects(
        facts: PostEvidenceFatalMismatchFacts,
        source: ActiveTurnPhase,
        rejection: PreparedFatalMismatchBindingRejection,
    ) {
        let unchanged_facts = facts.clone();
        let unchanged_source = source.clone();
        let error = facts
            .bind_prepared_atomic_candidate(source)
            .expect_err("invalid prepared-binding source rejects");
        assert_eq!(
            error.into_parts(),
            (unchanged_facts, unchanged_source, rejection)
        );
    }
}
