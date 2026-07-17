//! Turn-attempt stop causes and terminal value algebra.
//!
//! ADR-0004 and ADR-0005 are normative. This module models only canonical stop
//! values and cause-specific terminal history. Current-state transitions and
//! the turn aggregate's operation, wait, terminal-guard, and persistence rules
//! are separate later slices.
//! Standalone values here are candidates, not proof of owning-turn correlation
//! or complete aggregate evidence; authoritative ended-attempt construction
//! remains opaque until those guards arrive.

use std::collections::BTreeSet;

use crate::{AppliedInterruptProof, ModelCallId, ProviderTargetEvidenceId};

/// One trusted provider-target mismatch fact that requires fatal stop.
///
/// This value is opaque because raw evidence or call identities do not prove a
/// trusted mismatch:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ProviderTargetEvidenceId, ProviderTargetMismatchFailureKind,
///     ProviderTargetMismatchFailureRef,
/// };
///
/// fn raw_evidence_is_not_a_failure(evidence: ProviderTargetEvidenceId) {
///     let _ = ProviderTargetMismatchFailureRef {
///         kind: ProviderTargetMismatchFailureKind::NonterminalCallObservation { evidence },
///     };
/// }
/// ```
///
/// A later provider-evidence slice supplies the trusted producer after it can
/// validate the canonical call and observation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderTargetMismatchFailureRef {
    kind: ProviderTargetMismatchFailureKind,
}

impl ProviderTargetMismatchFailureRef {
    /// Returns the typed source of the fatal mismatch.
    pub const fn kind(self) -> ProviderTargetMismatchFailureKind {
        self.kind
    }

    #[cfg(test)]
    const fn nonterminal_call_observation(evidence: ProviderTargetEvidenceId) -> Self {
        Self {
            kind: ProviderTargetMismatchFailureKind::NonterminalCallObservation { evidence },
        }
    }

    #[cfg(test)]
    const fn terminal_ambiguity_resolution(evidence: ProviderTargetEvidenceId) -> Self {
        Self {
            kind: ProviderTargetMismatchFailureKind::TerminalAmbiguityResolution { evidence },
        }
    }

    #[cfg(test)]
    const fn terminal_call_invalidation(invalidated_call: ModelCallId) -> Self {
        Self {
            kind: ProviderTargetMismatchFailureKind::TerminalCallInvalidation { invalidated_call },
        }
    }
}

/// Identifies how a trusted provider-target mismatch affects attempt stop.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProviderTargetMismatchFailureKind {
    /// Trusted mismatch evidence was observed for a nonterminal call.
    NonterminalCallObservation {
        /// The exact trusted observation.
        evidence: ProviderTargetEvidenceId,
    },
    /// Trusted evidence resolved an already-terminal ambiguous call.
    TerminalAmbiguityResolution {
        /// The exact trusted resolving observation.
        evidence: ProviderTargetEvidenceId,
    },
    /// A completed current-authority call was invalidated before turn end.
    TerminalCallInvalidation {
        /// The exact call whose committed material became unusable.
        invalidated_call: ModelCallId,
    },
}

/// Whether fatal mismatch stop also retains an applied interrupt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AppliedInterruptState {
    /// No interrupt has been applied to this fatal stop value.
    NoAppliedInterrupt,
    /// The exact applied interrupt is retained alongside fatal failures.
    Applied {
        /// Purpose-specific authority from the matching applied command.
        proof: AppliedInterruptProof,
    },
}

/// Complete nonempty fatal-mismatch stop causes for one attempt.
///
/// The private canonical set is initialized from one trusted reference, so an
/// empty fatal stop cannot be represented:
///
/// ```compile_fail
/// use std::collections::BTreeSet;
/// use signalbox_domain::{AppliedInterruptState, FatalMismatchStopCauses};
///
/// let _ = FatalMismatchStopCauses {
///     failures: BTreeSet::new(),
///     interrupt: AppliedInterruptState::NoAppliedInterrupt,
/// };
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FatalMismatchStopCauses {
    failures: BTreeSet<ProviderTargetMismatchFailureRef>,
    interrupt: AppliedInterruptState,
}

