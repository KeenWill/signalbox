//! Pinned provider-target turn fact, model-call records, and transitions.
//!
//! ADR-0005 is normative. This module models the exact hub-resolved
//! provider/model target pinned as a durable turn fact before any model call
//! exists, the current call record created from that fact, and the
//! call-local predecessor matrix. Resolving a frozen selection against
//! deployment state, provider-target evidence, outcome-authority transfer,
//! and the turn aggregate's guards are separate later slices. A standalone
//! value is not proof that resolution or aggregate guards held.

use crate::{AppliedInterruptProof, ModelCallId, TurnId};

crate::define_identity!(
    /// Names one provider/model identity in the hub's normalized value space.
    ///
    /// The hub-resolved exact target and trusted provider-reported
    /// observations share this space, so a mismatch stays a typed value
    /// comparison. How raw provider-reported data normalizes into this key,
    /// and provenance beyond ADR-0005's typed baseline observation, remain
    /// the open ADR-0007 questions.
    ProviderModelIdentity
);

/// The exact provider/model target selected by hub resolution.
///
/// The wrapper keeps the hub-resolved role distinct from a provider-reported
/// identity so later evidence handling cannot substitute one for the other.
/// It is never a selection, an alias, a policy, or a fallback set.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedProviderTarget {
    identity: ProviderModelIdentity,
}

impl ResolvedProviderTarget {
    /// Wraps the exact normalized identity that resolution selected.
    pub const fn naming(identity: ProviderModelIdentity) -> Self {
        Self { identity }
    }

    /// Returns the exact normalized identity of this target.
    pub const fn identity(&self) -> ProviderModelIdentity {
        self.identity
    }
}

/// The exact provider/model target pinned as a durable turn fact.
///
/// ADR-0005 pins this fact before the first `ModelCallId` is created and
/// requires every call in the turn to use it. S20 / S21 / INV-014: raw parts
/// cannot claim that a turn pinned a target:
///
/// ```compile_fail
/// use signalbox_domain::{PinnedProviderTarget, ResolvedProviderTarget, TurnId};
///
/// fn raw_parts_are_not_a_pinned_turn_fact(turn: TurnId, target: ResolvedProviderTarget) {
///     let _ = PinnedProviderTarget { turn, target };
/// }
/// ```
///
/// The producer is crate-private and reserved for the later
/// resolution-owning slice, which must validate and resolve the turn's
/// frozen selection before pinning. Resolution failure pins nothing and is
/// recorded as the already-representable attempt and turn failure, so no
/// separate failure entity exists here.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PinnedProviderTarget {
    turn: TurnId,
    target: ResolvedProviderTarget,
}

impl PinnedProviderTarget {
    /// Pins the exact resolved target for one turn.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the later resolution-owning slice consumes this seam"
        )
    )]
    pub(crate) const fn pinned(turn: TurnId, target: ResolvedProviderTarget) -> Self {
        Self { turn, target }
    }

    /// Returns the turn this target is pinned to.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact resolved target every call in the turn must use.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// The terminal physical disposition of one model call.
///
/// The five variants are ADR-0005's exact `ModelCallDisposition` algebra.
/// Which disposition a classification may select, and what each implies for
/// the attempt and turn, are ADR-0004 and ADR-0005 aggregate rules outside
/// this value.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModelCallDisposition {
    /// The provider completed a usable response.
    Completed,
    /// Evidence adequately establishes that no usable response completed.
    KnownFailed,
    /// The provider returned an explicit refusal.
    Refused,
    /// The provider interaction physically ended by cancellation.
    Cancelled,
    /// Evidence cannot establish whether the provider accepted or completed
    /// the request.
    Ambiguous,
}

/// The nonterminal states of one current model call.
///
/// ADR-0005's `Terminal(ModelCallDisposition)` is the separate
/// [`EndedModelCall`] record, so terminal state cannot re-enter these
/// variants.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CurrentModelCallState {
    /// The call exists durably with its exact target; sending is not
    /// authorized.
    Prepared,
    /// Send authorization is durably persisted for the provider boundary.
    InFlight,
    /// Best-effort cancellation of remaining provider work was durably
    /// requested.
    CancellationRequested,
}

