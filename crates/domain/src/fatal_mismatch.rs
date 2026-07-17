//! Sealed post-evidence facts for fatal-mismatch closure.
//!
//! ADR-0031 is normative. This module consumes one trusted mismatch fact and a
//! complete projection of the current attempt's owned logical dependencies and
//! issued operations. It derives the complete fatal causes, exact unfinished
//! owned work, and the exact blocking-ambiguity remainder without accepting a
//! caller-selected cause set, ambiguity set, or disposition.
//! The result is not commit authority. Binding it to attempt and turn
//! transitions, proving the remaining ADR-0004 terminal guards, reclassifying
//! steering, and committing atomically remain the next aggregate slice.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the next stacked aggregate slice supplies the trusted projection producer"
    )
)]

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AppliedInterruptState, CurrentTurnAttempt, CurrentTurnAttemptState, FatalMismatchStopCauses,
    IssuedOperationRef, ModelCallId, NonEmptyIssuedOperationRefs, ToolRequestId,
    TurnAttemptStopCauses,
    provider_evidence::{AppliedProviderTargetMismatch, ProviderTargetMismatchEffectView},
};

/// One exact logical dependency whose closure is an ADR-0004 terminal guard.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum OwnedLogicalDependencyRef {
    /// One logical tool request owned by the turn.
    ToolRequest(ToolRequestId),
    /// The approval dependency for one exact tool request.
    Approval(ToolRequestId),
}

/// Whether one owned logical dependency is closed against later dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalDependencyClosure {
    /// The dependency still needs closure or a terminal outcome.
    Open,
    /// The dependency is terminally non-dispatchable.
    TerminallyNonDispatchable,
}

/// What supplies turn-level treatment for a physically ambiguous operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AmbiguousOperationTurnTreatment {
    /// No evidence or acknowledgement supplies a turn-level disposition.
    Blocking,
    /// Separate resolving evidence supplies a turn-level disposition.
    ResolvedByEvidence,
    /// The owner durably accepted duplicate risk for this exact operation.
    DuplicateRiskAccepted,
}

/// Post-evidence classification of one issued physical operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IssuedOperationClosure {
    /// No honest physical classification is durable yet.
    Unclassified,
    /// A non-ambiguous terminal physical classification is durable.
    ClassifiedNonAmbiguous,
    /// The physical record remains immutable `Ambiguous`.
    PhysicallyAmbiguous {
        /// Whether that known uncertainty still blocks the turn.
        turn_treatment: AmbiguousOperationTurnTreatment,
    },
}

/// One exact fact that keeps owned work from being closed.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FatalMismatchOwnedWorkBlocker {
    /// An owned logical dependency remains open.
    LogicalDependency(OwnedLogicalDependencyRef),
    /// An issued operation remains physically unclassified.
    UnclassifiedOperation(IssuedOperationRef),
}

/// The derived state of the attempt's complete owned-work projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FatalMismatchOwnedWorkStatus {
    /// At least one exact dependency or operation remains unfinished.
    Unfinished,
    /// Owned work is closed and no blocking ambiguity remains.
    ClosedWithoutBlockingAmbiguity,
    /// Owned work is closed with an exact nonempty blocking remainder.
    ClosedWithBlockingAmbiguity,
}

/// Complete fatal causes and owned-work facts after one trusted mismatch.
///
/// This is deliberately not re-exported from the crate. The application layer
/// cannot construct or use it as proof that a transition committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PostEvidenceFatalMismatchFacts {
    projection: CompleteFatalMismatchProjection,
    causes: FatalMismatchStopCauses,
    unfinished_blockers: BTreeSet<FatalMismatchOwnedWorkBlocker>,
    blocking_ambiguities: Option<NonEmptyIssuedOperationRefs>,
}

impl PostEvidenceFatalMismatchFacts {
    pub(crate) const fn current_attempt(&self) -> &CurrentTurnAttempt {
        &self.projection.current_attempt
    }

    pub(crate) const fn projection(&self) -> &CompleteFatalMismatchProjection {
        &self.projection
    }

    pub(crate) const fn causes(&self) -> &FatalMismatchStopCauses {
        &self.causes
    }

    pub(crate) fn owned_work_status(&self) -> FatalMismatchOwnedWorkStatus {
        if !self.unfinished_blockers.is_empty() {
            FatalMismatchOwnedWorkStatus::Unfinished
        } else if self.blocking_ambiguities.is_some() {
            FatalMismatchOwnedWorkStatus::ClosedWithBlockingAmbiguity
        } else {
            FatalMismatchOwnedWorkStatus::ClosedWithoutBlockingAmbiguity
        }
    }

