//! Sealed live attempt/turn candidates derived from fatal-mismatch facts.
//!
//! The caller-supplied active phase supplies component-level correlation only:
//! it must be `Running` and own the exact projected current attempt. It is not
//! canonical turn, session, progressing-slot, or commit authority.
//!
//! A closed result remains a candidate. Canonical call mutation, durable
//! cancellation intent, complete pending-steering reclassification, aggregate
//! freshness, slot release, and atomic persistence remain later boundaries.
//! Re-deriving a terminal candidate after work closes on an already persisted
//! fatal stop also remains a separate later seam.

use crate::{
    ActiveTurnPhase, CurrentTurnAttempt, CurrentTurnAttemptState, EndedTurnAttempt,
    FatalMismatchStopCauses, FatalMismatchStopDisposition, NonEmptyIssuedOperationRefs,
    ReconciliationMarker, TurnAttemptStopCauses, TurnDisposition,
    turn_attempt::CurrentTurnAttemptTransitionError,
};

use super::PostEvidenceFatalMismatchFacts;

/// Exact fatal evidence from which the lifecycle binding may construct a
/// reconciliation marker.
///
/// Fields and construction remain private to this module, so sibling domain
/// code cannot pair an arbitrary ambiguity set with unrelated fatal causes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FatalMismatchReconciliationMarkerCandidate {
    ambiguous_operations: NonEmptyIssuedOperationRefs,
    causes: FatalMismatchStopCauses,
}

impl FatalMismatchReconciliationMarkerCandidate {
    fn new(
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        causes: FatalMismatchStopCauses,
    ) -> Self {
        Self {
            ambiguous_operations,
            causes,
        }
    }

    pub(crate) fn into_parts(self) -> (NonEmptyIssuedOperationRefs, FatalMismatchStopCauses) {
        (self.ambiguous_operations, self.causes)
    }
}

/// An ended-attempt and turn-disposition candidate coupled from the same
/// sealed post-evidence facts.
///
/// This is not a committed terminal turn. Pending steering reclassification,
/// slot release, canonical record mutation, and persistence serialization
/// remain the later aggregate boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FatalMismatchClosedTerminalCandidate {
    ended_attempt: EndedTurnAttempt,
    turn_disposition: TurnDisposition,
    fatal_stop_fallback: ActiveTurnPhase,
}

impl FatalMismatchClosedTerminalCandidate {
    pub(crate) const fn ended_attempt(&self) -> &EndedTurnAttempt {
        &self.ended_attempt
    }

    pub(crate) const fn turn_disposition(&self) -> &TurnDisposition {
        &self.turn_disposition
    }

    /// Returns the exact fatal stop that must survive if later aggregate
    /// terminalization guards reject the closed candidate.
    pub(crate) const fn fatal_stop_fallback(&self) -> &ActiveTurnPhase {
        &self.fatal_stop_fallback
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FatalMismatchLifecycleOutcome {
    StopRequested { active_phase: ActiveTurnPhase },
    ClosedTerminalCandidate(FatalMismatchClosedTerminalCandidate),
}

/// Read-only view of one sealed fatal-mismatch lifecycle binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FatalMismatchLifecycleBindingView<'a> {
    /// Owned work remains unfinished and the exact attempt is fatal-stopped.
    StopRequested {
        /// The active running phase carrying the same complete fatal causes.
        active_phase: &'a ActiveTurnPhase,
    },
    /// Owned work is closed and attempt/turn terminal values are coupled, but
    /// the remaining aggregate and commit boundaries are not yet proven.
    ClosedTerminalCandidate {
        /// The paired ended-attempt and turn-disposition values.
        candidate: &'a FatalMismatchClosedTerminalCandidate,
    },
}

/// A sealed attempt/turn lifecycle binding derived from post-evidence facts.
///
/// The binding owns its source facts so later aggregate code can validate and
/// commit canonical call, dependency, steering, and slot state together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FatalMismatchLifecycleBinding {
    facts: PostEvidenceFatalMismatchFacts,
    source_phase: ActiveTurnPhase,
    outcome: FatalMismatchLifecycleOutcome,
}

