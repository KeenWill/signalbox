//! Turn-attempt stop causes, terminal values, and local state transitions.
//!
//! ADR-0004 and ADR-0005 are normative. This module models canonical stop
//! values, cause-specific terminal history, and the attempt-local predecessor
//! matrix. The turn aggregate's operation, wait, terminal-guard, correlation,
//! and persistence rules are a separate later slice. A locally ended attempt
//! therefore does not by itself prove complete aggregate evidence. Creation
//! and mutation stay crate-private so only that aggregate can expose guarded
//! lifecycle operations.

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

/// The sole nonterminal state carried by an active running turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CurrentTurnAttemptState {
    /// Orchestration is durably prepared but not externally authorized.
    Prepared,
    /// External orchestration is authorized for this attempt.
    Running,
    /// Stop was requested and new semantic effects are prohibited.
    StopRequested {
        /// The complete nonempty typed stop causes.
        causes: TurnAttemptStopCauses,
    },
}

/// One current, nonterminal physical orchestration tenure.
///
/// The identity is factored outside the state and preserved by every consuming
/// transition. Only the crate-private prepared entry creates a current attempt,
/// so callers cannot couple an identity directly to `Running` or
/// `StopRequested` or invoke attempt-local transitions around the turn
/// aggregate.
///
/// ```compile_fail
/// use signalbox_domain::{CurrentTurnAttempt, CurrentTurnAttemptState, TurnAttemptId};
///
/// fn running_cannot_be_forged(id: TurnAttemptId) {
///     let _ = CurrentTurnAttempt {
///         id,
///         state: CurrentTurnAttemptState::Running,
///     };
/// }
/// ```
///
/// Attempt-local transitions are sealed behind the turn aggregate:
///
/// ```compile_fail
/// use signalbox_domain::{CurrentTurnAttempt, TurnAttemptId};
///
/// fn local_transition_cannot_bypass_aggregate(id: TurnAttemptId) {
///     let _ = CurrentTurnAttempt::prepared(id);
/// }
/// ```
///
/// # Scope
///
/// This is an attempt component, not an independently persisted aggregate.
/// The turn aggregate owns current-attempt uniqueness, proof-to-turn and
/// mismatch correlation, operation classification, durable-wait changes,
/// complete terminal guards, and atomic persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentTurnAttempt {
    id: crate::TurnAttemptId,
    state: CurrentTurnAttemptState,
}

#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the next stacked aggregate slice"
)]
impl CurrentTurnAttempt {
    /// Creates a newly durably prepared attempt.
    pub(crate) const fn prepared(id: crate::TurnAttemptId) -> Self {
        Self {
            id,
            state: CurrentTurnAttemptState::Prepared,
        }
    }

    /// Returns the physical attempt identity preserved by transitions.
    pub const fn id(&self) -> crate::TurnAttemptId {
        self.id
    }

    /// Borrows the current nonterminal state.
    pub const fn state(&self) -> &CurrentTurnAttemptState {
        &self.state
    }

    /// Authorizes external orchestration from `Prepared`.
    pub(crate) fn begin_running(self) -> Result<Self, CurrentTurnAttemptTransitionError> {
        let Self { id, state } = self;
        match state {
            CurrentTurnAttemptState::Prepared => Ok(Self {
                id,
                state: CurrentTurnAttemptState::Running,
            }),
            state => Err(CurrentTurnAttemptTransitionError::new(
                Self { id, state },
                AttemptedTurnAttemptTransition::BeginRunning,
            )),
        }
    }

    /// Adds an applied interrupt to `Running` or a compatible stop request.
    ///
    /// Equal replay is idempotent. A distinct second proof is rejected without
    /// changing the current attempt.
    pub(crate) fn request_cancellation(
        self,
        proof: AppliedInterruptProof,
    ) -> Result<Self, CurrentTurnAttemptTransitionError> {
        let attempted = AttemptedTurnAttemptTransition::RequestCancellation { proof };
        let Self { id, state } = self;
        let state = match state {
            CurrentTurnAttemptState::Running => CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::cancellation_only(proof),
            },
            CurrentTurnAttemptState::StopRequested { causes } => {
                match causes.add_interrupt(proof) {
                    Ok(causes) => CurrentTurnAttemptState::StopRequested { causes },
                    Err(error) => {
                        let (causes, _) = error.into_parts();
                        return Err(CurrentTurnAttemptTransitionError::new(
                            Self {
                                id,
                                state: CurrentTurnAttemptState::StopRequested { causes },
                            },
                            attempted,
                        ));
                    }
                }
            }
            state => {
                return Err(CurrentTurnAttemptTransitionError::new(
                    Self { id, state },
                    attempted,
                ));
            }
        };