    pub(crate) fn unfinished_blockers(
        &self,
    ) -> Option<
        impl ExactSizeIterator<Item = FatalMismatchOwnedWorkBlocker> + DoubleEndedIterator + '_,
    > {
        if self.unfinished_blockers.is_empty() {
            None
        } else {
            Some(self.unfinished_blockers.iter().copied())
        }
    }

    pub(crate) const fn blocking_ambiguities(&self) -> Option<&NonEmptyIssuedOperationRefs> {
        self.blocking_ambiguities.as_ref()
    }
}

/// A sealed complete projection before the new mismatch fact takes effect.
///
/// Production construction arrives with the turn aggregate. The test-only
/// constructor keeps this slice from exposing a free-standing way to assert
/// that these maps contain every owned fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompleteFatalMismatchProjection {
    current_attempt: CurrentTurnAttempt,
    dependencies: BTreeMap<OwnedLogicalDependencyRef, LogicalDependencyClosure>,
    operations: BTreeMap<IssuedOperationRef, IssuedOperationClosure>,
}

impl CompleteFatalMismatchProjection {
    #[cfg(test)]
    fn new(
        current_attempt: CurrentTurnAttempt,
        dependencies: BTreeMap<OwnedLogicalDependencyRef, LogicalDependencyClosure>,
        operations: BTreeMap<IssuedOperationRef, IssuedOperationClosure>,
    ) -> Self {
        Self {
            current_attempt,
            dependencies,
            operations,
        }
    }

    /// Applies one trusted mismatch and derives complete post-evidence facts.
    pub(crate) fn apply(
        mut self,
        fact: AppliedProviderTargetMismatch,
    ) -> Result<PostEvidenceFatalMismatchFacts, FatalMismatchProjectionError> {
        let causes = match self.current_attempt.state() {
            CurrentTurnAttemptState::Prepared => {
                return Err(FatalMismatchProjectionError::new(
                    self,
                    fact,
                    FatalMismatchProjectionRejection::AttemptIsPrepared,
                ));
            }
            CurrentTurnAttemptState::Running => FatalMismatchStopCauses::new(
                fact.failure(),
                AppliedInterruptState::NoAppliedInterrupt,
            ),
            CurrentTurnAttemptState::StopRequested { causes } => match causes {
                TurnAttemptStopCauses::CancellationOnly { interrupt } => {
                    FatalMismatchStopCauses::new(
                        fact.failure(),
                        AppliedInterruptState::Applied { proof: *interrupt },
                    )
                }
                TurnAttemptStopCauses::FatalMismatch(causes) => {
                    causes.clone().with_failure(fact.failure())
                }
            },
        };

        let operation = IssuedOperationRef::ModelCall(fact.affected_call());
        let Some(current) = self.operations.get(&operation).copied() else {
            return Err(FatalMismatchProjectionError::new(
                self,
                fact,
                FatalMismatchProjectionRejection::AffectedCallIsNotOwned {
                    call: fact.affected_call(),
                },
            ));
        };
        let updated = match (fact.effect(), current) {
            (
                ProviderTargetMismatchEffectView::ClassifyNonterminalKnownFailed,
                IssuedOperationClosure::Unclassified,
            ) => IssuedOperationClosure::ClassifiedNonAmbiguous,
            (
                ProviderTargetMismatchEffectView::ResolveTerminalAmbiguity,
                IssuedOperationClosure::PhysicallyAmbiguous {
                    turn_treatment:
                        AmbiguousOperationTurnTreatment::Blocking
                        | AmbiguousOperationTurnTreatment::ResolvedByEvidence,
                },
            ) => IssuedOperationClosure::PhysicallyAmbiguous {
                turn_treatment: AmbiguousOperationTurnTreatment::ResolvedByEvidence,
            },
            (
                ProviderTargetMismatchEffectView::PreserveCompletedInvalidation,
                IssuedOperationClosure::ClassifiedNonAmbiguous,
            ) => IssuedOperationClosure::ClassifiedNonAmbiguous,
            (effect, current) => {
                return Err(FatalMismatchProjectionError::new(
                    self,
                    fact,
                    FatalMismatchProjectionRejection::AffectedCallStateMismatch {
                        call: fact.affected_call(),
                        effect,
                        current,
                    },
                ));
            }
        };
        self.operations.insert(operation, updated);

        let mut blockers = BTreeSet::new();
        for (dependency, closure) in &self.dependencies {
            if *closure == LogicalDependencyClosure::Open {
                blockers.insert(FatalMismatchOwnedWorkBlocker::LogicalDependency(
                    *dependency,
                ));
            }
        }

        let mut blocking_ambiguities = BTreeSet::new();
        for (operation, closure) in &self.operations {
            match closure {
                IssuedOperationClosure::Unclassified => {
                    blockers.insert(FatalMismatchOwnedWorkBlocker::UnclassifiedOperation(
                        *operation,
                    ));
                }
                IssuedOperationClosure::PhysicallyAmbiguous {
                    turn_treatment: AmbiguousOperationTurnTreatment::Blocking,
                } => {
                    blocking_ambiguities.insert(*operation);
                }
                IssuedOperationClosure::ClassifiedNonAmbiguous
                | IssuedOperationClosure::PhysicallyAmbiguous {
                    turn_treatment:
                        AmbiguousOperationTurnTreatment::ResolvedByEvidence
                        | AmbiguousOperationTurnTreatment::DuplicateRiskAccepted,
                } => {}
            }
        }

        let blocking_ambiguities =
            NonEmptyIssuedOperationRefs::try_from_operations(blocking_ambiguities).ok();

        Ok(PostEvidenceFatalMismatchFacts {
            projection: self,
            causes,
            unfinished_blockers: blockers,
            blocking_ambiguities,
        })
    }
}