impl FatalMismatchStopCauses {
    /// Creates a fatal stop from one mismatch and the complete known interrupt
    /// state.
    pub fn new(
        failure: ProviderTargetMismatchFailureRef,
        interrupt: AppliedInterruptState,
    ) -> Self {
        Self {
            failures: BTreeSet::from([failure]),
            interrupt,
        }
    }

    /// Iterates over the canonical nonempty failure set.
    pub fn failures(&self) -> impl ExactSizeIterator<Item = ProviderTargetMismatchFailureRef> + '_ {
        self.failures.iter().copied()
    }

    /// Returns whether this exact mismatch reference is present.
    pub fn contains(&self, failure: ProviderTargetMismatchFailureRef) -> bool {
        self.failures.contains(&failure)
    }

    /// Returns the typed interrupt state retained with the failures.
    pub const fn interrupt(&self) -> AppliedInterruptState {
        self.interrupt
    }

    fn add_failure(&mut self, failure: ProviderTargetMismatchFailureRef) {
        self.failures.insert(failure);
    }

    fn add_interrupt(&mut self, proof: AppliedInterruptProof) -> bool {
        match self.interrupt {
            AppliedInterruptState::NoAppliedInterrupt => {
                self.interrupt = AppliedInterruptState::Applied { proof };
                true
            }
            AppliedInterruptState::Applied { proof: existing } => existing == proof,
        }
    }
}

/// The complete nonempty reason a live attempt prohibits new semantic effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnAttemptStopCauses {
    /// Best-effort cancellation was requested from one applied interrupt.
    CancellationOnly {
        /// The exact applied interrupt authorizing cancellation.
        interrupt: AppliedInterruptProof,
    },
    /// Fatal mismatch dominates ordinary cancellation and retains all causes.
    FatalMismatch(FatalMismatchStopCauses),
}

impl TurnAttemptStopCauses {
    /// Creates cancellation-only stop from an applied interrupt.
    pub const fn cancellation_only(interrupt: AppliedInterruptProof) -> Self {
        Self::CancellationOnly { interrupt }
    }

    /// Creates fatal stop from one trusted mismatch and no applied interrupt.
    pub fn fatal_mismatch(failure: ProviderTargetMismatchFailureRef) -> Self {
        Self::FatalMismatch(FatalMismatchStopCauses::new(
            failure,
            AppliedInterruptState::NoAppliedInterrupt,
        ))
    }

    /// Adds one fatal mismatch by canonical set union.
    ///
    /// Cancellation-only stop upgrades to fatal while retaining its proof.
    pub fn add_fatal_mismatch(mut self, failure: ProviderTargetMismatchFailureRef) -> Self {
        match &mut self {
            Self::CancellationOnly { interrupt } => {
                Self::FatalMismatch(FatalMismatchStopCauses::new(
                    failure,
                    AppliedInterruptState::Applied { proof: *interrupt },
                ))
            }
            Self::FatalMismatch(causes) => {
                causes.add_failure(failure);
                self
            }
        }
    }

    /// Adds an interrupt without losing fatal failures.
    ///
    /// Equal replay is idempotent. A distinct second proof is rejected and the
    /// error returns both the unchanged causes and requested proof.
    pub fn add_interrupt(
        mut self,
        proof: AppliedInterruptProof,
    ) -> Result<Self, TurnAttemptStopCauseUnionError> {
        let accepted = match &mut self {
            Self::CancellationOnly { interrupt } => *interrupt == proof,
            Self::FatalMismatch(causes) => causes.add_interrupt(proof),
        };
        if accepted {
            Ok(self)
        } else {
            Err(TurnAttemptStopCauseUnionError {
                current: self,
                requested: proof,
            })
        }
    }
}