impl FatalMismatchLifecycleBinding {
    pub(crate) const fn facts(&self) -> &PostEvidenceFatalMismatchFacts {
        &self.facts
    }

    /// Returns the locally correlated source phase, not aggregate authority.
    pub(crate) const fn source_phase(&self) -> &ActiveTurnPhase {
        &self.source_phase
    }

    pub(crate) const fn view(&self) -> FatalMismatchLifecycleBindingView<'_> {
        match &self.outcome {
            FatalMismatchLifecycleOutcome::StopRequested { active_phase } => {
                FatalMismatchLifecycleBindingView::StopRequested { active_phase }
            }
            FatalMismatchLifecycleOutcome::ClosedTerminalCandidate(candidate) => {
                FatalMismatchLifecycleBindingView::ClosedTerminalCandidate { candidate }
            }
        }
    }
}

/// Why sealed post-evidence facts could not bind to local lifecycle values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FatalMismatchLifecycleBindingRejection {
    /// The supplied source phase is a durable wait rather than `Running`.
    SourcePhaseIsNotRunning,
    /// Component-level correlation found a phase that does not own the exact
    /// projected current attempt.
    SourceAttemptMismatch,
    /// The projected current attempt unexpectedly rejected the exact mismatch.
    AttemptRejected {
        /// The unchanged attempt and exact rejected local transition.
        error: CurrentTurnAttemptTransitionError,
    },
    /// The local attempt transition did not reproduce the already-derived F.
    DerivedFatalCausesMismatch {
        /// The unexpected fatal causes carried by the stopped attempt.
        actual: FatalMismatchStopCauses,
    },
    /// The local transition unexpectedly failed to establish fatal stop.
    FatalStopNotEstablished {
        /// The unexpected post-transition attempt.
        actual: CurrentTurnAttempt,
    },
    /// The selected source attempt rejected its exact matching terminal end.
    AttemptEndRejected {
        /// The unchanged attempt and exact rejected local end transition.
        error: CurrentTurnAttemptTransitionError,
    },
}

/// Rejected lifecycle binding with the exact unchanged post-evidence facts and
/// source phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FatalMismatchLifecycleBindingError {
    rejected: Box<(
        PostEvidenceFatalMismatchFacts,
        ActiveTurnPhase,
        FatalMismatchLifecycleBindingRejection,
    )>,
}

impl FatalMismatchLifecycleBindingError {
    fn new(
        facts: PostEvidenceFatalMismatchFacts,
        source_phase: ActiveTurnPhase,
        rejection: FatalMismatchLifecycleBindingRejection,
    ) -> Self {
        Self {
            rejected: Box::new((facts, source_phase, rejection)),
        }
    }

    pub(crate) const fn facts(&self) -> &PostEvidenceFatalMismatchFacts {
        &self.rejected.0
    }

    pub(crate) const fn source_phase(&self) -> &ActiveTurnPhase {
        &self.rejected.1
    }

    pub(crate) const fn rejection(&self) -> &FatalMismatchLifecycleBindingRejection {
        &self.rejected.2
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        PostEvidenceFatalMismatchFacts,
        ActiveTurnPhase,
        FatalMismatchLifecycleBindingRejection,
    ) {
        *self.rejected
    }
}