/// One current, nonterminal model call.
///
/// The sole entry is the crate-private prepared constructor consuming the
/// turn's [`PinnedProviderTarget`], so target-resolution failure — which
/// pins no fact — can never produce a call, and no field admits a targetless
/// or optional-target call. S20 / S21 / INV-014: a call record cannot be
/// forged around the pinned fact:
///
/// ```compile_fail
/// use signalbox_domain::{
///     CurrentModelCall, CurrentModelCallState, ModelCallId, PinnedProviderTarget,
/// };
///
/// fn a_call_cannot_be_forged(id: ModelCallId, pinned: PinnedProviderTarget) {
///     let _ = CurrentModelCall {
///         id,
///         pinned,
///         state: CurrentModelCallState::Prepared,
///     };
/// }
/// ```
///
/// Call creation is sealed behind the turn aggregate:
///
/// ```compile_fail
/// use signalbox_domain::{CurrentModelCall, ModelCallId, PinnedProviderTarget};
///
/// fn creation_cannot_bypass_the_aggregate(id: ModelCallId, pinned: PinnedProviderTarget) {
///     let _ = CurrentModelCall::prepared(id, pinned);
/// }
/// ```
///
/// # Scope
///
/// This is a call-record component, not an independently persisted
/// aggregate. The turn aggregate owns distinct-call-per-authorization
/// creation, outcome eligibility and authority transfer, correlation with
/// attempt stop causes, and atomic persistence. Requested-selection and
/// context-frontier recording on the durable call record remain with those
/// later slices.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CurrentModelCall {
    id: ModelCallId,
    pinned: PinnedProviderTarget,
    state: CurrentModelCallState,
}

#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the later aggregate slice"
)]
impl CurrentModelCall {
    /// Creates one durably prepared call from its turn's pinned target.
    pub(crate) const fn prepared(id: ModelCallId, pinned: PinnedProviderTarget) -> Self {
        Self {
            id,
            pinned,
            state: CurrentModelCallState::Prepared,
        }
    }

    /// Returns the call identity preserved by every later transition.
    pub const fn id(&self) -> ModelCallId {
        self.id
    }

    /// Borrows the pinned turn fact this call was created from.
    pub const fn pinned(&self) -> &PinnedProviderTarget {
        &self.pinned
    }

    /// Returns the turn whose authorization created this call.
    pub const fn turn(&self) -> TurnId {
        self.pinned.turn()
    }