/// A rejected attempt to add a distinct second interrupt proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnAttemptStopCauseUnionError {
    current: TurnAttemptStopCauses,
    requested: AppliedInterruptProof,
}

impl TurnAttemptStopCauseUnionError {
    /// Borrows the unchanged stop causes.
    pub const fn current(&self) -> &TurnAttemptStopCauses {
        &self.current
    }

    /// Returns the distinct proof that was rejected.
    pub const fn requested(&self) -> AppliedInterruptProof {
        self.requested
    }

    /// Returns the unchanged causes and rejected proof.
    pub fn into_parts(self) -> (TurnAttemptStopCauses, AppliedInterruptProof) {
        (self.current, self.requested)
    }
}

/// Cause-specific terminal history for one turn attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttemptEnd {
    /// The attempt ended without any stop cause.
    WithoutStop {
        /// The honest terminal classification.
        disposition: UnstoppedAttemptDisposition,
    },
    /// The attempt ended after an applied interrupt.
    AfterCancellation {
        /// The exact cancellation authority.
        cause: AppliedInterruptProof,
        /// The honest outcome after best-effort cancellation.
        disposition: CancellationStopDisposition,
    },
    /// The attempt ended after one or more fatal mismatches.
    AfterFatalMismatch {
        /// The complete fatal set and retained interrupt state.
        causes: FatalMismatchStopCauses,
        /// The restricted fatal-stop outcome.
        disposition: FatalMismatchStopDisposition,
    },
}

/// An attempt disposition available only when no stop was requested.
///
/// ```compile_fail
/// use signalbox_domain::UnstoppedAttemptDisposition;
/// let _ = UnstoppedAttemptDisposition::Cancelled;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UnstoppedAttemptDisposition {
    /// The turn produced its conversational outcome.
    TurnCompleted,
    /// The turn produced an explicit refusal.
    TurnRefused,
    /// Orchestration yielded to a typed durable wait.
    YieldedToDurableWait,
    /// Classified evidence establishes failure.
    KnownFailure,
    /// Startup abandoned the prior-process tenure.
    Lost,
    /// Live classification ended on unresolved physical ambiguity.
    Ambiguous,
}

/// An honest disposition after an applied cancellation request.
///
/// ```compile_fail
/// use signalbox_domain::CancellationStopDisposition;
/// let _ = CancellationStopDisposition::YieldedToDurableWait;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CancellationStopDisposition {
    /// Outcome-authoritative work raced cancellation and completed.
    TurnCompleted,
    /// Outcome-authoritative work raced cancellation and refused.
    TurnRefused,
    /// Classified evidence establishes failure.
    KnownFailure,
    /// Startup abandoned the prior-process tenure.
    Lost,
    /// The interrupt is proven to have prevented all remaining work.
    Cancelled,
    /// Live classification ended on unresolved physical ambiguity.
    Ambiguous,
}

