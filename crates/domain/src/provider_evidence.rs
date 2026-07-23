//! Provider-target observations, evidence records, and mismatch authority.
//!
//! docs/spec/model-call-execution.md is normative. This module models the
//! typed observation payload, the evidence record keyed by
//! [`ProviderTargetEvidenceId`] with identifier-replay and reuse
//! boundaries, the completed-call mismatch invalidation that is unique by
//! invalidated call, and the validating producers for sealed mismatch facts
//! that bind each [`ProviderTargetMismatchFailureRef`] to its exact
//! validated call effect.
//! Trust classification of raw provider-reported data, outcome eligibility
//! and authority transfer, the aggregate's classification precedence, and
//! persistence are separate later slices; a value here does not prove those
//! guards held.

use std::collections::BTreeMap;

use crate::{
    CurrentModelCall, CurrentModelCallState, EndedModelCall, ModelCallDisposition, ModelCallId,
    ProviderModelIdentity, ProviderTargetEvidenceId, ProviderTargetMismatchFailureRef,
    ResolvedProviderTarget,
};

/// The typed payload of one trusted provider-target observation.
///
/// The two variants are the exact `ProviderTargetObservation` algebra in
/// docs/spec/model-call-execution.md. Whether a reported identity is
/// trusted, and how raw provider-reported data normalizes into
/// [`ProviderModelIdentity`], are boundary scope and an open edge recorded
/// in docs/spec/identity-and-commands.md; an absent reported identity is
/// not representable as either variant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProviderTargetObservation {
    /// The reported identity matches the call's exact resolved target.
    MatchesResolvedTarget {
        /// The provider-reported identity.
        reported: ProviderModelIdentity,
    },
    /// The reported identity mismatches the call's exact resolved target.
    Mismatch {
        /// The provider-reported identity.
        reported: ProviderModelIdentity,
    },
}

/// The call-state effect already validated with one fatal mismatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderTargetMismatchEffectView {
    /// A nonterminal call is to be classified physically known failed.
    ClassifyNonterminalKnownFailed,
    /// A terminally ambiguous call keeps its physical disposition while its
    /// turn-level ambiguity is resolved.
    ResolveTerminalAmbiguity,
    /// A completed current-authority call is to be invalidated without
    /// rewriting it.
    PreserveCompletedInvalidation,
}

/// One trusted fatal mismatch paired with its exact validated call effect.
///
/// Only this module's evidence-correlation boundaries construct the value.
/// The sealed pairing prevents later closure logic from applying a failure
/// for call A to call B or inventing a different timing-branch effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AppliedProviderTargetMismatch {
    failure: ProviderTargetMismatchFailureRef,
    affected_call: ModelCallId,
    effect: ProviderTargetMismatchEffectView,
}

impl AppliedProviderTargetMismatch {
    const fn nonterminal(evidence: ProviderTargetEvidenceId, call: ModelCallId) -> Self {
        Self {
            failure: ProviderTargetMismatchFailureRef::nonterminal_call_observation(evidence),
            affected_call: call,
            effect: ProviderTargetMismatchEffectView::ClassifyNonterminalKnownFailed,
        }
    }

    const fn terminal_ambiguity_resolution(
        evidence: ProviderTargetEvidenceId,
        call: ModelCallId,
    ) -> Self {
        Self {
            failure: ProviderTargetMismatchFailureRef::terminal_ambiguity_resolution(evidence),
            affected_call: call,
            effect: ProviderTargetMismatchEffectView::ResolveTerminalAmbiguity,
        }
    }

    const fn completed_invalidation(call: ModelCallId) -> Self {
        Self {
            failure: ProviderTargetMismatchFailureRef::terminal_call_invalidation(call),
            affected_call: call,
            effect: ProviderTargetMismatchEffectView::PreserveCompletedInvalidation,
        }
    }

    pub(crate) const fn failure(self) -> ProviderTargetMismatchFailureRef {
        self.failure
    }

    pub(crate) const fn affected_call(self) -> ModelCallId {
        self.affected_call
    }

    pub(crate) const fn effect(self) -> ProviderTargetMismatchEffectView {
        self.effect
    }

    #[cfg(test)]
    pub(crate) const fn test_nonterminal(
        evidence: ProviderTargetEvidenceId,
        call: ModelCallId,
    ) -> Self {
        Self::nonterminal(evidence, call)
    }

    #[cfg(test)]
    pub(crate) const fn test_terminal_ambiguity_resolution(
        evidence: ProviderTargetEvidenceId,
        call: ModelCallId,
    ) -> Self {
        Self::terminal_ambiguity_resolution(evidence, call)
    }

    #[cfg(test)]
    pub(crate) const fn test_completed_invalidation(call: ModelCallId) -> Self {
        Self::completed_invalidation(call)
    }
}

/// The model call identity and its exact resolved target, read together
/// from one canonical call record.
///
/// docs/spec/model-call-execution.md derives the target from the canonical
/// call record inside the serialized transition. This value is the only
/// way to hand a `(call, target)` pair to the recording boundary, and it
/// can be built outside this module solely from a real [`CurrentModelCall`]
/// or [`EndedModelCall`]. That prevents recording call A's observation
/// against call B's target, which would durably accept a mislabeled match
/// as trusted evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalCallTarget {
    call: ModelCallId,
    target: ResolvedProviderTarget,
}

impl From<&CurrentModelCall> for CanonicalCallTarget {
    fn from(call: &CurrentModelCall) -> Self {
        Self {
            call: call.id(),
            target: call.target(),
        }
    }
}