        Ok(Self { id, state })
    }

    /// Adds a trusted fatal mismatch to `Running` or an existing stop request.
    ///
    /// Cancellation-only stop upgrades to fatal without losing its proof;
    /// existing fatal stop uses canonical idempotent set union.
    pub(crate) fn request_fatal_mismatch(
        self,
        failure: ProviderTargetMismatchFailureRef,
    ) -> Result<Self, CurrentTurnAttemptTransitionError> {
        let attempted = AttemptedTurnAttemptTransition::RequestFatalMismatch { failure };
        let Self { id, state } = self;
        let state = match state {
            CurrentTurnAttemptState::Running => CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::fatal_mismatch(failure),
            },
            CurrentTurnAttemptState::StopRequested { causes } => {
                CurrentTurnAttemptState::StopRequested {
                    causes: causes.add_fatal_mismatch(failure),
                }
            }
            state => {
                return Err(CurrentTurnAttemptTransitionError::new(
                    Self { id, state },
                    attempted,
                ));
            }
        };

        Ok(Self { id, state })
    }

    /// Ends without a stop cause when the predecessor permits the disposition.
    pub(crate) fn end_without_stop(
        self,
        disposition: UnstoppedAttemptDisposition,
    ) -> Result<EndedTurnAttempt, CurrentTurnAttemptTransitionError> {
        self.end(AttemptEnd::WithoutStop { disposition })
    }

    /// Ends with an applied interrupt and an honest classified disposition.
    pub(crate) fn end_after_cancellation(
        self,
        cause: AppliedInterruptProof,
        disposition: CancellationStopDisposition,
    ) -> Result<EndedTurnAttempt, CurrentTurnAttemptTransitionError> {
        self.end(AttemptEnd::AfterCancellation { cause, disposition })
    }

    /// Ends with the exact complete fatal causes and fatal disposition.
    pub(crate) fn end_after_fatal_mismatch(
        self,
        causes: FatalMismatchStopCauses,
        disposition: FatalMismatchStopDisposition,
    ) -> Result<EndedTurnAttempt, CurrentTurnAttemptTransitionError> {
        self.end(AttemptEnd::AfterFatalMismatch {
            causes,
            disposition,
        })
    }

    fn end(
        self,
        attempted_end: AttemptEnd,
    ) -> Result<EndedTurnAttempt, CurrentTurnAttemptTransitionError> {
        let allowed = match (&self.state, &attempted_end) {
            (
                CurrentTurnAttemptState::Prepared,
                AttemptEnd::WithoutStop {
                    disposition:
                        UnstoppedAttemptDisposition::KnownFailure | UnstoppedAttemptDisposition::Lost,
                },
            )
            | (
                CurrentTurnAttemptState::Prepared,
                AttemptEnd::AfterCancellation {
                    disposition: CancellationStopDisposition::Cancelled,
                    ..
                },
            ) => true,
            (
                CurrentTurnAttemptState::Prepared,
                AttemptEnd::AfterFatalMismatch {
                    causes,
                    disposition:
                        FatalMismatchStopDisposition::KnownFailure | FatalMismatchStopDisposition::Lost,
                },
                // ADR-0004 and ADR-0027 route an applied interrupt from Prepared
                // through the atomic AfterCancellation edge instead.
            ) => causes.interrupt() == AppliedInterruptState::NoAppliedInterrupt,
            (CurrentTurnAttemptState::Running, _) => true,
            (
                CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
                },
                AttemptEnd::AfterCancellation { cause, .. },
            ) => interrupt == cause,
            (
                CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::FatalMismatch(current),
                },
                AttemptEnd::AfterFatalMismatch { causes, .. },
            ) => current == causes,
            _ => false,
        };

        if allowed {
            Ok(EndedTurnAttempt {
                id: self.id,
                end: attempted_end,
            })
        } else {
            Err(CurrentTurnAttemptTransitionError::new(
                self,
                AttemptedTurnAttemptTransition::End { end: attempted_end },
            ))
        }
    }
}