/// Why a sealed projection rejected one trusted mismatch fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FatalMismatchProjectionRejection {
    /// ADR-0031's live closure rule does not apply to an unsent attempt.
    AttemptIsPrepared,
    /// The affected call is absent from the complete owned-operation map.
    AffectedCallIsNotOwned {
        /// The cross-wired or incomplete call identity.
        call: ModelCallId,
    },
    /// The affected call's projected physical state contradicts the fact.
    AffectedCallStateMismatch {
        /// The affected call identity.
        call: crate::ModelCallId,
        /// The already-validated effect the fact requires.
        effect: ProviderTargetMismatchEffectView,
        /// The contradictory current operation fact.
        current: IssuedOperationClosure,
    },
}

/// A rejected application with the unchanged projection and exact fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FatalMismatchProjectionError {
    rejected: Box<(
        CompleteFatalMismatchProjection,
        AppliedProviderTargetMismatch,
        FatalMismatchProjectionRejection,
    )>,
}

impl FatalMismatchProjectionError {
    fn new(
        projection: CompleteFatalMismatchProjection,
        fact: AppliedProviderTargetMismatch,
        rejection: FatalMismatchProjectionRejection,
    ) -> Self {
        Self {
            rejected: Box::new((projection, fact, rejection)),
        }
    }

    pub(crate) const fn projection(&self) -> &CompleteFatalMismatchProjection {
        &self.rejected.0
    }

    pub(crate) const fn fact(&self) -> AppliedProviderTargetMismatch {
        self.rejected.1
    }

    pub(crate) const fn rejection(&self) -> FatalMismatchProjectionRejection {
        self.rejected.2
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CompleteFatalMismatchProjection,
        AppliedProviderTargetMismatch,
        FatalMismatchProjectionRejection,
    ) {
        *self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppliedInterruptProof, ProviderTargetMismatchFailureRef,
        applied_interrupt::test_applied_interrupt_proof,
        test_support::{
            command_id, model_call_id, provider_target_evidence_id, tool_attempt_id,
            tool_request_id, turn_attempt_id, turn_id,
        },
    };

    fn failure(value: u128) -> ProviderTargetMismatchFailureRef {
        ProviderTargetMismatchFailureRef::nonterminal_call_observation(provider_target_evidence_id(
            value,
        ))
    }