/// A disposition permitted after fatal provider-target mismatch.
///
/// ```compile_fail
/// use signalbox_domain::FatalMismatchStopDisposition;
/// let _ = FatalMismatchStopDisposition::TurnCompleted;
/// ```
///
/// ```compile_fail
/// use signalbox_domain::FatalMismatchStopDisposition;
/// let _ = FatalMismatchStopDisposition::TurnRefused;
/// ```
///
/// ```compile_fail
/// use signalbox_domain::FatalMismatchStopDisposition;
/// let _ = FatalMismatchStopDisposition::Cancelled;
/// ```
///
/// ```compile_fail
/// use signalbox_domain::FatalMismatchStopDisposition;
/// let _ = FatalMismatchStopDisposition::YieldedToDurableWait;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FatalMismatchStopDisposition {
    /// Fatal evidence establishes failure.
    KnownFailure,
    /// Startup abandoned the prior-process tenure.
    Lost,
    /// Live classification ended with unresolved physical ambiguity.
    Ambiguous,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applied_interrupt::test_applied_interrupt_proof;
    use crate::test_support::{
        command_id, model_call_id, provider_target_evidence_id as evidence, turn_id,
    };

    fn proof(value: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(value), turn_id(100))
    }

    fn failure(value: u128) -> ProviderTargetMismatchFailureRef {
        ProviderTargetMismatchFailureRef::nonterminal_call_observation(evidence(value))
    }

    /// INV-006: fatal stop is nonempty and repeated additions are canonical set
    /// union rather than duplicate causes.
    #[test]
    fn fatal_failures_are_nonempty_canonical_set_union() {
        let causes = TurnAttemptStopCauses::fatal_mismatch(failure(2))
            .add_fatal_mismatch(failure(1))
            .add_fatal_mismatch(failure(2));
        let TurnAttemptStopCauses::FatalMismatch(causes) = causes else {
            panic!("fatal construction must stay fatal");
        };

        assert_eq!(
            causes.failures().collect::<Vec<_>>(),
            vec![failure(1), failure(2)]
        );
        assert!(causes.contains(failure(1)));
        assert!(!causes.contains(failure(3)));
    }

    /// S07 / INV-006 / INV-029: fatal failure and applied interrupt addition is
    /// idempotent and event-order independent without losing either fact.
    #[test]
    fn stop_union_is_idempotent_and_event_order_independent() {
        let interrupt_then_failure = TurnAttemptStopCauses::cancellation_only(proof(1))
            .add_fatal_mismatch(failure(2))
            .add_fatal_mismatch(failure(1))
            .add_fatal_mismatch(failure(2));
        let failure_then_interrupt = TurnAttemptStopCauses::fatal_mismatch(failure(1))
            .add_fatal_mismatch(failure(2))
            .add_interrupt(proof(1))
            .and_then(|causes| causes.add_interrupt(proof(1)))
            .expect("equal interrupt replay is idempotent");

        assert_eq!(interrupt_then_failure, failure_then_interrupt);
        let TurnAttemptStopCauses::FatalMismatch(causes) = interrupt_then_failure else {
            panic!("fatal union must stay fatal");
        };
        assert_eq!(causes.failures().len(), 2);
        assert_eq!(
            causes.interrupt(),
            AppliedInterruptState::Applied { proof: proof(1) }
        );
    }

    /// INV-006 / INV-029: a distinct second proof cannot replace the retained
    /// cancellation authority.
    #[test]
    fn distinct_second_interrupt_is_rejected_unchanged() {
        for current in [
            TurnAttemptStopCauses::cancellation_only(proof(1)),
            TurnAttemptStopCauses::fatal_mismatch(failure(1))
                .add_interrupt(proof(1))
                .unwrap(),
        ] {
            assert_eq!(current.clone().add_interrupt(proof(1)).unwrap(), current);
            let error = current.clone().add_interrupt(proof(2)).unwrap_err();
            assert_eq!(error.current(), &current);
            assert_eq!(error.requested(), proof(2));
            assert_eq!(error.into_parts(), (current, proof(2)));
        }
    }

    /// ADR-0005 / INV-006: the three accepted fatal-reference kinds remain
    /// typed and distinct.
    #[test]
    fn fatal_failure_reference_kinds_are_distinct() {
        let nonterminal =
            ProviderTargetMismatchFailureRef::nonterminal_call_observation(evidence(1));
        let resolution =
            ProviderTargetMismatchFailureRef::terminal_ambiguity_resolution(evidence(1));
        let invalidation =
            ProviderTargetMismatchFailureRef::terminal_call_invalidation(model_call_id(1));

        assert_ne!(nonterminal, resolution);
        assert_ne!(nonterminal, invalidation);
        assert_ne!(resolution, invalidation);
        assert_eq!(
            nonterminal.kind(),
            ProviderTargetMismatchFailureKind::NonterminalCallObservation {
                evidence: evidence(1),
            }
        );
        assert_eq!(
            resolution.kind(),
            ProviderTargetMismatchFailureKind::TerminalAmbiguityResolution {
                evidence: evidence(1),
            }
        );
        assert_eq!(
            invalidation.kind(),
            ProviderTargetMismatchFailureKind::TerminalCallInvalidation {
                invalidated_call: model_call_id(1),
            }
        );
    }

    /// S04 / S06 / S07 / S10 / S23 / INV-006 / INV-018 / INV-029 / INV-034:
    /// each terminal family retains its exact typed cause and disposition.
    #[test]
    fn every_allowed_terminal_disposition_stays_in_its_typed_family() {
        let fatal = FatalMismatchStopCauses::new(
            failure(1),
            AppliedInterruptState::Applied { proof: proof(1) },
        );
        for disposition in [
            UnstoppedAttemptDisposition::TurnCompleted,
            UnstoppedAttemptDisposition::TurnRefused,
            UnstoppedAttemptDisposition::YieldedToDurableWait,
            UnstoppedAttemptDisposition::KnownFailure,
            UnstoppedAttemptDisposition::Lost,
            UnstoppedAttemptDisposition::Ambiguous,
        ] {
            let end = AttemptEnd::WithoutStop { disposition };
            assert!(matches!(
                end,
                AttemptEnd::WithoutStop {
                    disposition: actual,
                } if actual == disposition
            ));
        }
        for disposition in [
            CancellationStopDisposition::TurnCompleted,
            CancellationStopDisposition::TurnRefused,
            CancellationStopDisposition::KnownFailure,
            CancellationStopDisposition::Lost,
            CancellationStopDisposition::Cancelled,
            CancellationStopDisposition::Ambiguous,
        ] {
            let end = AttemptEnd::AfterCancellation {
                cause: proof(1),
                disposition,
            };
            assert!(matches!(
                end,
                AttemptEnd::AfterCancellation {
                    cause,
                    disposition: actual,
                } if cause == proof(1) && actual == disposition
            ));
        }
        for disposition in [
            FatalMismatchStopDisposition::KnownFailure,
            FatalMismatchStopDisposition::Lost,
            FatalMismatchStopDisposition::Ambiguous,
        ] {
            let end = AttemptEnd::AfterFatalMismatch {
                causes: fatal.clone(),
                disposition,
            };
            assert!(matches!(
                end,
                AttemptEnd::AfterFatalMismatch {
                    causes,
                    disposition: actual,
                } if causes == fatal && actual == disposition
            ));
        }
    }

    /// INV-018: refusal remains representable without fatal stop and after a
    /// cancellation race; the fatal family has no refusal variant.
    #[test]
    fn refusal_is_typed_for_unstopped_and_cancellation_race_history() {
        assert!(matches!(
            AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::TurnRefused,
            },
            AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::TurnRefused,
            }
        ));
        assert!(matches!(
            AttemptEnd::AfterCancellation {
                cause: proof(1),
                disposition: CancellationStopDisposition::TurnRefused,
            },
            AttemptEnd::AfterCancellation {
                disposition: CancellationStopDisposition::TurnRefused,
                ..
            }
        ));
    }

    /// S04 / INV-034: startup loss retains the terminal family matching the
    /// complete recovered stop causes.
    #[test]
    fn lost_is_representable_in_all_three_matching_terminal_families() {
        let fatal = FatalMismatchStopCauses::new(
            failure(1),
            AppliedInterruptState::Applied { proof: proof(1) },
        );

        assert!(matches!(
            AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            },
            AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            }
        ));
        assert!(matches!(
            AttemptEnd::AfterCancellation {
                cause: proof(1),
                disposition: CancellationStopDisposition::Lost,
            },
            AttemptEnd::AfterCancellation {
                disposition: CancellationStopDisposition::Lost,
                ..
            }
        ));
        assert!(matches!(
            AttemptEnd::AfterFatalMismatch {
                causes: fatal,
                disposition: FatalMismatchStopDisposition::Lost,
            },
            AttemptEnd::AfterFatalMismatch {
                disposition: FatalMismatchStopDisposition::Lost,
                ..
            }
        ));
    }
}