/// Immutable terminal history for one physical attempt.
///
/// This type exposes no transition back to a current attempt:
///
/// ```compile_fail
/// use signalbox_domain::EndedTurnAttempt;
///
/// fn terminal_attempt_cannot_run_again(ended: EndedTurnAttempt) {
///     let _ = ended.begin_running();
/// }
/// ```
///
/// Terminal history can only be produced by a valid consuming transition:
///
/// ```compile_fail
/// use signalbox_domain::{AttemptEnd, EndedTurnAttempt, TurnAttemptId};
///
/// fn terminal_history_cannot_be_forged(id: TurnAttemptId, end: AttemptEnd) {
///     let _ = EndedTurnAttempt { id, end };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndedTurnAttempt {
    id: crate::TurnAttemptId,
    end: AttemptEnd,
}

impl EndedTurnAttempt {
    /// Returns the identity preserved from the current attempt.
    pub const fn id(&self) -> crate::TurnAttemptId {
        self.id
    }

    /// Borrows the exact cause-specific terminal history.
    pub const fn end(&self) -> &AttemptEnd {
        &self.end
    }
}

/// The transition input returned when a current attempt rejects it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the next stacked aggregate slice"
)]
pub(crate) enum AttemptedTurnAttemptTransition {
    /// Authorization was requested outside `Prepared`.
    BeginRunning,
    /// An interrupt was requested from an incompatible state or proof.
    RequestCancellation {
        /// The exact proof that could not be added.
        proof: AppliedInterruptProof,
    },
    /// A fatal mismatch was requested from an incompatible state.
    RequestFatalMismatch {
        /// The exact failure that could not be added.
        failure: ProviderTargetMismatchFailureRef,
    },
    /// The requested terminal history does not match the current state.
    End {
        /// The complete requested terminal history.
        end: AttemptEnd,
    },
}

/// A rejected transition with the unchanged current attempt and exact input.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the next stacked aggregate slice"
)]
pub(crate) struct CurrentTurnAttemptTransitionError {
    rejected: Box<(CurrentTurnAttempt, AttemptedTurnAttemptTransition)>,
}

#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the next stacked aggregate slice"
)]
impl CurrentTurnAttemptTransitionError {
    fn new(current: CurrentTurnAttempt, attempted: AttemptedTurnAttemptTransition) -> Self {
        Self {
            rejected: Box::new((current, attempted)),
        }
    }

    /// Borrows the unchanged current attempt.
    pub(crate) fn current(&self) -> &CurrentTurnAttempt {
        &self.rejected.0
    }

    /// Borrows the rejected transition input.
    pub(crate) fn attempted(&self) -> &AttemptedTurnAttemptTransition {
        &self.rejected.1
    }

    /// Returns the unchanged attempt and rejected transition input.
    pub(crate) fn into_parts(self) -> (CurrentTurnAttempt, AttemptedTurnAttemptTransition) {
        *self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applied_interrupt::test_applied_interrupt_proof;
    use crate::test_support::{
        command_id, model_call_id, provider_target_evidence_id as evidence,
        turn_attempt_id as attempt_id, turn_id,
    };

    fn proof(value: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(value), turn_id(100))
    }

    fn failure(value: u128) -> ProviderTargetMismatchFailureRef {
        ProviderTargetMismatchFailureRef::nonterminal_call_observation(evidence(value))
    }

    fn prepared() -> CurrentTurnAttempt {
        CurrentTurnAttempt::prepared(attempt_id(1))
    }

    fn running() -> CurrentTurnAttempt {
        prepared().begin_running().expect("Prepared may run")
    }

    fn cancellation_stopped() -> CurrentTurnAttempt {
        running()
            .request_cancellation(proof(1))
            .expect("Running may request cancellation")
    }