    fn interrupt(value: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(value), turn_id(100))
    }

    fn running_attempt() -> CurrentTurnAttempt {
        CurrentTurnAttempt::prepared(turn_attempt_id(1))
            .begin_running()
            .expect("prepared test attempt can run")
    }

    fn projection(
        attempt: CurrentTurnAttempt,
        dependencies: impl IntoIterator<Item = (OwnedLogicalDependencyRef, LogicalDependencyClosure)>,
        operations: impl IntoIterator<Item = (IssuedOperationRef, IssuedOperationClosure)>,
    ) -> CompleteFatalMismatchProjection {
        CompleteFatalMismatchProjection::new(
            attempt,
            dependencies.into_iter().collect(),
            operations.into_iter().collect(),
        )
    }

    fn nonterminal_fact(call: u128, evidence: u128) -> AppliedProviderTargetMismatch {
        AppliedProviderTargetMismatch::test_nonterminal(
            provider_target_evidence_id(evidence),
            model_call_id(call),
        )
    }

    fn blocking_ambiguity() -> IssuedOperationClosure {
        IssuedOperationClosure::PhysicallyAmbiguous {
            turn_treatment: AmbiguousOperationTurnTreatment::Blocking,
        }
    }

    /// S27 / INV-006: one open logical dependency and one
    /// unclassified issued operation are exact blockers, while independently
    /// derived `U` still retains the known blocking ambiguity.
    #[test]
    fn s27_inv006_unfinished_owned_work_derives_exact_blockers() {
        let open_request = OwnedLogicalDependencyRef::ToolRequest(tool_request_id(1));
        let unclassified = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let ambiguous = IssuedOperationRef::ToolAttempt(tool_attempt_id(3));
        let facts = projection(
            running_attempt(),
            [
                (open_request, LogicalDependencyClosure::Open),
                (
                    OwnedLogicalDependencyRef::Approval(tool_request_id(4)),
                    LogicalDependencyClosure::TerminallyNonDispatchable,
                ),
            ],
            [
                (
                    IssuedOperationRef::ModelCall(model_call_id(1)),
                    IssuedOperationClosure::Unclassified,
                ),
                (unclassified, IssuedOperationClosure::Unclassified),
                (ambiguous, blocking_ambiguity()),
            ],
        )
        .apply(nonterminal_fact(1, 1))
        .expect("owned call and compatible state accept mismatch");

        assert_eq!(
            facts.owned_work_status(),
            FatalMismatchOwnedWorkStatus::Unfinished
        );
        assert_eq!(
            facts
                .unfinished_blockers()
                .expect("unfinished facts expose exact blockers")
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                FatalMismatchOwnedWorkBlocker::LogicalDependency(open_request),
                FatalMismatchOwnedWorkBlocker::UnclassifiedOperation(unclassified),
            ])
        );
        let remainder = facts
            .blocking_ambiguities()
            .expect("unfinished work does not erase independently derived U");
        assert_eq!(remainder.operation_count(), 1);
        assert!(remainder.contains(ambiguous));
    }

    /// S27 / INV-006 / INV-025 / INV-026: when every owned fact is classified
    /// and no blocking ambiguity remains, closure is a candidate for direct
    /// failure; resolved and accepted-risk physical ambiguities are excluded
    /// without being rewritten.
    #[test]
    fn s27_inv006_inv025_inv026_closed_without_blocking_ambiguity_is_exact() {
        let resolved = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let accepted = IssuedOperationRef::ToolAttempt(tool_attempt_id(3));
        let facts = projection(
            running_attempt(),
            [(
                OwnedLogicalDependencyRef::ToolRequest(tool_request_id(1)),
                LogicalDependencyClosure::TerminallyNonDispatchable,
            )],
            [
                (
                    IssuedOperationRef::ModelCall(model_call_id(1)),
                    IssuedOperationClosure::Unclassified,
                ),
                (
                    resolved,
                    IssuedOperationClosure::PhysicallyAmbiguous {
                        turn_treatment: AmbiguousOperationTurnTreatment::ResolvedByEvidence,
                    },
                ),
                (
                    accepted,
                    IssuedOperationClosure::PhysicallyAmbiguous {
                        turn_treatment: AmbiguousOperationTurnTreatment::DuplicateRiskAccepted,
                    },
                ),
            ],
        )
        .apply(nonterminal_fact(1, 1))
        .expect("closed projection accepts mismatch");

        assert_eq!(
            facts.owned_work_status(),
            FatalMismatchOwnedWorkStatus::ClosedWithoutBlockingAmbiguity
        );
        assert_eq!(
            facts
                .projection()
                .operations
                .get(&IssuedOperationRef::ModelCall(model_call_id(1))),
            Some(&IssuedOperationClosure::ClassifiedNonAmbiguous)
        );
        assert!(facts.unfinished_blockers().is_none());
        assert!(facts.blocking_ambiguities().is_none());
    }

    /// S27 / INV-006 / INV-014 / INV-025 / INV-026: the primary scenario's
    /// closed `{Y}` remainder is derived exactly and canonically.
    #[test]
    fn s27_inv006_inv014_inv025_inv026_closed_remainder_is_exact() {
        let y = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let fact = nonterminal_fact(1, 1);
        let facts = projection(
            running_attempt(),
            [],
            [
                (
                    IssuedOperationRef::ModelCall(model_call_id(1)),
                    IssuedOperationClosure::Unclassified,
                ),
                (y, blocking_ambiguity()),
            ],
        )
        .apply(fact)
        .expect("last unclassified operation becomes known failed");

        assert_eq!(facts.causes().failures().count(), 1);
        assert!(facts.causes().contains(fact.failure()));
        assert_eq!(
            facts.causes().interrupt(),
            AppliedInterruptState::NoAppliedInterrupt
        );
        assert_eq!(
            facts.owned_work_status(),
            FatalMismatchOwnedWorkStatus::ClosedWithBlockingAmbiguity
        );
        let remainder = facts
            .blocking_ambiguities()
            .expect("closed ambiguity produces an exact nonempty remainder");
        assert_eq!(remainder.operation_count(), 1);
        assert!(remainder.contains(y));
    }

    /// S07 / S27 / INV-014 / INV-029: prior failures, the new trusted failure,
    /// and the exact applied interrupt are retained by canonical idempotent
    /// union.
    #[test]
    fn s07_s27_inv014_inv029_complete_f_unions_causes_and_interrupt() {
        let prior = failure(1);
        let new = failure(2);
        let cancellation_only = projection(
            running_attempt()
                .request_cancellation(interrupt(1))
                .expect("running attempt accepts interrupt"),
            [],
            [(
                IssuedOperationRef::ModelCall(model_call_id(1)),
                IssuedOperationClosure::Unclassified,
            )],
        )
        .apply(AppliedProviderTargetMismatch::test_nonterminal(
            provider_target_evidence_id(2),
            model_call_id(1),
        ))
        .expect("mismatch upgrades cancellation-only stop");
        assert_eq!(
            cancellation_only
                .causes()
                .failures()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([new])
        );
        assert_eq!(
            cancellation_only.causes().interrupt(),
            AppliedInterruptState::Applied {
                proof: interrupt(1)
            }
        );

        let stopped = running_attempt()
            .request_cancellation(interrupt(1))
            .expect("running attempt accepts interrupt")
            .request_fatal_mismatch(prior)
            .expect("cancellation stop upgrades to fatal");
        let facts = projection(
            stopped,
            [],
            [(
                IssuedOperationRef::ModelCall(model_call_id(1)),
                IssuedOperationClosure::Unclassified,
            )],
        )
        .apply(AppliedProviderTargetMismatch::test_nonterminal(
            provider_target_evidence_id(2),
            model_call_id(1),
        ))
        .expect("compatible fact extends complete causes");

        assert_eq!(facts.current_attempt().id(), turn_attempt_id(1));
        assert_eq!(
            facts.causes().interrupt(),
            AppliedInterruptState::Applied {
                proof: interrupt(1)
            }
        );
        assert_eq!(
            facts.causes().failures().collect::<BTreeSet<_>>(),
            BTreeSet::from([prior, new])
        );

        let replayed = projection(
            running_attempt()
                .request_cancellation(interrupt(1))
                .expect("running attempt accepts interrupt")
                .request_fatal_mismatch(prior)
                .expect("cancellation stop upgrades to fatal"),
            [],
            [(
                IssuedOperationRef::ModelCall(model_call_id(1)),
                IssuedOperationClosure::Unclassified,
            )],
        )
        .apply(AppliedProviderTargetMismatch::test_nonterminal(
            provider_target_evidence_id(1),
            model_call_id(1),
        ))
        .expect("failure-set replay remains idempotent");
        assert_eq!(
            replayed.causes().failures().collect::<BTreeSet<_>>(),
            BTreeSet::from([prior])
        );
        assert_eq!(replayed.causes().interrupt(), facts.causes().interrupt());
    }

    /// S21 / INV-014 / INV-025 / INV-026: resolving mismatch evidence changes
    /// only the ambiguous call's turn treatment, so another blocking operation
    /// remains exact while the physical call stays ambiguous.
    #[test]
    fn s21_inv014_inv025_inv026_terminal_resolution_removes_only_resolved_call() {
        let x = IssuedOperationRef::ModelCall(model_call_id(1));
        let y = IssuedOperationRef::ToolAttempt(tool_attempt_id(2));
        let fact = AppliedProviderTargetMismatch::test_terminal_ambiguity_resolution(
            provider_target_evidence_id(1),
            model_call_id(1),
        );
        let facts = projection(
            running_attempt(),
            [],
            [(x, blocking_ambiguity()), (y, blocking_ambiguity())],
        )
        .apply(fact)
        .expect("blocking terminal ambiguity accepts resolving fact");

        let remainder = facts
            .blocking_ambiguities()
            .expect("the unrelated ambiguity remains blocking");
        assert_eq!(remainder.operation_count(), 1);
        assert!(!remainder.contains(x));
        assert!(remainder.contains(y));
        assert_eq!(
            facts.projection().operations.get(&x),
            Some(&IssuedOperationClosure::PhysicallyAmbiguous {
                turn_treatment: AmbiguousOperationTurnTreatment::ResolvedByEvidence,
            })
        );
        assert_eq!(
            fact.effect(),
            ProviderTargetMismatchEffectView::ResolveTerminalAmbiguity
        );
    }

    /// S21 / S27 / INV-006 / INV-014 / INV-025 / INV-026: all three trusted
    /// effects accept exactly their compatible physical predecessor states.
    /// Every other effect/state pair rejects with both inputs unchanged.
    #[test]
    fn s21_s27_inv006_inv014_inv025_inv026_effect_state_matrix_is_exhaustive() {
        let call = model_call_id(1);
        let operation = IssuedOperationRef::ModelCall(call);
        let unclassified = IssuedOperationClosure::Unclassified;
        let classified = IssuedOperationClosure::ClassifiedNonAmbiguous;
        let blocking = blocking_ambiguity();
        let resolved = IssuedOperationClosure::PhysicallyAmbiguous {
            turn_treatment: AmbiguousOperationTurnTreatment::ResolvedByEvidence,
        };
        let accepted = IssuedOperationClosure::PhysicallyAmbiguous {
            turn_treatment: AmbiguousOperationTurnTreatment::DuplicateRiskAccepted,
        };
        let states = [unclassified, classified, blocking, resolved, accepted];
        let cases = [
            (
                nonterminal_fact(1, 1),
                [Some(classified), None, None, None, None],
            ),
            (
                AppliedProviderTargetMismatch::test_terminal_ambiguity_resolution(
                    provider_target_evidence_id(1),
                    call,
                ),
                [None, None, Some(resolved), Some(resolved), None],
            ),
            (
                AppliedProviderTargetMismatch::test_completed_invalidation(call),
                [None, Some(classified), None, None, None],
            ),
        ];

        for (fact, expected_states) in cases {
            for (current, expected) in states.iter().copied().zip(expected_states) {
                let input = projection(running_attempt(), [], [(operation, current)]);
                let unchanged = input.clone();
                match expected {
                    Some(expected) => {
                        let facts = input.apply(fact).expect("compatible pair must apply");
                        assert_eq!(
                            facts.projection().operations.get(&operation),
                            Some(&expected)
                        );
                        assert!(facts.causes().contains(fact.failure()));
                        assert!(facts.blocking_ambiguities().is_none());
                    }
                    None => {
                        let rejection =
                            FatalMismatchProjectionRejection::AffectedCallStateMismatch {
                                call,
                                effect: fact.effect(),
                                current,
                            };
                        let error = input
                            .apply(fact)
                            .expect_err("incompatible pair must reject");
                        assert_eq!(error.projection(), &unchanged);
                        assert_eq!(error.fact(), fact);
                        assert_eq!(error.rejection(), rejection);
                        assert_eq!(error.into_parts(), (unchanged, fact, rejection));
                    }
                }
            }
        }
    }

    /// INV-006 / INV-014: the two non-matrix predecessor failures reject with
    /// the exact projection and fact unchanged.
    #[test]
    fn inv006_inv014_prepared_or_missing_call_rejects_unchanged() {
        let fact = nonterminal_fact(1, 1);
        let cases = [
            (
                projection(
                    CurrentTurnAttempt::prepared(turn_attempt_id(1)),
                    [],
                    [(
                        IssuedOperationRef::ModelCall(model_call_id(1)),
                        IssuedOperationClosure::Unclassified,
                    )],
                ),
                FatalMismatchProjectionRejection::AttemptIsPrepared,
            ),
            (
                projection(running_attempt(), [], []),
                FatalMismatchProjectionRejection::AffectedCallIsNotOwned {
                    call: model_call_id(1),
                },
            ),
        ];

        for (input, rejection) in cases {
            let unchanged = input.clone();
            let error = input
                .apply(fact)
                .expect_err("invalid predecessor must reject");
            assert_eq!(error.into_parts(), (unchanged, fact, rejection));
        }
    }
}