impl From<&EndedModelCall> for CanonicalCallTarget {
    fn from(call: &EndedModelCall) -> Self {
        Self {
            call: call.id(),
            target: call.target(),
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
impl CanonicalCallTarget {
    /// Returns the canonical call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact resolved target read from the canonical record.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// One recorded provider-target observation for one model call.
///
/// The record deliberately carries no copy of the exact target:
/// docs/spec/model-call-execution.md derives the target from the canonical
/// call record inside the serialized transition. S21 / INV-014: raw parts
/// cannot claim a recorded evidence fact:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ModelCallId, ProviderTargetEvidence, ProviderTargetEvidenceId,
///     ProviderTargetObservation,
/// };
///
/// fn raw_parts_are_not_recorded_evidence(
///     id: ProviderTargetEvidenceId,
///     call: ModelCallId,
///     observation: ProviderTargetObservation,
/// ) {
///     let _ = ProviderTargetEvidence {
///         id,
///         call,
///         observation,
///     };
/// }
/// ```
///
/// The sole producer is the crate-private [`ProviderTargetEvidenceLog`]
/// recording boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProviderTargetEvidence {
    id: ProviderTargetEvidenceId,
    call: ModelCallId,
    observation: ProviderTargetObservation,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetEvidence {
    /// Returns the evidence identifier this record is keyed by.
    pub const fn id(&self) -> ProviderTargetEvidenceId {
        self.id
    }

    /// Returns the model call this observation was recorded for.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the typed observation payload.
    pub const fn observation(&self) -> ProviderTargetObservation {
        self.observation
    }

    /// Correlates this record as the first trusted mismatch observation on a
    /// nonterminal call and produces the sealed fatal-mismatch fact.
    ///
    /// Nonterminality is established by the current-call type, and the call
    /// must have crossed send authorization: the mismatch edge in
    /// docs/spec/model-call-execution.md starts at `InFlight` or
    /// `CancellationRequested`, because an unsent `Prepared` call has no
    /// provider interaction that could report a target. Outcome eligibility
    /// and the atomic `Terminal(KnownFailed)` classification are the
    /// aggregate's transition.
    pub(crate) fn nonterminal_call_mismatch_fact(
        &self,
        call: &CurrentModelCall,
    ) -> Result<AppliedProviderTargetMismatch, ProviderTargetMismatchCorrelationError> {
        if call.state() == CurrentModelCallState::Prepared {
            return Err(ProviderTargetMismatchCorrelationError::CallIsUnsent);
        }
        self.correlated_mismatch(call.id(), call.target())?;
        Ok(AppliedProviderTargetMismatch::nonterminal(
            self.id,
            call.id(),
        ))
    }

    /// Correlates this record as mismatch evidence resolving a call that
    /// already ended `Ambiguous` and produces the sealed fatal-mismatch fact.
    ///
    /// The terminal physical disposition stays unchanged by construction:
    /// this takes the ended record immutably and returns only the sealed fact.
    pub(crate) fn terminal_ambiguity_resolution_fact(
        &self,
        call: &EndedModelCall,
    ) -> Result<AppliedProviderTargetMismatch, ProviderTargetMismatchCorrelationError> {
        if call.disposition() != ModelCallDisposition::Ambiguous {
            return Err(ProviderTargetMismatchCorrelationError::CallIsNotAmbiguous {
                disposition: call.disposition(),
            });
        }
        self.correlated_mismatch(call.id(), call.target())?;
        Ok(AppliedProviderTargetMismatch::terminal_ambiguity_resolution(self.id, call.id()))
    }

    fn correlated_mismatch(
        &self,
        call: ModelCallId,
        target: ResolvedProviderTarget,
    ) -> Result<ProviderModelIdentity, ProviderTargetMismatchCorrelationError> {
        if self.call != call {
            return Err(
                ProviderTargetMismatchCorrelationError::EvidenceForDifferentCall {
                    evidence_call: self.call,
                    call,
                },
            );
        }
        let ProviderTargetObservation::Mismatch { reported } = self.observation else {
            return Err(
                ProviderTargetMismatchCorrelationError::ObservationIsNotMismatch {
                    observation: self.observation,
                },
            );
        };
        if reported == target.identity() {
            return Err(
                ProviderTargetMismatchCorrelationError::ReportedIdentityMatchesTarget { reported },
            );
        }
        Ok(reported)
    }
}

/// Reports why evidence cannot correlate into mismatch-failure authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
pub(crate) enum ProviderTargetMismatchCorrelationError {
    /// The evidence was recorded for a different model call.
    EvidenceForDifferentCall {
        /// The call on the evidence record.
        evidence_call: ModelCallId,
        /// The call offered for correlation.
        call: ModelCallId,
    },
    /// The typed payload is not a mismatch observation.
    ObservationIsNotMismatch {
        /// The exact recorded payload.
        observation: ProviderTargetObservation,
    },
    /// The claimed mismatch reports the call's own exact target.
    ReportedIdentityMatchesTarget {
        /// The reported identity equal to the target.
        reported: ProviderModelIdentity,
    },
    /// Ambiguity resolution was offered for a non-ambiguous terminal call.
    CallIsNotAmbiguous {
        /// The exact terminal disposition.
        disposition: ModelCallDisposition,
    },
    /// A nonterminal mismatch was offered for an unsent `Prepared` call.
    CallIsUnsent,
}

/// The durable provider-target evidence records keyed by identifier.
///
/// docs/spec/model-call-execution.md: evidence-identifier lookup precedes
/// current-state validation. This value owns that keyed boundary and the
/// payload's consistency with the exact target derived from the canonical
/// call record: recording with a fresh identifier validates and appends,
/// replay of the same identifier with the structurally equal call and
/// payload returns the recorded result, and reuse with a different call or
/// payload is rejected without change. What a recorded observation then
/// does to call, attempt, and turn state is the aggregate's serialized
/// transition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderTargetEvidenceLog {
    records: BTreeMap<ProviderTargetEvidenceId, ProviderTargetEvidence>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetEvidenceLog {
    /// Creates an empty evidence log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the recorded evidence for this identifier, if any.
    pub fn lookup(&self, id: ProviderTargetEvidenceId) -> Option<&ProviderTargetEvidence> {
        self.records.get(&id)
    }

    /// Records one observation, replaying an equal record idempotently.
    ///
    /// The call identity and its exact target arrive paired in a
    /// [`CanonicalCallTarget`] read from one canonical call record, so a
    /// caller cannot cross-wire call A with call B's target; the target is
    /// a validation input, never copied into the record. Identifier lookup
    /// happens first: an identical identifier/call/payload replay returns
    /// the already-recorded result, and identifier reuse with a different
    /// call or payload is rejected with the unchanged existing record and
    /// the exact rejected input. A fresh identifier is durably recorded
    /// only when the claimed variant is consistent with the exact target —
    /// a match must report it and a mismatch must not.
    pub(crate) fn record(
        &mut self,
        id: ProviderTargetEvidenceId,
        canonical: CanonicalCallTarget,
        observation: ProviderTargetObservation,
    ) -> Result<ProviderTargetEvidenceRecording, ProviderTargetEvidenceRecordingError> {
        let call = canonical.call;
        let target = canonical.target;
        if let Some(existing) = self.records.get(&id) {
            return if existing.call == call && existing.observation == observation {
                Ok(ProviderTargetEvidenceRecording::Replayed(*existing))
            } else {
                Err(ProviderTargetEvidenceRecordingError::IdentifierReuse(
                    ProviderTargetEvidenceReuseError {
                        existing: *existing,
                        requested_call: call,
                        requested_observation: observation,
                    },
                ))
            };
        }

        let consistent = match observation {
            ProviderTargetObservation::MatchesResolvedTarget { reported } => {
                reported == target.identity()
            }
            ProviderTargetObservation::Mismatch { reported } => reported != target.identity(),
        };
        if !consistent {
            return Err(
                ProviderTargetEvidenceRecordingError::ObservationContradictsTarget {
                    target,
                    observation,
                },
            );
        }

        let evidence = ProviderTargetEvidence {
            id,
            call,
            observation,
        };
        self.records.insert(id, evidence);
        Ok(ProviderTargetEvidenceRecording::First(evidence))
    }
}

/// Reports why an observation could not be durably recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
pub(crate) enum ProviderTargetEvidenceRecordingError {
    /// The identifier is already recorded with a different call or payload.
    IdentifierReuse(ProviderTargetEvidenceReuseError),
    /// The claimed variant contradicts the canonical call record's target.
    ObservationContradictsTarget {
        /// The exact target derived from the canonical call record.
        target: ResolvedProviderTarget,
        /// The exact contradictory payload that was rejected.
        observation: ProviderTargetObservation,
    },
}

/// One accepted evidence-recording outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
pub(crate) enum ProviderTargetEvidenceRecording {
    /// A fresh identifier durably recorded this evidence.
    First(ProviderTargetEvidence),
    /// A structurally equal replay returned the recorded result.
    Replayed(ProviderTargetEvidence),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetEvidenceRecording {
    /// Returns the recorded evidence for either outcome.
    pub(crate) const fn evidence(&self) -> ProviderTargetEvidence {
        match self {
            Self::First(evidence) | Self::Replayed(evidence) => *evidence,
        }
    }
}

/// A rejected identifier reuse with a different call or payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
pub(crate) struct ProviderTargetEvidenceReuseError {
    existing: ProviderTargetEvidence,
    requested_call: ModelCallId,
    requested_observation: ProviderTargetObservation,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recording seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetEvidenceReuseError {
    /// Returns the unchanged existing record for the identifier.
    pub(crate) const fn existing(&self) -> ProviderTargetEvidence {
        self.existing
    }

    /// Returns the rejected call association.
    pub(crate) const fn requested_call(&self) -> ModelCallId {
        self.requested_call
    }

    /// Returns the rejected typed payload.
    pub(crate) const fn requested_observation(&self) -> ProviderTargetObservation {
        self.requested_observation
    }
}

/// The typed invalidation of one completed current-authority call.
///
/// docs/spec/model-call-execution.md makes this value unique by
/// `invalidated_call`: the first valid mismatch fixes it, structurally
/// equal evidence replay is idempotent, and later observations cannot
/// duplicate or replace it. The value carries no exact target and no
/// authority generation; both derive from the canonical call and transfer
/// chain inside the serialized transition. S21 / INV-014: raw identities
/// cannot claim an invalidation:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ModelCallId, ProviderTargetEvidenceId, ProviderTargetMismatchInvalidation,
/// };
///
/// fn raw_ids_are_not_an_invalidation(
///     invalidated_call: ModelCallId,
///     first_mismatch_evidence: ProviderTargetEvidenceId,
/// ) {
///     let _ = ProviderTargetMismatchInvalidation {
///         invalidated_call,
///         first_mismatch_evidence,
///     };
/// }
/// ```
///
/// The sole producer is the crate-private [`ProviderTargetMismatchInvalidationLog`],
/// which owns the per-call uniqueness: it looks up the at most one existing
/// invalidation for the call itself and validates the canonical completed
/// call and correlated mismatch evidence against it, so admission cannot
/// depend on a caller remembering to pass that lookup. Whether the call is
/// still outcome-authoritative when the transition commits, and that it
/// belongs to the active turn, are the aggregate's checks.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProviderTargetMismatchInvalidation {
    invalidated_call: ModelCallId,
    first_mismatch_evidence: ProviderTargetEvidenceId,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetMismatchInvalidation {
    /// Returns the completed call whose material became unusable.
    pub const fn invalidated_call(&self) -> ModelCallId {
        self.invalidated_call
    }

    /// Returns the exact first mismatch evidence that fixed this value.
    pub const fn first_mismatch_evidence(&self) -> ProviderTargetEvidenceId {
        self.first_mismatch_evidence
    }

    /// Admits mismatch evidence against one completed call and the at most
    /// one existing invalidation for it.
    ///
    /// The first valid correlated mismatch constructs the unique value; a
    /// structurally equal evidence replay returns the existing value; any
    /// later observation, cross-wired record, non-mismatch payload, or
    /// non-completed call is rejected without relabeling.
    ///
    /// This is module-private: the crate-visible admission boundary is
    /// [`ProviderTargetMismatchInvalidationLog`], which supplies `existing`
    /// from its own per-call lookup so uniqueness cannot depend on a caller
    /// remembering to perform it.
    fn admit(
        existing: Option<&Self>,
        call: &EndedModelCall,
        evidence: &ProviderTargetEvidence,
    ) -> Result<AdmittedProviderTargetMismatchInvalidation, ProviderTargetMismatchInvalidationError>
    {
        if call.disposition() != ModelCallDisposition::Completed {
            return Err(ProviderTargetMismatchInvalidationError::CallNotCompleted {
                disposition: call.disposition(),
            });
        }
        evidence
            .correlated_mismatch(call.id(), call.target())
            .map_err(ProviderTargetMismatchInvalidationError::Correlation)?;

        match existing {
            None => Ok(AdmittedProviderTargetMismatchInvalidation::First(Self {
                invalidated_call: call.id(),
                first_mismatch_evidence: evidence.id(),
            })),
            Some(existing) if existing.invalidated_call != call.id() => Err(
                ProviderTargetMismatchInvalidationError::ExistingInvalidationForDifferentCall {
                    existing: *existing,
                    call: call.id(),
                },
            ),
            Some(existing) if existing.first_mismatch_evidence == evidence.id() => Ok(
                AdmittedProviderTargetMismatchInvalidation::Replayed(*existing),
            ),
            Some(existing) => Err(
                ProviderTargetMismatchInvalidationError::LaterObservationCannotReplace {
                    existing: *existing,
                    later_evidence: evidence.id(),
                },
            ),
        }
    }

    /// Produces the sealed fatal-mismatch fact this invalidation requires.
    pub(crate) const fn fatal_mismatch_fact(&self) -> AppliedProviderTargetMismatch {
        AppliedProviderTargetMismatch::completed_invalidation(self.invalidated_call)
    }
}

/// The durable completed-call mismatch invalidations keyed by invalidated
/// call.
///
/// docs/spec/model-call-execution.md makes the invalidation unique by
/// `invalidated_call`. This value owns that per-call uniqueness so
/// admission cannot depend on a caller remembering to look up the existing
/// invalidation: the log itself finds the at most one existing value for
/// the call, the first valid correlated mismatch fixes the entry, a
/// structurally equal evidence replay returns it, and any later observation
/// for the same call is rejected without duplicating or replacing the fixed
/// value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderTargetMismatchInvalidationLog {
    invalidations: BTreeMap<ModelCallId, ProviderTargetMismatchInvalidation>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "admission seam is consumed by the later aggregate slice"
    )
)]
impl ProviderTargetMismatchInvalidationLog {
    /// Creates an empty invalidation log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the recorded invalidation for this call, if any.
    pub fn lookup(&self, call: ModelCallId) -> Option<&ProviderTargetMismatchInvalidation> {
        self.invalidations.get(&call)
    }

    /// Admits mismatch evidence against one completed call, enforcing the
    /// per-call uniqueness from its own keyed lookup.
    ///
    /// The first valid correlated mismatch durably fixes the invalidation
    /// for the call; a structurally equal evidence replay returns it; any
    /// later observation for the same call, cross-wired record, non-mismatch
    /// payload, or non-completed call is rejected without changing the log.
    pub(crate) fn admit(
        &mut self,
        call: &EndedModelCall,
        evidence: &ProviderTargetEvidence,
    ) -> Result<AdmittedProviderTargetMismatchInvalidation, ProviderTargetMismatchInvalidationError>
    {
        let existing = self.invalidations.get(&call.id());
        let admitted = ProviderTargetMismatchInvalidation::admit(existing, call, evidence)?;
        if let AdmittedProviderTargetMismatchInvalidation::First(invalidation) = admitted {
            self.invalidations.insert(call.id(), invalidation);
        }
        Ok(admitted)
    }
}

/// One accepted invalidation-admission outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
pub(crate) enum AdmittedProviderTargetMismatchInvalidation {
    /// The first valid mismatch fixed the unique value.
    First(ProviderTargetMismatchInvalidation),
    /// A structurally equal evidence replay returned the existing value.
    Replayed(ProviderTargetMismatchInvalidation),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
impl AdmittedProviderTargetMismatchInvalidation {
    /// Returns the unique invalidation for either outcome.
    pub(crate) const fn invalidation(&self) -> ProviderTargetMismatchInvalidation {
        match self {
            Self::First(invalidation) | Self::Replayed(invalidation) => *invalidation,
        }
    }
}

/// Reports why mismatch evidence cannot invalidate a completed call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "trusted producer seam is consumed by the later aggregate slice"
    )
)]
pub(crate) enum ProviderTargetMismatchInvalidationError {
    /// The offered terminal call did not end `Completed`.
    CallNotCompleted {
        /// The exact terminal disposition.
        disposition: ModelCallDisposition,
    },
    /// The evidence does not correlate as a mismatch for this call.
    Correlation(ProviderTargetMismatchCorrelationError),
    /// The caller paired the call with another call's invalidation.
    ExistingInvalidationForDifferentCall {
        /// The unchanged existing invalidation.
        existing: ProviderTargetMismatchInvalidation,
        /// The completed call that was offered.
        call: ModelCallId,
    },
    /// A later observation cannot duplicate or replace the fixed value.
    LaterObservationCannotReplace {
        /// The unchanged existing invalidation.
        existing: ProviderTargetMismatchInvalidation,
        /// The exact later evidence that was rejected.
        later_evidence: ProviderTargetEvidenceId,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        AdmittedProviderTargetMismatchInvalidation, CanonicalCallTarget, ProviderTargetEvidence,
        ProviderTargetEvidenceLog, ProviderTargetEvidenceRecording,
        ProviderTargetEvidenceRecordingError, ProviderTargetMismatchCorrelationError,
        ProviderTargetMismatchEffectView, ProviderTargetMismatchInvalidation,
        ProviderTargetMismatchInvalidationError, ProviderTargetMismatchInvalidationLog,
        ProviderTargetObservation,
    };
    use crate::test_support::{
        context_frontier_id, model_call_id, provider_model_identity,
        provider_target_evidence_id as evidence_id, semantic_transcript_entry_id, session_id,
        turn_attempt_id, turn_id,
    };
    use crate::{
        CurrentModelCall, CurrentModelCallState, EndedModelCall, ModelCallDisposition,
        PinnedProviderTarget, ProviderTargetMismatchFailureKind, ResolvedContextFrontierSnapshot,
        ResolvedProviderTarget, SemanticTranscriptEntryRef,
    };