    fn fatal_stopped() -> CurrentTurnAttempt {
        running()
            .request_fatal_mismatch(failure(1))
            .expect("Running may request fatal stop")
    }

    fn fatal_causes(attempt: &CurrentTurnAttempt) -> FatalMismatchStopCauses {
        let CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(causes),
        } = attempt.state()
        else {
            panic!("test fixture must be fatal-stopped");
        };
        causes.clone()
    }

    /// INV-004 / INV-006: Prepared is the sole entry and authorization
    /// preserves the physical-attempt identity.
    #[test]
    fn prepared_begins_running_with_the_same_identity() {
        let current = prepared().begin_running().expect("Prepared may run");

        assert_eq!(current.id(), attempt_id(1));
        assert_eq!(current.state(), &CurrentTurnAttemptState::Running);
    }

    /// INV-006: authorization rejects every non-Prepared current state and
    /// returns that state unchanged.
    #[test]
    fn begin_running_rejects_every_other_current_state_unchanged() {
        for current in [running(), cancellation_stopped(), fatal_stopped()] {
            let error = current.clone().begin_running().unwrap_err();
            assert_eq!(
                error.into_parts(),
                (current, AttemptedTurnAttemptTransition::BeginRunning)
            );
        }
    }

    /// S07 / INV-006 / INV-029: Running accepts either singleton stop; stopped
    /// values replay/union compatible causes; Prepared accepts neither.
    #[test]
    fn stop_request_transition_matrix_preserves_complete_causes() {
        let cancellation = cancellation_stopped();
        assert!(matches!(
            cancellation.state(),
            CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
            } if *interrupt == proof(1)
        ));
        assert_eq!(
            cancellation.clone().request_cancellation(proof(1)).unwrap(),
            cancellation
        );

        let fatal = fatal_stopped();
        assert!(fatal_causes(&fatal).contains(failure(1)));
        assert_eq!(
            fatal.clone().request_fatal_mismatch(failure(1)).unwrap(),
            fatal
        );
        let upgraded = cancellation
            .request_fatal_mismatch(failure(1))
            .expect("fatal mismatch upgrades cancellation stop");
        let added_interrupt = fatal
            .request_cancellation(proof(1))
            .expect("fatal stop retains a first interrupt");
        assert_eq!(upgraded, added_interrupt);
        let unioned = upgraded
            .request_fatal_mismatch(failure(2))
            .expect("fatal failure set unions");
        assert!(fatal_causes(&unioned).contains(failure(2)));

        assert_eq!(
            prepared()
                .request_cancellation(proof(1))
                .unwrap_err()
                .into_parts(),
            (
                prepared(),
                AttemptedTurnAttemptTransition::RequestCancellation { proof: proof(1) }
            )
        );
        assert_eq!(
            prepared()
                .request_fatal_mismatch(failure(1))
                .unwrap_err()
                .into_parts(),
            (
                prepared(),
                AttemptedTurnAttemptTransition::RequestFatalMismatch {
                    failure: failure(1)
                }
            )
        );
    }

    /// INV-006 / INV-029: a distinct second interrupt is rejected for either
    /// stopped family without changing the exact current attempt.
    #[test]
    fn conflicting_interrupt_returns_the_unchanged_stopped_attempt() {
        let fatal_with_interrupt = fatal_stopped().request_cancellation(proof(1)).unwrap();
        for current in [cancellation_stopped(), fatal_with_interrupt] {
            let error = current.clone().request_cancellation(proof(2)).unwrap_err();
            assert_eq!(error.current(), &current);
            assert_eq!(
                error.attempted(),
                &AttemptedTurnAttemptTransition::RequestCancellation { proof: proof(2) }
            );
        }
    }

    /// S03 / S04 / S07 / INV-006 / INV-029 / INV-034: Prepared accepts exactly
    /// the restricted unsent and startup terminal branches from ADR-0004.
    #[test]
    fn prepared_terminal_matrix_is_complete() {
        for disposition in all_unstopped_dispositions() {
            let allowed = matches!(
                disposition,
                UnstoppedAttemptDisposition::KnownFailure | UnstoppedAttemptDisposition::Lost
            );
            assert_eq!(
                prepared().end_without_stop(disposition).is_ok(),
                allowed,
                "unexpected Prepared/WithoutStop result for {disposition:?}"
            );
        }
        for disposition in all_cancellation_dispositions() {
            assert_eq!(
                prepared()
                    .end_after_cancellation(proof(1), disposition)
                    .is_ok(),
                disposition == CancellationStopDisposition::Cancelled,
                "unexpected Prepared/AfterCancellation result for {disposition:?}"
            );
        }
        for disposition in all_fatal_dispositions() {
            let allowed = disposition != FatalMismatchStopDisposition::Ambiguous;
            assert_eq!(
                prepared()
                    .end_after_fatal_mismatch(
                        FatalMismatchStopCauses::new(
                            failure(1),
                            AppliedInterruptState::NoAppliedInterrupt,
                        ),
                        disposition,
                    )
                    .is_ok(),
                allowed,
                "unexpected Prepared/AfterFatalMismatch result for {disposition:?}"
            );
            assert!(
                prepared()
                    .end_after_fatal_mismatch(
                        FatalMismatchStopCauses::new(
                            failure(1),
                            AppliedInterruptState::Applied { proof: proof(1) },
                        ),
                        disposition,
                    )
                    .is_err()
            );
        }
    }

    /// S02 / S04 / S06 / S07 / S10 / S23 / INV-004 / INV-006: Running may
    /// enter every type-valid terminal branch once slice 5 establishes guards.
    #[test]
    fn running_accepts_every_type_valid_terminal_value() {
        for disposition in all_unstopped_dispositions() {
            assert_eq!(
                running()
                    .end_without_stop(disposition)
                    .expect("Running accepts every unstopped disposition")
                    .id(),
                attempt_id(1)
            );
        }
        for disposition in all_cancellation_dispositions() {
            assert!(
                running()
                    .end_after_cancellation(proof(1), disposition)
                    .is_ok()
            );
        }
        for interrupt in [
            AppliedInterruptState::NoAppliedInterrupt,
            AppliedInterruptState::Applied { proof: proof(1) },
        ] {
            for disposition in all_fatal_dispositions() {
                assert!(
                    running()
                        .end_after_fatal_mismatch(
                            FatalMismatchStopCauses::new(failure(1), interrupt),
                            disposition,
                        )
                        .is_ok()
                );
            }
        }
    }

    /// S04 / S07 / S23 / INV-006 / INV-029 / INV-034: CancellationOnly ends
    /// only as AfterCancellation with its exact proof and any honest result.
    #[test]
    fn cancellation_stopped_terminal_matrix_is_complete() {
        for disposition in all_cancellation_dispositions() {
            assert!(
                cancellation_stopped()
                    .end_after_cancellation(proof(1), disposition)
                    .is_ok()
            );
            assert!(
                cancellation_stopped()
                    .end_after_cancellation(proof(2), disposition)
                    .is_err()
            );
        }
        for disposition in all_unstopped_dispositions() {
            assert!(
                cancellation_stopped()
                    .end_without_stop(disposition)
                    .is_err()
            );
        }
        for disposition in all_fatal_dispositions() {
            assert!(
                cancellation_stopped()
                    .end_after_fatal_mismatch(
                        FatalMismatchStopCauses::new(
                            failure(1),
                            AppliedInterruptState::Applied { proof: proof(1) },
                        ),
                        disposition,
                    )
                    .is_err()
            );
        }
    }

    /// S04 / S06 / S21 / S23 / INV-006 / INV-034: FatalMismatch ends only as
    /// AfterFatalMismatch with the exact complete cause value.
    #[test]
    fn fatal_stopped_terminal_matrix_is_complete() {
        let without_interrupt = fatal_stopped();
        let exact_without_interrupt = fatal_causes(&without_interrupt);
        for disposition in all_fatal_dispositions() {
            assert!(
                without_interrupt
                    .clone()
                    .end_after_fatal_mismatch(exact_without_interrupt.clone(), disposition)
                    .is_ok()
            );
        }

        let current = fatal_stopped()
            .request_fatal_mismatch(failure(2))
            .and_then(|attempt| attempt.request_cancellation(proof(1)))
            .expect("compatible fatal causes union");
        let exact = fatal_causes(&current);
        for disposition in all_fatal_dispositions() {
            assert!(
                current
                    .clone()
                    .end_after_fatal_mismatch(exact.clone(), disposition)
                    .is_ok()
            );
            assert!(
                current
                    .clone()
                    .end_after_fatal_mismatch(
                        FatalMismatchStopCauses::new(
                            failure(1),
                            AppliedInterruptState::Applied { proof: proof(1) },
                        ),
                        disposition,
                    )
                    .is_err()
            );
        }
        for disposition in all_unstopped_dispositions() {
            assert!(current.clone().end_without_stop(disposition).is_err());
        }
        for disposition in all_cancellation_dispositions() {
            assert!(
                current
                    .clone()
                    .end_after_cancellation(proof(1), disposition)
                    .is_err()
            );
        }

        let TurnAttemptStopCauses::FatalMismatch(superset) =
            TurnAttemptStopCauses::FatalMismatch(exact.clone()).add_fatal_mismatch(failure(3))
        else {
            panic!("adding a fatal failure must stay fatal");
        };
        assert!(
            current
                .clone()
                .end_after_fatal_mismatch(superset, FatalMismatchStopDisposition::KnownFailure,)
                .is_err()
        );
        let TurnAttemptStopCauses::FatalMismatch(different_interrupt) =
            TurnAttemptStopCauses::fatal_mismatch(failure(1))
                .add_fatal_mismatch(failure(2))
                .add_interrupt(proof(2))
                .expect("first interrupt is compatible")
        else {
            panic!("adding an interrupt must stay fatal");
        };
        assert!(
            current
                .end_after_fatal_mismatch(
                    different_interrupt,
                    FatalMismatchStopDisposition::KnownFailure,
                )
                .is_err()
        );
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

    /// INV-004 / INV-006: a successful terminal transition preserves identity
    /// and exact history; rejection returns the unchanged state and input.
    #[test]
    fn terminal_transition_preserves_success_and_rejection_inputs_exactly() {
        let expected = AttemptEnd::AfterCancellation {
            cause: proof(1),
            disposition: CancellationStopDisposition::Ambiguous,
        };
        let ended = cancellation_stopped()
            .end_after_cancellation(proof(1), CancellationStopDisposition::Ambiguous)
            .expect("matching cancellation history may end");
        assert_eq!(ended.id(), attempt_id(1));
        assert_eq!(ended.end(), &expected);

        let current = cancellation_stopped();
        let rejected = AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::KnownFailure,
        };
        let error = current
            .clone()
            .end_without_stop(UnstoppedAttemptDisposition::KnownFailure)
            .unwrap_err();
        assert_eq!(
            error.into_parts(),
            (
                current,
                AttemptedTurnAttemptTransition::End { end: rejected }
            )
        );
    }

    fn all_unstopped_dispositions() -> [UnstoppedAttemptDisposition; 6] {
        [
            UnstoppedAttemptDisposition::TurnCompleted,
            UnstoppedAttemptDisposition::TurnRefused,
            UnstoppedAttemptDisposition::YieldedToDurableWait,
            UnstoppedAttemptDisposition::KnownFailure,
            UnstoppedAttemptDisposition::Lost,
            UnstoppedAttemptDisposition::Ambiguous,
        ]
    }

    fn all_cancellation_dispositions() -> [CancellationStopDisposition; 6] {
        [
            CancellationStopDisposition::TurnCompleted,
            CancellationStopDisposition::TurnRefused,
            CancellationStopDisposition::KnownFailure,
            CancellationStopDisposition::Lost,
            CancellationStopDisposition::Cancelled,
            CancellationStopDisposition::Ambiguous,
        ]
    }

    fn all_fatal_dispositions() -> [FatalMismatchStopDisposition; 3] {
        [
            FatalMismatchStopDisposition::KnownFailure,
            FatalMismatchStopDisposition::Lost,
            FatalMismatchStopDisposition::Ambiguous,
        ]
    }
}
