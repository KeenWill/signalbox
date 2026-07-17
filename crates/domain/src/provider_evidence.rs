//! Provider-target observations, evidence records, and mismatch authority.
//!
//! ADR-0005 is normative. This module models the typed observation payload,
//! the evidence record keyed by [`ProviderTargetEvidenceId`] with
//! identifier-replay and reuse boundaries, the completed-call mismatch
//! invalidation that is unique by invalidated call, and the validating
//! producers for [`ProviderTargetMismatchFailureRef`]. Trust classification
//! of raw provider-reported data, outcome eligibility and authority
//! transfer, the aggregate's classification precedence, and persistence are
//! separate later slices; a value here does not prove those guards held.

use std::collections::BTreeMap;

use crate::{
    CurrentModelCall, EndedModelCall, ModelCallDisposition, ModelCallId, ProviderModelIdentity,
    ProviderTargetEvidenceId, ProviderTargetMismatchFailureRef, ResolvedProviderTarget,
};

/// The typed payload of one trusted provider-target observation.
///
/// The two variants are ADR-0005's exact `ProviderTargetObservation`
/// algebra. Whether a reported identity is trusted, and how raw
/// provider-reported data normalizes into [`ProviderModelIdentity`], are
/// boundary and ADR-0007 scope; an absent reported identity is not
/// representable as either variant.
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

/// One recorded provider-target observation for one model call.
///
/// The record deliberately carries no copy of the exact target: ADR-0005
/// derives the target from the canonical call record inside the serialized
/// transition. S21 / INV-014: raw parts cannot claim a recorded evidence
/// fact:
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

#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
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
    /// nonterminal call and produces the typed fatal failure.
    ///
    /// Nonterminality is established by the current-call type; outcome
    /// eligibility and the atomic `Terminal(KnownFailed)` classification are
    /// the aggregate's transition.
    pub(crate) fn nonterminal_call_mismatch_failure(
        &self,
        call: &CurrentModelCall,
    ) -> Result<ProviderTargetMismatchFailureRef, ProviderTargetMismatchCorrelationError> {
        self.correlated_mismatch(call.id(), call.target())?;
        Ok(ProviderTargetMismatchFailureRef::nonterminal_call_observation(self.id))
    }

    /// Correlates this record as mismatch evidence resolving a call that
    /// already ended `Ambiguous` and produces the typed fatal failure.
    ///
    /// The terminal physical disposition stays unchanged by construction:
    /// this takes the ended record immutably and returns only the failure.
    pub(crate) fn terminal_ambiguity_resolution_failure(
        &self,
        call: &EndedModelCall,
    ) -> Result<ProviderTargetMismatchFailureRef, ProviderTargetMismatchCorrelationError> {
        if call.disposition() != ModelCallDisposition::Ambiguous {
            return Err(ProviderTargetMismatchCorrelationError::CallIsNotAmbiguous {
                disposition: call.disposition(),
            });
        }
        self.correlated_mismatch(call.id(), call.target())?;
        Ok(ProviderTargetMismatchFailureRef::terminal_ambiguity_resolution(self.id))
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
#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
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
}

/// The durable provider-target evidence records keyed by identifier.
///
/// ADR-0005: evidence-identifier lookup precedes current-state validation.
/// This value owns only that keyed boundary: recording with a fresh
/// identifier appends, replay of the same identifier with the structurally
/// equal call and payload returns the recorded result, and reuse with a
/// different call or payload is rejected without change. What a recorded
/// observation then does to call, attempt, and turn state is the
/// aggregate's serialized transition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderTargetEvidenceLog {
    records: BTreeMap<ProviderTargetEvidenceId, ProviderTargetEvidence>,
}

