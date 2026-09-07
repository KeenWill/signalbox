//! Tool-batch transcript result and terminal projections for `docs/spec/tool-loop.md`.

use super::{
    PreparedToolResultProjection, ToolBatch, ToolBatchPhase, ToolResultProjectionError,
    ToolResultProjectionFailure,
};
use crate::{
    ReconstitutedToolAttempt, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, ToolApprovalDecision, ToolAttemptEnd, ToolExecutionErrorKind,
};
use std::collections::BTreeSet;

impl ToolBatch {
    /// Builds one proposal-ordered reference-only result entry per request.
    pub fn prepare_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_result_projection_with_delegation(entry_ids, continuation_frontier, None)
    }

    /// Builds proposal-ordered results including one delivered foreground child result.
    pub fn prepare_delegation_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
        outcome: crate::DelegationOutcome,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_result_projection_with_delegation(
            entry_ids,
            continuation_frontier,
            Some(outcome),
        )
    }

    fn prepare_result_projection_with_delegation(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
        delegation_outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. })
            || entry_ids.len() != self.requests.len()
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let child_wait_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(ended.end(), ToolAttemptEnd::AwaitingChild { .. })
                )
            })
            .count();
        if delegation_outcome.is_some() != (child_wait_count == 1) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut delegation_outcome = delegation_outcome.map(Box::new);
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                _ if request.inadmissible_reason().is_some() => {
                    SemanticTranscriptEntryPayload::ToolInadmissible {
                        request: request.id(),
                    }
                }
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    let Some(ReconstitutedToolAttempt::Ended(attempt)) =
                        self.attempts.get(&request.id())
                    else {
                        return Err(ToolResultProjectionError {
                            failure: ToolResultProjectionFailure::BatchNotResolved,
                        });
                    };
                    match attempt.end() {
                        ToolAttemptEnd::Completed { .. } => {}
                        ToolAttemptEnd::KnownFailed { error }
                            if error.kind() != ToolExecutionErrorKind::CrashLost => {}
                        ToolAttemptEnd::AwaitingChild {
                            spawning_request,
                            child,
                        } => {
                            let Some(outcome) = delegation_outcome.take() else {
                                return Err(ToolResultProjectionError {
                                    failure: ToolResultProjectionFailure::BatchNotResolved,
                                });
                            };
                            entries.push(SemanticTranscriptEntry::from_validated_parts(
                                identity,
                                self.session,
                                SemanticTranscriptEntryPayload::DelegationResult {
                                    awaiting_request: request.id(),
                                    spawning_request: *spawning_request,
                                    child: *child,
                                    mode: crate::DelegationWaitMode::Foreground,
                                    delivery_sequence: None,
                                    outcome,
                                },
                            ));
                            continue;
                        }
                        ToolAttemptEnd::KnownFailed { .. } | ToolAttemptEnd::Ambiguous => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::TurnLevelFailure,
                            });
                        }
                    }
                    SemanticTranscriptEntryPayload::ToolExecutionResult {
                        attempt: attempt.attempt(),
                    }
                }
                Some(_) | None => {
                    return Err(ToolResultProjectionError {
                        failure: ToolResultProjectionFailure::BatchNotResolved,
                    });
                }
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        if delegation_outcome.is_some() {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                continuation_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }

    /// Builds the terminal result suffix for a batch blocked by one
    /// crash-lost physical attempt.
    ///
    /// This is the failure counterpart to cancellation projection: completed,
    /// known-failed, and denied requests retain their ordinary references,
    /// while every not-yet-attempted request closes without fabricated
    /// executor evidence.
    pub fn prepare_failure_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        let crash_lost_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(
                            ended.end(),
                            ToolAttemptEnd::KnownFailed { error }
                                if error.kind() == ToolExecutionErrorKind::CrashLost
                        )
                )
            })
            .count();
        if crash_lost_count != 1
            || self.attempts.values().any(|attempt| match attempt {
                ReconstitutedToolAttempt::Current(_) => true,
                ReconstitutedToolAttempt::Ended(ended) => ended.end() == &ToolAttemptEnd::Ambiguous,
            })
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::TurnLevelFailure,
            });
        }
        self.prepare_cancellation_projection(entry_ids, result_frontier)
    }

    /// Builds proposal-ordered results for an interrupt-cancelled executing
    /// batch after every physical attempt has reached a durable end.
    pub fn prepare_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_cancellation_projection_with_delegation(
            entry_ids,
            result_frontier,
            false,
            None,
        )
    }

    /// Builds interrupt closure for one foreground child wait, retaining a
    /// delivered result when descendant termination materialized one and
    /// otherwise closing the request with the parent turn.
    pub fn prepare_delegation_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
        outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_cancellation_projection_with_delegation(
            entry_ids,
            result_frontier,
            true,
            outcome,
        )
    }

    fn prepare_cancellation_projection_with_delegation(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
        closes_delegation_wait: bool,
        delegation_outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        let phase_can_close = matches!(self.phase, ToolBatchPhase::Executing { .. })
            || (closes_delegation_wait
                && matches!(self.phase, ToolBatchPhase::AwaitingChild { .. }));
        if !phase_can_close
            || entry_ids.len() != self.requests.len()
            || self
                .attempts
                .values()
                .any(|attempt| matches!(attempt, ReconstitutedToolAttempt::Current(_)))
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let child_wait_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(ended.end(), ToolAttemptEnd::AwaitingChild { .. })
                )
            })
            .count();
        if closes_delegation_wait != (child_wait_count == 1) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut delegation_outcome = delegation_outcome.map(Box::new);
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                _ if request.inadmissible_reason().is_some() => {
                    SemanticTranscriptEntryPayload::ToolInadmissible {
                        request: request.id(),
                    }
                }
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    match self.attempts.get(&request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(attempt))
                            if matches!(
                                attempt.end(),
                                ToolAttemptEnd::Completed { .. }
                                    | ToolAttemptEnd::KnownFailed { .. }
                            ) =>
                        {
                            SemanticTranscriptEntryPayload::ToolExecutionResult {
                                attempt: attempt.attempt(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(attempt)) => {
                            let ToolAttemptEnd::AwaitingChild {
                                spawning_request,
                                child,
                            } = attempt.end()
                            else {
                                return Err(ToolResultProjectionError {
                                    failure: ToolResultProjectionFailure::TurnLevelFailure,
                                });
                            };
                            match delegation_outcome.take() {
                                Some(outcome) => SemanticTranscriptEntryPayload::DelegationResult {
                                    awaiting_request: request.id(),
                                    spawning_request: *spawning_request,
                                    child: *child,
                                    mode: crate::DelegationWaitMode::Foreground,
                                    delivery_sequence: None,
                                    outcome,
                                },
                                None => SemanticTranscriptEntryPayload::ToolClosed {
                                    request: request.id(),
                                },
                            }
                        }
                        Some(ReconstitutedToolAttempt::Current(_)) => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::BatchNotResolved,
                            });
                        }
                        None => SemanticTranscriptEntryPayload::ToolClosed {
                            request: request.id(),
                        },
                    }
                }
                Some(_) | None => SemanticTranscriptEntryPayload::ToolClosed {
                    request: request.id(),
                },
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        if delegation_outcome.is_some() {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                result_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }

    /// Builds proposal-ordered logical closure for an interrupt-terminalized
    /// recovery batch while retaining its physical ambiguity separately.
    pub fn prepare_reconciliation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        terminal_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        if !matches!(self.phase, ToolBatchPhase::AwaitingRecovery { .. })
            || entry_ids.len() != self.requests.len()
            || self
                .attempts
                .values()
                .any(|attempt| matches!(attempt, ReconstitutedToolAttempt::Current(_)))
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                _ if request.inadmissible_reason().is_some() => {
                    SemanticTranscriptEntryPayload::ToolInadmissible {
                        request: request.id(),
                    }
                }
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    match self.attempts.get(&request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(attempt))
                            if matches!(attempt.end(), ToolAttemptEnd::Completed { .. })
                                || matches!(
                                    attempt.end(),
                                    ToolAttemptEnd::KnownFailed { error }
                                        if error.kind() != ToolExecutionErrorKind::CrashLost
                                ) =>
                        {
                            SemanticTranscriptEntryPayload::ToolExecutionResult {
                                attempt: attempt.attempt(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(_)) | None => {
                            SemanticTranscriptEntryPayload::ToolClosed {
                                request: request.id(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Current(_)) => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::BatchNotResolved,
                            });
                        }
                    }
                }
                Some(_) | None => SemanticTranscriptEntryPayload::ToolClosed {
                    request: request.id(),
                },
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                terminal_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }
}