    /// Returns the exact resolved target recorded on this call.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.pinned.target()
    }

    /// Returns the current nonterminal state.
    pub const fn state(&self) -> CurrentModelCallState {
        self.state
    }

    /// Authorizes crossing the provider boundary from `Prepared`.
    pub(crate) fn begin_in_flight(self) -> Result<Self, CurrentModelCallTransitionError> {
        match self.state {
            CurrentModelCallState::Prepared => Ok(Self {
                state: CurrentModelCallState::InFlight,
                ..self
            }),
            _ => Err(CurrentModelCallTransitionError::new(
                self,
                AttemptedModelCallTransition::BeginInFlight,
            )),
        }
    }

    /// Durably requests best-effort cancellation of remaining provider work.
    ///
    /// ADR-0005's only cancellation-request edge starts at `InFlight`. An
    /// already-requested call and an unsent `Prepared` call are both
    /// rejected unchanged: the durable request exists at most once, and the
    /// unsent call ends through the proof-correlated unsent path instead.
    pub(crate) fn request_cancellation(self) -> Result<Self, CurrentModelCallTransitionError> {
        match self.state {
            CurrentModelCallState::InFlight => Ok(Self {
                state: CurrentModelCallState::CancellationRequested,
                ..self
            }),
            CurrentModelCallState::Prepared | CurrentModelCallState::CancellationRequested => {
                Err(CurrentModelCallTransitionError::new(
                    self,
                    AttemptedModelCallTransition::RequestCancellation,
                ))
            }
        }
    }

    /// Ends with a durably classified disposition when the predecessor
    /// permits it.
    ///
    /// `InFlight` and `CancellationRequested` accept every disposition;
    /// `Prepared` accepts only `KnownFailed`, because an unsent request
    /// cannot complete, refuse, or become ambiguous, and its cancellation
    /// requires the exact applied interrupt proof.
    pub(crate) fn end_classified(
        self,
        disposition: ModelCallDisposition,
    ) -> Result<EndedModelCall, CurrentModelCallTransitionError> {
        let allowed = match self.state {
            CurrentModelCallState::InFlight | CurrentModelCallState::CancellationRequested => true,
            CurrentModelCallState::Prepared => disposition == ModelCallDisposition::KnownFailed,
        };

        if allowed {
            Ok(EndedModelCall {
                id: self.id,
                pinned: self.pinned,
                disposition,
            })
        } else {
            Err(CurrentModelCallTransitionError::new(
                self,
                AttemptedModelCallTransition::EndClassified { disposition },
            ))
        }
    }

    /// Ends an unsent `Prepared` call as `Cancelled` from the exact applied
    /// interrupt proof for this call's turn.
    ///
    /// A proof for a different predecessor is rejected, and every other
    /// current state classifies its evidence through [`Self::end_classified`]
    /// instead.
    pub(crate) fn end_cancelled_unsent(
        self,
        proof: AppliedInterruptProof,
    ) -> Result<EndedModelCall, CurrentModelCallTransitionError> {
        if self.state == CurrentModelCallState::Prepared && proof.predecessor() == self.turn() {
            Ok(EndedModelCall {
                id: self.id,
                pinned: self.pinned,
                disposition: ModelCallDisposition::Cancelled,
            })
        } else {
            Err(CurrentModelCallTransitionError::new(
                self,
                AttemptedModelCallTransition::EndCancelledUnsent { proof },
            ))
        }
    }
}

/// Immutable terminal history for one model call.
///
/// ADR-0005 prohibits every transition out of `Terminal`; late evidence is
/// separate audit/reconciliation evidence. This type exposes no transition
/// back to a current call:
///
/// ```compile_fail
/// use signalbox_domain::EndedModelCall;
///
/// fn a_terminal_call_cannot_go_back_in_flight(ended: EndedModelCall) {
///     let _ = ended.begin_in_flight();
/// }
/// ```
///
/// Terminal history can only be produced by a valid consuming transition:
///
/// ```compile_fail
/// use signalbox_domain::{
///     EndedModelCall, ModelCallDisposition, ModelCallId, PinnedProviderTarget,
/// };
///
/// fn terminal_history_cannot_be_forged(id: ModelCallId, pinned: PinnedProviderTarget) {
///     let _ = EndedModelCall {
///         id,
///         pinned,
///         disposition: ModelCallDisposition::Completed,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EndedModelCall {
    id: ModelCallId,
    pinned: PinnedProviderTarget,
    disposition: ModelCallDisposition,
}

impl EndedModelCall {
    /// Returns the identity preserved from the current call.
    pub const fn id(&self) -> ModelCallId {
        self.id
    }

    /// Borrows the pinned turn fact preserved from the current call.
    pub const fn pinned(&self) -> &PinnedProviderTarget {
        &self.pinned
    }

    /// Returns the turn whose authorization created this call.
    pub const fn turn(&self) -> TurnId {
        self.pinned.turn()
    }

    /// Returns the exact resolved target recorded on this call.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.pinned.target()
    }

    /// Returns the terminal physical disposition.
    pub const fn disposition(&self) -> ModelCallDisposition {
        self.disposition
    }
}

/// The transition input returned when a current call rejects it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the later aggregate slice"
)]
pub(crate) enum AttemptedModelCallTransition {
    /// Send authorization was requested outside `Prepared`.
    BeginInFlight,
    /// Best-effort cancellation was requested outside `InFlight`.
    RequestCancellation,
    /// The classified disposition does not match the current state.
    EndClassified {
        /// The complete requested terminal disposition.
        disposition: ModelCallDisposition,
    },
    /// The unsent-cancellation proof or current state does not match.
    EndCancelledUnsent {
        /// The exact proof that was offered.
        proof: AppliedInterruptProof,
    },
}