#[allow(
    dead_code,
    reason = "recording seam is consumed by the later aggregate slice"
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
    /// Identifier lookup happens first: a fresh identifier appends and
    /// returns the new record, an identical identifier/call/payload replay
    /// returns the already-recorded result, and identifier reuse with a
    /// different call or payload is rejected with the unchanged existing
    /// record and the exact rejected input.
    pub(crate) fn record(
        &mut self,
        id: ProviderTargetEvidenceId,
        call: ModelCallId,
        observation: ProviderTargetObservation,
    ) -> Result<ProviderTargetEvidenceRecording, ProviderTargetEvidenceReuseError> {
        if let Some(existing) = self.records.get(&id) {
            return if existing.call == call && existing.observation == observation {
                Ok(ProviderTargetEvidenceRecording::Replayed(*existing))
            } else {
                Err(ProviderTargetEvidenceReuseError {
                    existing: *existing,
                    requested_call: call,
                    requested_observation: observation,
                })
            };
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

/// One accepted evidence-recording outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "recording seam is consumed by the later aggregate slice"
)]
pub(crate) enum ProviderTargetEvidenceRecording {
    /// A fresh identifier durably recorded this evidence.
    First(ProviderTargetEvidence),
    /// A structurally equal replay returned the recorded result.
    Replayed(ProviderTargetEvidence),
}

#[allow(
    dead_code,
    reason = "recording seam is consumed by the later aggregate slice"
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
#[allow(
    dead_code,
    reason = "recording seam is consumed by the later aggregate slice"
)]
pub(crate) struct ProviderTargetEvidenceReuseError {
    existing: ProviderTargetEvidence,
    requested_call: ModelCallId,
    requested_observation: ProviderTargetObservation,
}

#[allow(
    dead_code,
    reason = "recording seam is consumed by the later aggregate slice"
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
/// ADR-0005 makes this value unique by `invalidated_call`: the first valid
/// mismatch fixes it, structurally equal evidence replay is idempotent, and
/// later observations cannot duplicate or replace it. The value carries no
/// exact target and no authority generation; both derive from the canonical
/// call and transfer chain inside the serialized transition. S21 / INV-014:
/// raw identities cannot claim an invalidation:
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
/// The sole producer is the crate-private admission boundary, which
/// validates the canonical completed call and correlated mismatch evidence
/// against the at most one existing invalidation for that call. Whether
/// the call is still outcome-authoritative when the transition commits, and
/// that it belongs to the active turn, are the aggregate's checks.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProviderTargetMismatchInvalidation {
    invalidated_call: ModelCallId,
    first_mismatch_evidence: ProviderTargetEvidenceId,
}

#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
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
    pub(crate) fn admit(
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

    /// Produces the typed fatal failure carried by the stop this
    /// invalidation requires.
    pub(crate) fn failure(&self) -> ProviderTargetMismatchFailureRef {
        ProviderTargetMismatchFailureRef::terminal_call_invalidation(self.invalidated_call)
    }
}

/// One accepted invalidation-admission outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
)]
pub(crate) enum AdmittedProviderTargetMismatchInvalidation {
    /// The first valid mismatch fixed the unique value.
    First(ProviderTargetMismatchInvalidation),
    /// A structurally equal evidence replay returned the existing value.
    Replayed(ProviderTargetMismatchInvalidation),
}