    const TARGET_IDENTITY: u128 = 7;
    /// The one call most tests record evidence for; helpers that omit a call
    /// seed act on this call.
    const SUBJECT_CALL: u128 = 2;
    /// The canonical reported identity that mismatches [`TARGET_IDENTITY`].
    const MISMATCHING_IDENTITY: u128 = 8;

    fn pinned_target() -> PinnedProviderTarget {
        PinnedProviderTarget::pinned(
            turn_id(1),
            ResolvedProviderTarget::naming(provider_model_identity(TARGET_IDENTITY)),
        )
    }

    fn frontier_snapshot() -> ResolvedContextFrontierSnapshot {
        ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(1),
            context_frontier_id(1),
            vec![SemanticTranscriptEntryRef::from_source(
                session_id(1),
                semantic_transcript_entry_id(1),
            )],
        )
        .expect("test frontier contains one exact semantic entry")
    }

    fn current_call(call: u128) -> CurrentModelCall {
        CurrentModelCall::prepared(
            model_call_id(call),
            turn_attempt_id(6),
            crate::FrozenModelSelection::Direct(crate::test_support::direct(5)),
            pinned_target(),
            &frontier_snapshot(),
        )
        .begin_in_flight()
        .expect("Prepared may send")
    }

    fn ended_call(call: u128, disposition: ModelCallDisposition) -> EndedModelCall {
        current_call(call)
            .end_classified(disposition)
            .expect("issued calls classify every disposition")
    }

