//! Pinned provider-target turn fact and model-call record values.
//!
//! ADR-0005 is normative. This module models the exact hub-resolved
//! provider/model target pinned as a durable turn fact before any model call
//! exists, and the current call record created from that fact. Resolving a
//! frozen selection against deployment state, call-state transitions,
//! provider-target evidence, outcome-authority transfer, and the turn
//! aggregate's guards are separate later slices. A standalone value is not
//! proof that resolution or aggregate guards held.

use crate::{ModelCallId, TurnId};

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
/// ADR-0005's `Terminal(ModelCallDisposition)` is a separate ended-call
/// record introduced with the transition slice, so terminal state cannot
/// re-enter these variants.
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

impl CurrentModelCall {
    /// Creates one durably prepared call from its turn's pinned target.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the later aggregate slice consumes this sealed entry"
        )
    )]
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
}

#[cfg(test)]
mod tests {
    use super::{
        CurrentModelCall, CurrentModelCallState, PinnedProviderTarget, ProviderModelIdentity,
        ResolvedProviderTarget,
    };
    use crate::test_support::{model_call_id, provider_model_identity, turn_id};
    use uuid::Uuid;

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(provider_model_identity(value))
    }

    fn pinned_target() -> PinnedProviderTarget {
        PinnedProviderTarget::pinned(turn_id(1), target(7))
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
        let call = CurrentModelCall::prepared(model_call_id(3), pinned_target());

        assert_eq!(call.id(), model_call_id(3));
        assert_eq!(call.pinned(), &pinned_target());
        assert_eq!(call.turn(), turn_id(1));
        assert_eq!(call.target(), target(7));
        assert_eq!(call.state(), CurrentModelCallState::Prepared);
    }
}