#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
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
#[allow(
    dead_code,
    reason = "trusted producer seam is consumed by the later aggregate slice"
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
        AdmittedProviderTargetMismatchInvalidation, ProviderTargetEvidence,
        ProviderTargetEvidenceLog, ProviderTargetEvidenceRecording,
        ProviderTargetMismatchCorrelationError, ProviderTargetMismatchInvalidation,
        ProviderTargetMismatchInvalidationError, ProviderTargetObservation,
    };
    use crate::test_support::{
        model_call_id, provider_model_identity, provider_target_evidence_id as evidence_id, turn_id,
    };
    use crate::{
        CurrentModelCall, EndedModelCall, ModelCallDisposition, PinnedProviderTarget,
        ProviderTargetMismatchFailureKind, ResolvedProviderTarget,
    };

    const TARGET_IDENTITY: u128 = 7;

    fn pinned_target() -> PinnedProviderTarget {
        PinnedProviderTarget::pinned(
            turn_id(1),
            ResolvedProviderTarget::naming(provider_model_identity(TARGET_IDENTITY)),
        )
    }

    fn current_call(call: u128) -> CurrentModelCall {
        CurrentModelCall::prepared(model_call_id(call), pinned_target())
            .begin_in_flight()
            .expect("Prepared may send")
    }

    fn ended_call(call: u128, disposition: ModelCallDisposition) -> EndedModelCall {
        current_call(call)
            .end_classified(disposition)
            .expect("issued calls classify every disposition")
    }

    fn mismatch(reported: u128) -> ProviderTargetObservation {
        ProviderTargetObservation::Mismatch {
            reported: provider_model_identity(reported),
        }
    }

    fn recorded_mismatch(id: u128, call: u128, reported: u128) -> ProviderTargetEvidence {
        ProviderTargetEvidenceLog::new()
            .record(evidence_id(id), model_call_id(call), mismatch(reported))
            .expect("a fresh identifier records")
            .evidence()
    }

    /// S21 / INV-014: identifier lookup precedes any other validation; a
    /// fresh record appends, an equal replay returns the recorded result,
    /// and reuse with a different call or payload is rejected unchanged.
    #[test]
    fn evidence_identifier_replay_and_reuse_boundaries_are_exact() {
        let mut log = ProviderTargetEvidenceLog::new();
        let first = log
            .record(evidence_id(1), model_call_id(2), mismatch(8))
            .unwrap();
        let ProviderTargetEvidenceRecording::First(evidence) = first else {
            panic!("a fresh identifier must record first");
        };
        assert_eq!(evidence.id(), evidence_id(1));
        assert_eq!(evidence.call(), model_call_id(2));
        assert_eq!(evidence.observation(), mismatch(8));
        assert_eq!(log.lookup(evidence_id(1)), Some(&evidence));

        assert_eq!(
            log.record(evidence_id(1), model_call_id(2), mismatch(8)),
            Ok(ProviderTargetEvidenceRecording::Replayed(evidence))
        );

        for (call, observation) in [
            (model_call_id(3), mismatch(8)),
            (
                model_call_id(2),
                ProviderTargetObservation::MatchesResolvedTarget {
                    reported: provider_model_identity(8),
                },
            ),
        ] {
            let error = log.record(evidence_id(1), call, observation).unwrap_err();
            assert_eq!(error.existing(), evidence);
            assert_eq!(error.requested_call(), call);
            assert_eq!(error.requested_observation(), observation);
        }
        assert_eq!(log.lookup(evidence_id(1)), Some(&evidence));
    }

    /// S21 / INV-014: a correlated mismatch on a nonterminal call produces
    /// exactly the nonterminal-observation failure; cross-wired calls,
    /// non-mismatch payloads, and target-equal reports are rejected.
    #[test]
    fn nonterminal_mismatch_producer_validates_the_canonical_call() {
        let call = current_call(2);
        let failure = recorded_mismatch(1, 2, 8)
            .nonterminal_call_mismatch_failure(&call)
            .expect("correlated mismatch produces the fatal failure");
        assert_eq!(
            failure.kind(),
            ProviderTargetMismatchFailureKind::NonterminalCallObservation {
                evidence: evidence_id(1)
            }
        );

        assert_eq!(
            recorded_mismatch(1, 3, 8).nonterminal_call_mismatch_failure(&call),
            Err(
                ProviderTargetMismatchCorrelationError::EvidenceForDifferentCall {
                    evidence_call: model_call_id(3),
                    call: model_call_id(2),
                }
            )
        );
        let matches = ProviderTargetEvidenceLog::new()
            .record(
                evidence_id(1),
                model_call_id(2),
                ProviderTargetObservation::MatchesResolvedTarget {
                    reported: provider_model_identity(TARGET_IDENTITY),
                },
            )
            .unwrap()
            .evidence();
        assert_eq!(
            matches.nonterminal_call_mismatch_failure(&call),
            Err(
                ProviderTargetMismatchCorrelationError::ObservationIsNotMismatch {
                    observation: matches.observation(),
                }
            )
        );
        assert_eq!(
            recorded_mismatch(1, 2, TARGET_IDENTITY).nonterminal_call_mismatch_failure(&call),
            Err(
                ProviderTargetMismatchCorrelationError::ReportedIdentityMatchesTarget {
                    reported: provider_model_identity(TARGET_IDENTITY),
                }
            )
        );
    }

    /// S21 / INV-014: mismatch evidence resolving terminal ambiguity leaves
    /// the physical disposition unchanged and produces the resolution
    /// failure only for an `Ambiguous` call.
    #[test]
    fn ambiguity_resolution_producer_requires_a_terminal_ambiguous_call() {
        let ambiguous = ended_call(2, ModelCallDisposition::Ambiguous);
        let failure = recorded_mismatch(1, 2, 8)
            .terminal_ambiguity_resolution_failure(&ambiguous)
            .expect("correlated mismatch resolves terminal ambiguity");
        assert_eq!(
            failure.kind(),
            ProviderTargetMismatchFailureKind::TerminalAmbiguityResolution {
                evidence: evidence_id(1)
            }
        );
        assert_eq!(ambiguous.disposition(), ModelCallDisposition::Ambiguous);

        for disposition in [
            ModelCallDisposition::Completed,
            ModelCallDisposition::KnownFailed,
            ModelCallDisposition::Refused,
            ModelCallDisposition::Cancelled,
        ] {
            assert_eq!(
                recorded_mismatch(1, 2, 8)
                    .terminal_ambiguity_resolution_failure(&ended_call(2, disposition)),
                Err(ProviderTargetMismatchCorrelationError::CallIsNotAmbiguous { disposition })
            );
        }
    }

    /// S21 / INV-014: the completed-call invalidation is unique by
    /// invalidated call — the first valid mismatch fixes it, structurally
    /// equal replay is idempotent, and later observations cannot duplicate
    /// or replace it.
    #[test]
    fn invalidation_is_unique_by_invalidated_call() {
        let completed = ended_call(2, ModelCallDisposition::Completed);
        let evidence = recorded_mismatch(1, 2, 8);

        let admitted =
            ProviderTargetMismatchInvalidation::admit(None, &completed, &evidence).unwrap();
        let AdmittedProviderTargetMismatchInvalidation::First(invalidation) = admitted else {
            panic!("the first valid mismatch must fix the value");
        };
        assert_eq!(invalidation.invalidated_call(), model_call_id(2));
        assert_eq!(invalidation.first_mismatch_evidence(), evidence_id(1));
        assert_eq!(
            invalidation.failure().kind(),
            ProviderTargetMismatchFailureKind::TerminalCallInvalidation {
                invalidated_call: model_call_id(2)
            }
        );

        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(Some(&invalidation), &completed, &evidence),
            Ok(AdmittedProviderTargetMismatchInvalidation::Replayed(
                invalidation
            ))
        );

        let later = recorded_mismatch(4, 2, 9);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(Some(&invalidation), &completed, &later),
            Err(
                ProviderTargetMismatchInvalidationError::LaterObservationCannotReplace {
                    existing: invalidation,
                    later_evidence: evidence_id(4),
                }
            )
        );

        let other_completed = ended_call(5, ModelCallDisposition::Completed);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                Some(&invalidation),
                &other_completed,
                &recorded_mismatch(6, 5, 8),
            ),
            Err(
                ProviderTargetMismatchInvalidationError::ExistingInvalidationForDifferentCall {
                    existing: invalidation,
                    call: model_call_id(5),
                }
            )
        );
    }

    /// S21 / INV-014: invalidation validates the canonical completed call
    /// and correlated mismatch before uniqueness is even considered.
    #[test]
    fn invalidation_rejects_uncompleted_calls_and_uncorrelated_evidence() {
        let evidence = recorded_mismatch(1, 2, 8);
        for disposition in [
            ModelCallDisposition::KnownFailed,
            ModelCallDisposition::Refused,
            ModelCallDisposition::Cancelled,
            ModelCallDisposition::Ambiguous,
        ] {
            assert_eq!(
                ProviderTargetMismatchInvalidation::admit(
                    None,
                    &ended_call(2, disposition),
                    &evidence
                ),
                Err(ProviderTargetMismatchInvalidationError::CallNotCompleted { disposition })
            );
        }

        let completed = ended_call(2, ModelCallDisposition::Completed);
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                None,
                &completed,
                &recorded_mismatch(1, 3, 8)
            ),
            Err(ProviderTargetMismatchInvalidationError::Correlation(
                ProviderTargetMismatchCorrelationError::EvidenceForDifferentCall {
                    evidence_call: model_call_id(3),
                    call: model_call_id(2),
                }
            ))
        );
        assert_eq!(
            ProviderTargetMismatchInvalidation::admit(
                None,
                &completed,
                &recorded_mismatch(1, 2, TARGET_IDENTITY)
            ),
            Err(ProviderTargetMismatchInvalidationError::Correlation(
                ProviderTargetMismatchCorrelationError::ReportedIdentityMatchesTarget {
                    reported: provider_model_identity(TARGET_IDENTITY),
                }
            ))
        );
    }
}