impl PostEvidenceFatalMismatchFacts {
    /// Binds sealed F/owned-work/U facts to local attempt and turn values.
    ///
    /// The source phase is checked only for local shape and exact-attempt
    /// correlation. A terminal result remains only a candidate for the later
    /// aggregate's steering, slot, canonical-record, and atomic-commit guards.
    pub(crate) fn bind_lifecycle_candidate(
        self,
        source_phase: ActiveTurnPhase,
    ) -> Result<FatalMismatchLifecycleBinding, FatalMismatchLifecycleBindingError> {
        let source_attempt = match &source_phase {
            ActiveTurnPhase::Running { current_attempt } => current_attempt.clone(),
            ActiveTurnPhase::AwaitingApproval { .. }
            | ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
                return Err(FatalMismatchLifecycleBindingError::new(
                    self,
                    source_phase,
                    FatalMismatchLifecycleBindingRejection::SourcePhaseIsNotRunning,
                ));
            }
        };
        if source_attempt != *self.current_attempt() {
            return Err(FatalMismatchLifecycleBindingError::new(
                self,
                source_phase,
                FatalMismatchLifecycleBindingRejection::SourceAttemptMismatch,
            ));
        }
        let stopped_attempt = match self
            .current_attempt()
            .clone()
            .request_fatal_mismatch(self.applied_mismatch().failure())
        {
            Ok(attempt) => attempt,
            Err(error) => {
                return Err(FatalMismatchLifecycleBindingError::new(
                    self,
                    source_phase,
                    FatalMismatchLifecycleBindingRejection::AttemptRejected { error },
                ));
            }
        };