    fn target() -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(provider_model_identity(TARGET_IDENTITY))
    }

    fn mismatch(reported: u128) -> ProviderTargetObservation {
        ProviderTargetObservation::Mismatch {
            reported: provider_model_identity(reported),
        }
    }

    /// Pairs a call identity with a target the way the canonical call
    /// record does, so tests can still exercise cross-wired combinations the
    /// public `From` conversions never produce.
    fn canonical(call: u128, target: ResolvedProviderTarget) -> CanonicalCallTarget {
        CanonicalCallTarget {
            call: model_call_id(call),
            target,
        }
    }

    fn record_against(
        target: ResolvedProviderTarget,
        id: u128,
        call: u128,
        observation: ProviderTargetObservation,
    ) -> ProviderTargetEvidence {
        ProviderTargetEvidenceLog::new()
            .record(evidence_id(id), canonical(call, target), observation)
            .expect("a consistent fresh identifier records")
            .evidence()
    }

    /// Mismatch evidence recorded for the subject call, reporting the
    /// canonical mismatching identity.
    fn recorded_mismatch(evidence: u128) -> ProviderTargetEvidence {
        recorded_mismatch_for_call(evidence, SUBJECT_CALL)
    }

    /// Mismatch evidence recorded for the identified call instead of the
    /// subject call.
    fn recorded_mismatch_for_call(evidence: u128, call: u128) -> ProviderTargetEvidence {
        record_against(target(), evidence, call, mismatch(MISMATCHING_IDENTITY))
    }

    /// Mismatch evidence for the subject call reporting the identified
    /// non-target identity instead of the canonical one.
    fn recorded_mismatch_reporting(evidence: u128, reported: u128) -> ProviderTargetEvidence {
        record_against(target(), evidence, SUBJECT_CALL, mismatch(reported))
    }

    /// S21 / INV-014: identifier lookup precedes any other validation; a
    /// fresh consistent record appends, an equal replay returns the
    /// recorded result, and reuse with a different call or payload is
    /// rejected unchanged.
    #[test]
    fn evidence_identifier_replay_and_reuse_boundaries_are_exact() {
        let mut log = ProviderTargetEvidenceLog::new();
        let first = log
            .record(evidence_id(1), canonical(2, target()), mismatch(8))
            .unwrap();
        let ProviderTargetEvidenceRecording::First(evidence) = first else {
            panic!("a fresh identifier must record first");
        };
        assert_eq!(evidence.id(), evidence_id(1));
        assert_eq!(evidence.call(), model_call_id(2));
        assert_eq!(evidence.observation(), mismatch(8));
        assert_eq!(log.lookup(evidence_id(1)), Some(&evidence));

        assert_eq!(
            log.record(evidence_id(1), canonical(2, target()), mismatch(8)),
            Ok(ProviderTargetEvidenceRecording::Replayed(evidence))
        );

        let other_call = log
            .record(evidence_id(1), canonical(3, target()), mismatch(8))
            .unwrap_err();
        let ProviderTargetEvidenceRecordingError::IdentifierReuse(reuse) = other_call else {
            panic!("identifier reuse must be rejected as reuse");
        };
        assert_eq!(reuse.existing(), evidence);
        assert_eq!(reuse.requested_call(), model_call_id(3));
        assert_eq!(reuse.requested_observation(), mismatch(8));

        let other_payload = ProviderTargetObservation::MatchesResolvedTarget {
            reported: provider_model_identity(TARGET_IDENTITY),
        };
        let other_payload_error = log
            .record(evidence_id(1), canonical(2, target()), other_payload)
            .unwrap_err();
        let ProviderTargetEvidenceRecordingError::IdentifierReuse(reuse) = other_payload_error
        else {
            panic!("identifier reuse must be rejected as reuse");
        };
        assert_eq!(reuse.existing(), evidence);
        assert_eq!(reuse.requested_call(), model_call_id(2));
        assert_eq!(reuse.requested_observation(), other_payload);
        assert_eq!(log.lookup(evidence_id(1)), Some(&evidence));
    }

    /// S21 / INV-014: a fresh identifier is durably recorded only when the
    /// claimed variant is consistent with the exact target derived from the
    /// canonical call record.
    #[test]
    fn recording_rejects_observations_that_contradict_the_target() {
        let mut log = ProviderTargetEvidenceLog::new();

        let mismatch_reporting_the_target = mismatch(TARGET_IDENTITY);
        assert_eq!(
            log.record(
                evidence_id(1),
                canonical(2, target()),
                mismatch_reporting_the_target
            ),
            Err(
                ProviderTargetEvidenceRecordingError::ObservationContradictsTarget {
                    target: target(),
                    observation: mismatch_reporting_the_target,
                }
            )
        );
        assert_eq!(log.lookup(evidence_id(1)), None);

        let match_reporting_another_identity = ProviderTargetObservation::MatchesResolvedTarget {
            reported: provider_model_identity(8),
        };
        assert_eq!(
            log.record(
                evidence_id(1),
                canonical(2, target()),
                match_reporting_another_identity
            ),
            Err(
                ProviderTargetEvidenceRecordingError::ObservationContradictsTarget {
                    target: target(),
                    observation: match_reporting_another_identity,
                }
            )
        );
        assert_eq!(log.lookup(evidence_id(1)), None);
    }

    /// S21 / INV-014: a correlated mismatch on an issued nonterminal call
    /// produces exactly the sealed nonterminal-observation fact; unsent calls,
    /// cross-wired calls, non-mismatch payloads, and target-equal reports
    /// are rejected.
    #[test]
    fn nonterminal_mismatch_producer_validates_the_canonical_call() {
        let call = current_call(SUBJECT_CALL);
        let fact = recorded_mismatch(1)
            .nonterminal_call_mismatch_fact(&call)
            .expect("correlated mismatch produces the fatal fact");
        assert_eq!(
            fact.failure().kind(),
            ProviderTargetMismatchFailureKind::NonterminalCallObservation {
                evidence: evidence_id(1)
            }
        );
        assert_eq!(fact.affected_call(), call.id());
        assert_eq!(
            fact.effect(),
            ProviderTargetMismatchEffectView::ClassifyNonterminalKnownFailed
        );

        let unsent = CurrentModelCall::prepared(
            model_call_id(SUBJECT_CALL),
            turn_attempt_id(6),
            crate::FrozenModelSelection::Direct(crate::test_support::direct(5)),
            pinned_target(),
            &frontier_snapshot(),
        );
        assert_eq!(unsent.state(), CurrentModelCallState::Prepared);
        assert_eq!(
            recorded_mismatch(1).nonterminal_call_mismatch_fact(&unsent),
            Err(ProviderTargetMismatchCorrelationError::CallIsUnsent)
        );

        assert_eq!(
            recorded_mismatch_for_call(1, 3).nonterminal_call_mismatch_fact(&call),
            Err(
                ProviderTargetMismatchCorrelationError::EvidenceForDifferentCall {
                    evidence_call: model_call_id(3),
                    call: call.id(),
                }
            )
        );
        let matches = record_against(
            target(),
            1,
            SUBJECT_CALL,
            ProviderTargetObservation::MatchesResolvedTarget {
                reported: provider_model_identity(TARGET_IDENTITY),
            },
        );
        assert_eq!(
            matches.nonterminal_call_mismatch_fact(&call),
            Err(
                ProviderTargetMismatchCorrelationError::ObservationIsNotMismatch {
                    observation: matches.observation(),
                }
            )
        );
        // Recorded against a cross-wired target so the claimed mismatch can
        // reach the producer's own reported-versus-target check.
        let cross_wired = record_against(
            ResolvedProviderTarget::naming(provider_model_identity(9)),
            1,
            SUBJECT_CALL,
            mismatch(TARGET_IDENTITY),
        );
        assert_eq!(
            cross_wired.nonterminal_call_mismatch_fact(&call),
            Err(
                ProviderTargetMismatchCorrelationError::ReportedIdentityMatchesTarget {
                    reported: provider_model_identity(TARGET_IDENTITY),
                }
            )
        );
    }

    /// S21 / INV-014: mismatch evidence resolving terminal ambiguity leaves
    /// the physical disposition unchanged and produces the resolution
    /// fact only for an `Ambiguous` call.
    #[test]
    fn ambiguity_resolution_producer_requires_a_terminal_ambiguous_call() {
        let ambiguous = ended_call(SUBJECT_CALL, ModelCallDisposition::Ambiguous);
        let fact = recorded_mismatch(1)
            .terminal_ambiguity_resolution_fact(&ambiguous)
            .expect("correlated mismatch resolves terminal ambiguity");
        assert_eq!(
            fact.failure().kind(),
            ProviderTargetMismatchFailureKind::TerminalAmbiguityResolution {
                evidence: evidence_id(1)
            }
        );
        assert_eq!(fact.affected_call(), ambiguous.id());
        assert_eq!(
            fact.effect(),
            ProviderTargetMismatchEffectView::ResolveTerminalAmbiguity
        );
        assert_eq!(ambiguous.disposition(), ModelCallDisposition::Ambiguous);

        assert_non_ambiguous_disposition_rejects_resolution(ModelCallDisposition::Completed);
        assert_non_ambiguous_disposition_rejects_resolution(ModelCallDisposition::KnownFailed);
        assert_non_ambiguous_disposition_rejects_resolution(ModelCallDisposition::Refused);
        assert_non_ambiguous_disposition_rejects_resolution(ModelCallDisposition::Cancelled);
    }

    #[track_caller]
    fn assert_non_ambiguous_disposition_rejects_resolution(disposition: ModelCallDisposition) {
        assert_eq!(
            recorded_mismatch(1)
                .terminal_ambiguity_resolution_fact(&ended_call(SUBJECT_CALL, disposition)),
            Err(ProviderTargetMismatchCorrelationError::CallIsNotAmbiguous { disposition })
        );
    }

    /// S21 / INV-014: the completed-call invalidation is unique by
    /// invalidated call — the first valid mismatch fixes it, structurally
    /// equal replay is idempotent, and later observations cannot duplicate
    /// or replace it.
    #[test]
    fn invalidation_is_unique_by_invalidated_call() {
        let completed = ended_call(SUBJECT_CALL, ModelCallDisposition::Completed);
        let evidence = recorded_mismatch(1);

        let admitted =
            ProviderTargetMismatchInvalidation::admit(None, &completed, &evidence).unwrap();
        let AdmittedProviderTargetMismatchInvalidation::First(invalidation) = admitted else {
            panic!("the first valid mismatch must fix the value");
        };
        assert_eq!(invalidation.invalidated_call(), completed.id());
        assert_eq!(invalidation.first_mismatch_evidence(), evidence.id());
        assert_eq!(
            invalidation.fatal_mismatch_fact().failure().kind(),
            ProviderTargetMismatchFailureKind::TerminalCallInvalidation {
                invalidated_call: completed.id()
            }
        );
        assert_eq!(
            invalidation.fatal_mismatch_fact().affected_call(),
            completed.id()
        );
        assert_eq!(
            invalidation.fatal_mismatch_fact().effect(),
            ProviderTargetMismatchEffectView::PreserveCompletedInvalidation
        );

        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(Some(&invalidation), &completed, &evidence),
            Ok(AdmittedProviderTargetMismatchInvalidation::Replayed(
                invalidation
            ))
        );

        let later = recorded_mismatch_reporting(4, 9);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(Some(&invalidation), &completed, &later),
            Err(
                ProviderTargetMismatchInvalidationError::LaterObservationCannotReplace {
                    existing: invalidation,
                    later_evidence: later.id(),
                }
            )
        );

        let other_completed = ended_call(5, ModelCallDisposition::Completed);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                Some(&invalidation),
                &other_completed,
                &recorded_mismatch_for_call(6, 5),
            ),
            Err(
                ProviderTargetMismatchInvalidationError::ExistingInvalidationForDifferentCall {
                    existing: invalidation,
                    call: other_completed.id(),
                }
            )
        );
    }

    /// S21 / INV-014: invalidation validates the canonical completed call
    /// and correlated mismatch before uniqueness is even considered.
    #[test]
    fn invalidation_rejects_uncompleted_calls_and_uncorrelated_evidence() {
        assert_uncompleted_disposition_rejects_invalidation(ModelCallDisposition::KnownFailed);
        assert_uncompleted_disposition_rejects_invalidation(ModelCallDisposition::Refused);
        assert_uncompleted_disposition_rejects_invalidation(ModelCallDisposition::Cancelled);
        assert_uncompleted_disposition_rejects_invalidation(ModelCallDisposition::Ambiguous);

        let completed = ended_call(SUBJECT_CALL, ModelCallDisposition::Completed);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                None,
                &completed,
                &recorded_mismatch_for_call(1, 3)
            ),
            Err(ProviderTargetMismatchInvalidationError::Correlation(
                ProviderTargetMismatchCorrelationError::EvidenceForDifferentCall {
                    evidence_call: model_call_id(3),
                    call: completed.id(),
                }
            ))
        );
        // Recorded against a cross-wired target so the claimed mismatch can
        // reach the admission's own reported-versus-target check.
        let cross_wired = record_against(
            ResolvedProviderTarget::naming(provider_model_identity(9)),
            1,
            SUBJECT_CALL,
            mismatch(TARGET_IDENTITY),
        );
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(None, &completed, &cross_wired),
            Err(ProviderTargetMismatchInvalidationError::Correlation(
                ProviderTargetMismatchCorrelationError::ReportedIdentityMatchesTarget {
                    reported: provider_model_identity(TARGET_IDENTITY),
                }
            ))
        );
    }

    #[track_caller]
    fn assert_uncompleted_disposition_rejects_invalidation(disposition: ModelCallDisposition) {
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                None,
                &ended_call(SUBJECT_CALL, disposition),
                &recorded_mismatch(1)
            ),
            Err(ProviderTargetMismatchInvalidationError::CallNotCompleted { disposition })
        );
    }

    /// S21 / INV-014: recording reads the call identity and its exact target
    /// together from one canonical call record, so a match observation is
    /// validated against the call's own target and cannot be cross-wired to
    /// another call's target through the public conversions.
    #[test]
    fn recording_binds_the_call_and_target_from_one_canonical_record() {
        let current = current_call(2);
        let from_current = CanonicalCallTarget::from(&current);
        assert_eq!(from_current.call(), model_call_id(2));
        assert_eq!(from_current.target(), current.target());

        let ended = ended_call(3, ModelCallDisposition::Completed);
        let from_ended = CanonicalCallTarget::from(&ended);
        assert_eq!(from_ended.call(), model_call_id(3));
        assert_eq!(from_ended.target(), ended.target());

        let mut log = ProviderTargetEvidenceLog::new();
        let matches = ProviderTargetObservation::MatchesResolvedTarget {
            reported: provider_model_identity(TARGET_IDENTITY),
        };
        let recorded = log
            .record(evidence_id(1), from_current, matches)
            .expect("a match reported against the call's own target records");
        assert_eq!(recorded.evidence().call(), model_call_id(2));

        // The only pair reachable from a real record carries that record's
        // target, so a match claiming a different identity is a contradiction
        // rather than durable trusted evidence for this call.
        let mislabeled = ProviderTargetObservation::MatchesResolvedTarget {
            reported: provider_model_identity(9),
        };
        assert_eq!(
            log.record(
                evidence_id(2),
                CanonicalCallTarget::from(&current),
                mislabeled
            ),
            Err(
                ProviderTargetEvidenceRecordingError::ObservationContradictsTarget {
                    target: current.target(),
                    observation: mislabeled,
                }
            )
        );
    }

    /// S21 / INV-014: the invalidation log owns the per-call uniqueness, so
    /// the first valid mismatch fixes the entry and a later observation for
    /// the same call is rejected without the caller tracking the existing
    /// value.
    #[test]
    fn invalidation_log_enforces_uniqueness_without_caller_tracking() {
        let completed = ended_call(SUBJECT_CALL, ModelCallDisposition::Completed);
        let evidence = recorded_mismatch(1);

        let mut log = ProviderTargetMismatchInvalidationLog::new();
        assert_eq!(log.lookup(completed.id()), None);

        let admitted = log.admit(&completed, &evidence).unwrap();
        let AdmittedProviderTargetMismatchInvalidation::First(invalidation) = admitted else {
            panic!("the first valid mismatch must fix the value");
        };
        assert_eq!(invalidation.invalidated_call(), completed.id());
        assert_eq!(invalidation.first_mismatch_evidence(), evidence.id());
        assert_eq!(log.lookup(completed.id()), Some(&invalidation));

        assert_eq!(
            log.admit(&completed, &evidence),
            Ok(AdmittedProviderTargetMismatchInvalidation::Replayed(
                invalidation
            ))
        );

        let later = recorded_mismatch_reporting(4, 9);
        assert_eq!(
            log.admit(&completed, &later),
            Err(
                ProviderTargetMismatchInvalidationError::LaterObservationCannotReplace {
                    existing: invalidation,
                    later_evidence: later.id(),
                }
            )
        );
        assert_eq!(log.lookup(completed.id()), Some(&invalidation));

        let other_completed = ended_call(5, ModelCallDisposition::Completed);
        let other = log
            .admit(&other_completed, &recorded_mismatch_for_call(6, 5))
            .unwrap();
        assert_eq!(
            other,
            AdmittedProviderTargetMismatchInvalidation::First(other.invalidation())
        );
        assert_eq!(
            other.invalidation().invalidated_call(),
            other_completed.id()
        );
    }
}