/// A rejected transition with the unchanged current call and exact input.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the later aggregate slice"
)]
pub(crate) struct CurrentModelCallTransitionError {
    rejected: Box<(CurrentModelCall, AttemptedModelCallTransition)>,
}

#[allow(
    dead_code,
    reason = "sealed transition seam is consumed by the later aggregate slice"
)]
impl CurrentModelCallTransitionError {
    fn new(current: CurrentModelCall, attempted: AttemptedModelCallTransition) -> Self {
        Self {
            rejected: Box::new((current, attempted)),
        }
    }

    /// Borrows the unchanged current call.
    pub(crate) fn current(&self) -> &CurrentModelCall {
        &self.rejected.0
    }

    /// Borrows the rejected transition input.
    pub(crate) fn attempted(&self) -> &AttemptedModelCallTransition {
        &self.rejected.1
    }

    /// Returns the unchanged call and rejected transition input.
    pub(crate) fn into_parts(self) -> (CurrentModelCall, AttemptedModelCallTransition) {
        *self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttemptedModelCallTransition, CurrentModelCall, CurrentModelCallState, EndedModelCall,
        ModelCallDisposition, PinnedProviderTarget, ProviderModelIdentity, ResolvedProviderTarget,
    };
    use crate::AppliedInterruptProof;
    use crate::applied_interrupt::test_applied_interrupt_proof;
    use crate::test_support::{command_id, model_call_id, provider_model_identity, turn_id};
    use uuid::Uuid;

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(provider_model_identity(value))
    }

    fn pinned_target() -> PinnedProviderTarget {
        PinnedProviderTarget::pinned(turn_id(1), target(7))
    }

    fn proof_for(predecessor: u128) -> AppliedInterruptProof {
        test_applied_interrupt_proof(command_id(9), turn_id(predecessor))
    }

    fn prepared() -> CurrentModelCall {
        CurrentModelCall::prepared(model_call_id(3), pinned_target())
    }

    fn in_flight() -> CurrentModelCall {
        prepared().begin_in_flight().expect("Prepared may send")
    }

    fn cancellation_requested() -> CurrentModelCall {
        in_flight()
            .request_cancellation()
            .expect("InFlight may request cancellation")
    }

    fn all_dispositions() -> [ModelCallDisposition; 5] {
        [
            ModelCallDisposition::Completed,
            ModelCallDisposition::KnownFailed,
            ModelCallDisposition::Refused,
            ModelCallDisposition::Cancelled,
            ModelCallDisposition::Ambiguous,
        ]
    }

    #[test]
    fn provider_model_identities_expose_their_uuid_values() {
        let uuid = Uuid::from_u128(1);

        assert_eq!(
            provider_model_identity(1),
            ProviderModelIdentity::from_uuid(uuid)
        );
        assert_ne!(provider_model_identity(1), provider_model_identity(2));
        assert_eq!(provider_model_identity(1).as_uuid(), &uuid);
        assert_eq!(provider_model_identity(1).into_uuid(), uuid);
    }

    /// S20 / S21 / INV-014: the pinned turn fact preserves its exact turn
    /// and target, and any target or turn difference is a different fact.
    #[test]
    fn pinned_fact_preserves_the_exact_turn_and_target() {
        let pinned = pinned_target();

        assert_eq!(pinned.turn(), turn_id(1));
        assert_eq!(pinned.target(), target(7));
        assert_eq!(pinned.target().identity(), provider_model_identity(7));
        assert_eq!(pinned, PinnedProviderTarget::pinned(turn_id(1), target(7)));
        assert_ne!(pinned, PinnedProviderTarget::pinned(turn_id(1), target(8)));
        assert_ne!(pinned, PinnedProviderTarget::pinned(turn_id(2), target(7)));
    }

    /// S20 / INV-014: a prepared call records its distinct identity and its
    /// turn's exact pinned target at creation.
    #[test]
    fn prepared_call_records_the_pinned_target_at_creation() {
        let call = prepared();

        assert_eq!(call.id(), model_call_id(3));
        assert_eq!(call.pinned(), &pinned_target());
        assert_eq!(call.turn(), turn_id(1));
        assert_eq!(call.target(), target(7));
        assert_eq!(call.state(), CurrentModelCallState::Prepared);
    }

    /// S02 / INV-004 / INV-006 / INV-014: send authorization is valid only
    /// from `Prepared` and preserves the call identity and pinned target.
    #[test]
    fn begin_in_flight_accepts_only_prepared_and_preserves_the_record() {
        let call = in_flight();
        assert_eq!(call.id(), model_call_id(3));
        assert_eq!(call.pinned(), &pinned_target());
        assert_eq!(call.state(), CurrentModelCallState::InFlight);

        for current in [in_flight(), cancellation_requested()] {
            let error = current.clone().begin_in_flight().unwrap_err();
            assert_eq!(
                error.into_parts(),
                (current, AttemptedModelCallTransition::BeginInFlight)
            );
        }
    }

    /// S07 / S21 / INV-006: best-effort cancellation request is valid only
    /// from `InFlight`; unsent and already-requested calls are rejected
    /// unchanged.
    #[test]
    fn cancellation_request_accepts_only_in_flight_calls() {
        let requested = cancellation_requested();
        assert_eq!(
            requested.state(),
            CurrentModelCallState::CancellationRequested
        );

        for current in [prepared(), cancellation_requested()] {
            let error = current.clone().request_cancellation().unwrap_err();
            assert_eq!(error.current(), &current);
            assert_eq!(
                error.attempted(),
                &AttemptedModelCallTransition::RequestCancellation
            );
        }
    }

    /// S04 / S21 / INV-006 / INV-014 / INV-029: `Prepared` classifies only
    /// known failure without a proof; cancellation of the unsent call
    /// requires the exact applied interrupt proof for this call's turn.
    #[test]
    fn prepared_terminal_matrix_requires_the_exact_proof_for_cancellation() {
        for disposition in all_dispositions() {
            assert_eq!(
                prepared().end_classified(disposition).is_ok(),
                disposition == ModelCallDisposition::KnownFailed,
                "unexpected Prepared classification result for {disposition:?}"
            );
        }

        let ended = prepared()
            .end_cancelled_unsent(proof_for(1))
            .expect("the exact proof cancels the unsent call");
        assert_eq!(ended.disposition(), ModelCallDisposition::Cancelled);

        let error = prepared().end_cancelled_unsent(proof_for(2)).unwrap_err();
        assert_eq!(
            error.into_parts(),
            (
                prepared(),
                AttemptedModelCallTransition::EndCancelledUnsent {
                    proof: proof_for(2)
                }
            )
        );
        for current in [in_flight(), cancellation_requested()] {
            assert!(current.end_cancelled_unsent(proof_for(1)).is_err());
        }
    }

    /// S02 / S04 / S21 / S23 / INV-006 / INV-025: issued calls accept every
    /// classified disposition, and ambiguity stays its own disposition
    /// instead of being coerced to failure.
    #[test]
    fn issued_calls_accept_every_classified_disposition() {
        for current in [in_flight(), cancellation_requested()] {
            for disposition in all_dispositions() {
                let ended = current
                    .clone()
                    .end_classified(disposition)
                    .expect("issued calls classify every disposition");
                assert_eq!(ended.disposition(), disposition);
            }
        }
    }

    /// INV-004 / INV-014: terminal history preserves the identity, turn, and
    /// exact pinned target of the current call it consumed.
    #[test]
    fn terminal_history_preserves_identity_and_exact_target() {
        let ended: EndedModelCall = in_flight()
            .end_classified(ModelCallDisposition::Completed)
            .expect("InFlight may complete");

        assert_eq!(ended.id(), model_call_id(3));
        assert_eq!(ended.pinned(), &pinned_target());
        assert_eq!(ended.turn(), turn_id(1));
        assert_eq!(ended.target(), target(7));
        assert_eq!(ended.disposition(), ModelCallDisposition::Completed);
    }
}