        let CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(actual_causes),
        } = stopped_attempt.state()
        else {
            return Err(FatalMismatchLifecycleBindingError::new(
                self,
                source_phase,
                FatalMismatchLifecycleBindingRejection::FatalStopNotEstablished {
                    actual: stopped_attempt,
                },
            ));
        };
        if actual_causes != self.causes() {
            return Err(FatalMismatchLifecycleBindingError::new(
                self,
                source_phase,
                FatalMismatchLifecycleBindingRejection::DerivedFatalCausesMismatch {
                    actual: actual_causes.clone(),
                },
            ));
        }

        let terminal_source_attempt = match source_attempt.state() {
            CurrentTurnAttemptState::Running => source_attempt.clone(),
            CurrentTurnAttemptState::StopRequested { .. } => stopped_attempt.clone(),
            CurrentTurnAttemptState::Prepared => {
                return Err(FatalMismatchLifecycleBindingError::new(
                    self,
                    source_phase,
                    FatalMismatchLifecycleBindingRejection::SourceAttemptMismatch,
                ));
            }
        };
        let fatal_stop_fallback = ActiveTurnPhase::Running {
            current_attempt: stopped_attempt.clone(),
        };

        let outcome = match (
            self.unfinished_blockers.is_empty(),
            &self.blocking_ambiguities,
        ) {
            (false, _) => FatalMismatchLifecycleOutcome::StopRequested {
                active_phase: fatal_stop_fallback,
            },
            (true, None) => {
                let ended_attempt = match terminal_source_attempt.end_after_fatal_mismatch(
                    self.causes().clone(),
                    FatalMismatchStopDisposition::KnownFailure,
                ) {
                    Ok(ended_attempt) => ended_attempt,
                    Err(error) => {
                        return Err(FatalMismatchLifecycleBindingError::new(
                            self,
                            source_phase,
                            FatalMismatchLifecycleBindingRejection::AttemptEndRejected { error },
                        ));
                    }
                };
                FatalMismatchLifecycleOutcome::ClosedTerminalCandidate(
                    FatalMismatchClosedTerminalCandidate {
                        ended_attempt,
                        turn_disposition: TurnDisposition::Failed,
                        fatal_stop_fallback,
                    },
                )
            }
            (true, Some(blocking_ambiguities)) => {
                let ended_attempt = match terminal_source_attempt.end_after_fatal_mismatch(
                    self.causes().clone(),
                    FatalMismatchStopDisposition::Ambiguous,
                ) {
                    Ok(ended_attempt) => ended_attempt,
                    Err(error) => {
                        return Err(FatalMismatchLifecycleBindingError::new(
                            self,
                            source_phase,
                            FatalMismatchLifecycleBindingRejection::AttemptEndRejected { error },
                        ));
                    }
                };
                let marker = ReconciliationMarker::from_fatal_mismatch_candidate(
                    FatalMismatchReconciliationMarkerCandidate::new(
                        blocking_ambiguities.clone(),
                        self.causes().clone(),
                    ),
                );
                FatalMismatchLifecycleOutcome::ClosedTerminalCandidate(
                    FatalMismatchClosedTerminalCandidate {
                        ended_attempt,
                        turn_disposition: TurnDisposition::ReconciliationRequired { marker },
                        fatal_stop_fallback,
                    },
                )
            }
        };

        Ok(FatalMismatchLifecycleBinding {
            facts: self,
            source_phase,
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::super::{
        AmbiguousOperationTurnTreatment, CompleteFatalMismatchProjection,
        FatalMismatchOwnedWorkBlocker, IssuedOperationClosure, LogicalDependencyClosure,
        OwnedLogicalDependencyRef,
    };
    use super::*;
    use crate::{
        AppliedInterruptProof, AppliedInterruptState, AttemptEnd, IssuedOperationRef, ModelCallId,
        ProviderTargetMismatchFailureRef, ReconciliationReason,
        applied_interrupt::test_applied_interrupt_proof,
        provider_evidence::AppliedProviderTargetMismatch,
        test_support::{
            command_id, model_call_id, provider_target_evidence_id, tool_attempt_id,
            tool_request_id, turn_attempt_id, turn_id,
        },
    };

    fn running_attempt(value: u128) -> CurrentTurnAttempt {
        CurrentTurnAttempt::prepared(turn_attempt_id(value))
            .begin_running()
            .expect("prepared test attempt can run")
    }

    fn interrupt(value: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(value), turn_id(100))
    }

    fn mismatch(affected_call: ModelCallId, evidence: u128) -> AppliedProviderTargetMismatch {
        AppliedProviderTargetMismatch::test_nonterminal(
            provider_target_evidence_id(evidence),
            affected_call,
        )
    }

    fn failure(evidence: u128) -> ProviderTargetMismatchFailureRef {
        ProviderTargetMismatchFailureRef::nonterminal_call_observation(provider_target_evidence_id(
            evidence,
        ))
    }

    /// One fatal-mismatch source attempt and the complete interrupt state and
    /// failure set its binding must derive after the evidence-1 mismatch.
    struct FatalSource {
        attempt: CurrentTurnAttempt,
        expected_interrupt: AppliedInterruptState,
        expected_failures: BTreeSet<ProviderTargetMismatchFailureRef>,
    }

    /// A live running source with no stop; binding derives the new failure
    /// alone.
    fn live_running_source() -> FatalSource {
        FatalSource {
            attempt: running_attempt(1),
            expected_interrupt: AppliedInterruptState::NoAppliedInterrupt,
            expected_failures: BTreeSet::from([failure(1)]),
        }
    }

    /// A source with a pre-existing cancellation-only stop; binding retains
    /// the exact proof alongside the new failure.
    fn cancellation_stopped_source() -> FatalSource {
        let proof = interrupt(1);
        FatalSource {
            attempt: running_attempt(2)
                .request_cancellation(proof)
                .expect("running attempt accepts applied interrupt"),
            expected_interrupt: AppliedInterruptState::Applied { proof },
            expected_failures: BTreeSet::from([failure(1)]),
        }
    }

    /// A source with an existing multi-failure fatal stop; binding unions the
    /// new failure into the complete cause set.
    fn fatal_stopped_source() -> FatalSource {
        let proof = interrupt(1);
        let prior_failure = failure(2);
        FatalSource {
            attempt: running_attempt(3)
                .request_cancellation(proof)
                .expect("running attempt accepts applied interrupt")
                .request_fatal_mismatch(prior_failure)
                .expect("cancellation stop upgrades to fatal"),
            expected_interrupt: AppliedInterruptState::Applied { proof },
            expected_failures: BTreeSet::from([failure(1), prior_failure]),
        }
    }

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

    fn blocking_ambiguity() -> IssuedOperationClosure {
        IssuedOperationClosure::PhysicallyAmbiguous {
            turn_treatment: AmbiguousOperationTurnTreatment::Blocking,
        }
    }

    fn fatal_causes(phase: &ActiveTurnPhase) -> &FatalMismatchStopCauses {
        let ActiveTurnPhase::Running { current_attempt } = phase else {
            panic!("fatal lifecycle output remains an active running phase");
        };
        let CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(causes),
        } = current_attempt.state()
        else {
            panic!("fatal lifecycle output carries the exact fatal stop");
        };
        causes
    }

    /// S27 / INV-006 / INV-029: unfinished owned work yields only the exact
    /// fatal-stopped attempt, retaining the same source facts and identity.
    #[test]
    fn s27_inv006_inv029_unfinished_work_yields_exact_fatal_stop() {
        assert_unfinished_work_yields_exact_fatal_stop(live_running_source());
        assert_unfinished_work_yields_exact_fatal_stop(cancellation_stopped_source());
        assert_unfinished_work_yields_exact_fatal_stop(fatal_stopped_source());
    }

    #[track_caller]
    fn assert_unfinished_work_yields_exact_fatal_stop(source: FatalSource) {
        let FatalSource {
            attempt,
            expected_interrupt,
            expected_failures,
        } = source;
        let owned_call = model_call_id(1);
        let open_request = OwnedLogicalDependencyRef::ToolRequest(tool_request_id(1));
        let unclassified = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let ambiguous = IssuedOperationRef::ToolAttempt(tool_attempt_id(3));
        let attempt_id = attempt.id();
        let facts = projection(
            attempt.clone(),
            [(open_request, LogicalDependencyClosure::Open)],
            [
                (
                    IssuedOperationRef::ModelCall(owned_call),
                    IssuedOperationClosure::Unclassified,
                ),
                (unclassified, IssuedOperationClosure::Unclassified),
                (ambiguous, blocking_ambiguity()),
            ],
        )
        .apply(mismatch(owned_call, 1))
        .expect("owned mismatch produces complete facts");
        let expected_facts = facts.clone();
        let source_phase = ActiveTurnPhase::Running {
            current_attempt: attempt,
        };
        let expected_source = source_phase.clone();
        let binding = facts
            .bind_lifecycle_candidate(source_phase)
            .expect("exact running phase binds");

        assert_eq!(binding.facts(), &expected_facts);
        assert_eq!(binding.source_phase(), &expected_source);
        assert_eq!(
            binding
                .facts()
                .unfinished_blockers()
                .expect("unfinished binding retains exact blockers")
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                FatalMismatchOwnedWorkBlocker::LogicalDependency(open_request),
                FatalMismatchOwnedWorkBlocker::UnclassifiedOperation(unclassified),
            ])
        );
        assert!(
            binding
                .facts()
                .blocking_ambiguities()
                .is_some_and(|remainder| remainder.contains(ambiguous))
        );
        let FatalMismatchLifecycleBindingView::StopRequested { active_phase } = binding.view()
        else {
            panic!("unfinished work cannot select a terminal candidate");
        };
        let ActiveTurnPhase::Running { current_attempt } = active_phase else {
            panic!("stop candidate remains the running active phase");
        };
        assert_eq!(current_attempt.id(), attempt_id);
        assert_eq!(fatal_causes(active_phase), binding.facts().causes());
        assert_eq!(binding.facts().causes().interrupt(), expected_interrupt);
        assert_eq!(
            binding.facts().causes().failures().collect::<BTreeSet<_>>(),
            expected_failures
        );
    }

    /// S07 / S27 / INV-006 / INV-029: a live running source, a pre-existing
    /// cancellation-only stop, and an existing multi-failure fatal stop all
    /// couple closed work to exact known failure and retain the same fatal
    /// stop as aggregate fallback.
    #[test]
    fn s07_s27_inv006_inv029_closed_sources_yield_exact_failure_candidates() {
        assert_closed_source_yields_exact_failure_candidate(live_running_source());
        assert_closed_source_yields_exact_failure_candidate(cancellation_stopped_source());
        assert_closed_source_yields_exact_failure_candidate(fatal_stopped_source());
    }

    #[track_caller]
    fn assert_closed_source_yields_exact_failure_candidate(source: FatalSource) {
        let FatalSource {
            attempt,
            expected_interrupt,
            expected_failures,
        } = source;
        let owned_call = model_call_id(1);
        let attempt_id = attempt.id();
        let facts = projection(
            attempt.clone(),
            [],
            [(
                IssuedOperationRef::ModelCall(owned_call),
                IssuedOperationClosure::Unclassified,
            )],
        )
        .apply(mismatch(owned_call, 1))
        .expect("last unclassified operation becomes known failed");
        let expected_causes = facts.causes().clone();

        let binding = facts
            .bind_lifecycle_candidate(ActiveTurnPhase::Running {
                current_attempt: attempt,
            })
            .expect("exact source attempt binds through its running phase");
        let FatalMismatchLifecycleBindingView::ClosedTerminalCandidate { candidate } =
            binding.view()
        else {
            panic!("closed work selects a terminal candidate");
        };

        assert_eq!(candidate.ended_attempt().id(), attempt_id);
        assert!(matches!(
            candidate.ended_attempt().end(),
            AttemptEnd::AfterFatalMismatch {
                causes,
                disposition: FatalMismatchStopDisposition::KnownFailure,
            } if causes == &expected_causes
        ));
        assert_eq!(candidate.turn_disposition(), &TurnDisposition::Failed);
        assert_eq!(
            fatal_causes(candidate.fatal_stop_fallback()),
            &expected_causes
        );
        let ActiveTurnPhase::Running {
            current_attempt: fallback_attempt,
        } = candidate.fatal_stop_fallback()
        else {
            panic!("fallback remains the running active phase");
        };
        assert_eq!(fallback_attempt.id(), attempt_id);
        assert_eq!(expected_causes.interrupt(), expected_interrupt);
        assert_eq!(
            expected_causes.failures().collect::<BTreeSet<_>>(),
            expected_failures
        );
    }

    /// S07 / S27 / INV-006 / INV-025 / INV-026 / INV-029: a live running
    /// source, a cancellation-only stop, and an existing multi-failure fatal
    /// stop close as ambiguous while attempt history, marker reason, and
    /// fallback all carry the same exact F and the marker carries exactly U.
    #[test]
    fn s07_s27_inv006_inv025_inv026_inv029_closed_sources_yield_exact_reconciliation_candidates() {
        assert_closed_source_yields_exact_reconciliation_candidate(live_running_source());
        assert_closed_source_yields_exact_reconciliation_candidate(cancellation_stopped_source());
        assert_closed_source_yields_exact_reconciliation_candidate(fatal_stopped_source());
    }

    #[track_caller]
    fn assert_closed_source_yields_exact_reconciliation_candidate(source: FatalSource) {
        let FatalSource {
            attempt,
            expected_interrupt,
            expected_failures,
        } = source;
        let owned_call = model_call_id(1);
        let y = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let attempt_id = attempt.id();
        let facts = projection(
            attempt.clone(),
            [],
            [
                (
                    IssuedOperationRef::ModelCall(owned_call),
                    IssuedOperationClosure::Unclassified,
                ),
                (y, blocking_ambiguity()),
            ],
        )
        .apply(mismatch(owned_call, 1))
        .expect("known mismatch closes X while Y remains ambiguous");
        let expected_causes = facts.causes().clone();

        let binding = facts
            .bind_lifecycle_candidate(ActiveTurnPhase::Running {
                current_attempt: attempt,
            })
            .expect("exact source attempt binds through its running phase");
        let FatalMismatchLifecycleBindingView::ClosedTerminalCandidate { candidate } =
            binding.view()
        else {
            panic!("closed ambiguity selects a terminal candidate");
        };

        assert_eq!(candidate.ended_attempt().id(), attempt_id);
        assert!(matches!(
            candidate.ended_attempt().end(),
            AttemptEnd::AfterFatalMismatch {
                causes,
                disposition: FatalMismatchStopDisposition::Ambiguous,
            } if causes == &expected_causes
        ));
        let TurnDisposition::ReconciliationRequired { marker } = candidate.turn_disposition()
        else {
            panic!("nonempty exact U requires reconciliation");
        };
        assert_eq!(marker.ambiguous_operations().operation_count(), 1);
        assert!(marker.ambiguous_operations().contains(y));
        assert!(matches!(
            marker.reason(),
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes }
                if causes == &expected_causes
        ));
        assert_eq!(
            fatal_causes(candidate.fatal_stop_fallback()),
            &expected_causes
        );
        let ActiveTurnPhase::Running {
            current_attempt: fallback_attempt,
        } = candidate.fatal_stop_fallback()
        else {
            panic!("fallback remains the running active phase");
        };
        assert_eq!(fallback_attempt.id(), attempt_id);
        assert_eq!(binding.facts().causes(), &expected_causes);
        assert_eq!(expected_causes.interrupt(), expected_interrupt);
        assert_eq!(
            expected_causes.failures().collect::<BTreeSet<_>>(),
            expected_failures
        );
    }

    /// INV-006: local phase-shape and exact-attempt correlation reject with
    /// the original facts and supplied phase unchanged; neither check claims
    /// aggregate freshness, current-turn ownership, or progressing-slot proof.
    #[test]
    fn inv006_phase_correlation_rejections_return_inputs_unchanged() {
        assert_non_running_phase_rejects_unchanged(ActiveTurnPhase::AwaitingApproval {
            request: tool_request_id(1),
        });
        assert_non_running_phase_rejects_unchanged(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: NonEmptyIssuedOperationRefs::try_from_operations([
                IssuedOperationRef::ToolAttempt(tool_attempt_id(1)),
            ])
            .expect("one operation is nonempty"),
        });

        let different_identity = running_attempt(2);
        let same_identity_not_running = CurrentTurnAttempt::prepared(turn_attempt_id(1));
        let same_identity_already_stopped = running_attempt(1)
            .request_cancellation(interrupt(1))
            .expect("same-identity running attempt accepts interrupt");
        assert_mismatched_running_phase_rejects_unchanged(different_identity);
        assert_mismatched_running_phase_rejects_unchanged(same_identity_not_running);
        assert_mismatched_running_phase_rejects_unchanged(same_identity_already_stopped);
    }

    /// Closed post-evidence facts whose exact correlated attempt is
    /// `running_attempt(1)`.
    fn correlated_facts() -> PostEvidenceFatalMismatchFacts {
        let owned_call = model_call_id(1);
        projection(
            running_attempt(1),
            [],
            [(
                IssuedOperationRef::ModelCall(owned_call),
                IssuedOperationClosure::Unclassified,
            )],
        )
        .apply(mismatch(owned_call, 1))
        .expect("closed projection produces facts")
    }

    #[track_caller]
    fn assert_non_running_phase_rejects_unchanged(wait: ActiveTurnPhase) {
        let facts = correlated_facts();
        let unchanged_facts = facts.clone();
        let unchanged_wait = wait.clone();
        let error = facts
            .bind_lifecycle_candidate(wait)
            .expect_err("durable wait is not the local running phase");
        assert_eq!(error.facts(), &unchanged_facts);
        assert_eq!(error.source_phase(), &unchanged_wait);
        assert_eq!(
            error.rejection(),
            &FatalMismatchLifecycleBindingRejection::SourcePhaseIsNotRunning
        );
        assert_eq!(
            error.into_parts(),
            (
                unchanged_facts,
                unchanged_wait,
                FatalMismatchLifecycleBindingRejection::SourcePhaseIsNotRunning,
            )
        );
    }

    #[track_caller]
    fn assert_mismatched_running_phase_rejects_unchanged(wrong_attempt: CurrentTurnAttempt) {
        let facts = correlated_facts();
        let unchanged_facts = facts.clone();
        let wrong_phase = ActiveTurnPhase::Running {
            current_attempt: wrong_attempt,
        };
        let unchanged_wrong_phase = wrong_phase.clone();
        let error = facts
            .bind_lifecycle_candidate(wrong_phase)
            .expect_err("different identity or state fails local correlation");
        assert_eq!(
            error.into_parts(),
            (
                unchanged_facts,
                unchanged_wrong_phase,
                FatalMismatchLifecycleBindingRejection::SourceAttemptMismatch,
            )
        );
    }
}
