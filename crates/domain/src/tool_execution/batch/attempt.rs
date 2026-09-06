//! Tool-batch attempt issuance and runner authorization for `docs/spec/tool-loop.md`.

use super::{
    PreparedClaimedToolAttemptReplacement, PreparedToolAttempt, ToolBatch, ToolBatchExecutionError,
    ToolBatchExecutionFailure, ToolBatchPhase,
};
use crate::{
    ApprovedToolRequest, AuthorizedToolAttempt, CurrentToolAttemptState, ReconstitutedToolAttempt,
    RunnerToolAttemptAuthorization, ToolApprovalResolution, ToolAttemptCrashOutcome,
    ToolAttemptEnd, ToolAttemptId, ToolDispatchAuthority, ToolEffectClass, ToolExecutionErrorKind,
    tool_attempt::RUNNER_ISSUANCE_AVAILABLE, tool_attempt::RUNNER_ISSUANCE_ISSUED,
    tool_attempt::RUNNER_ISSUANCE_RETIRED,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

impl ToolBatch {
    /// Prepares the earliest approved request without a physical attempt.
    pub fn prepare_next_attempt(
        &self,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<PreparedToolAttempt, ToolBatchExecutionError> {
        let ToolBatchPhase::Executing { turn_attempt } = self.phase else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        };
        if self.attempts.values().any(|attempt| {
            matches!(
                attempt,
                ReconstitutedToolAttempt::Current(current)
                    if matches!(
                        current.state(),
                        CurrentToolAttemptState::Prepared | CurrentToolAttemptState::InFlight
                    )
            )
        }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::LiveAttemptPresent,
            });
        }
        if self.attempts.values().any(|attempt| {
            matches!(
                attempt,
                ReconstitutedToolAttempt::Ended(ended)
                    if matches!(
                        ended.end(),
                        ToolAttemptEnd::KnownFailed { error }
                            if error.kind() == ToolExecutionErrorKind::CrashLost
                    )
            )
        }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::TurnLevelFailure,
            });
        }
        if self.retired_attempts.contains(&attempt)
            || self.attempts.values().any(|candidate| {
                let candidate_id = match candidate {
                    ReconstitutedToolAttempt::Current(current) => current.attempt(),
                    ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
                };
                candidate_id == attempt
            })
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptIdentityReuse,
            });
        }
        let next = self.requests.iter().find(|request| {
            self.approvals
                .get(&request.id())
                .is_some_and(ToolApprovalResolution::is_approved)
                && !self.attempts.contains_key(&request.id())
        });
        let Some(request) = next else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ReadyForContinuation,
            });
        };
        let approval = self.approvals[&request.id()].clone();
        let approved = ApprovedToolRequest::try_from_resolution(request.clone(), approval)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ApprovalMismatch,
            })?;
        Ok(PreparedToolAttempt {
            attempt: approved.prepare_attempt(attempt, turn_attempt, effect_class),
        })
    }

    pub(crate) fn replace_claimed_attempt(
        mut self,
        claimed_attempt: ToolAttemptId,
        replacement_attempt: ToolAttemptId,
    ) -> Result<PreparedClaimedToolAttemptReplacement, ToolBatchExecutionError> {
        let ToolBatchPhase::Executing { turn_attempt } = self.phase else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        };
        if self.retired_attempts.contains(&replacement_attempt)
            || self.attempts.values().any(|candidate| {
                let candidate_id = match candidate {
                    ReconstitutedToolAttempt::Current(current) => current.attempt(),
                    ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
                };
                candidate_id == replacement_attempt
            })
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptIdentityReuse,
            });
        }
        let Some((request, current)) =
            self.attempts
                .iter()
                .find_map(|(request, candidate)| match candidate {
                    ReconstitutedToolAttempt::Current(current)
                        if current.attempt() == claimed_attempt
                            && current.state() == CurrentToolAttemptState::InFlight =>
                    {
                        Some((*request, current.clone()))
                    }
                    ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => {
                        None
                    }
                })
        else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            });
        };
        let request_record = self
            .requests
            .iter()
            .find(|candidate| candidate.id() == request)
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let approval = self
            .approvals
            .get(&request)
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ApprovalMismatch,
            })?;
        let approved =
            ApprovedToolRequest::try_from_resolution(request_record, approval).map_err(|_| {
                ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                }
            })?;
        let effect_class = current.effect_class();
        let retired = match current.classify_crash_loss() {
            ToolAttemptCrashOutcome::KnownFailed(retired)
            | ToolAttemptCrashOutcome::Ambiguous(retired) => retired,
        };
        let replacement_runner_issuance = Arc::new(AtomicU8::new(RUNNER_ISSUANCE_AVAILABLE));
        self.runner_issuance.insert(
            replacement_attempt,
            Arc::clone(&replacement_runner_issuance),
        );
        let authorized = approved
            .prepare_attempt(replacement_attempt, turn_attempt, effect_class)
            .authorize_with_runner_issuance(replacement_runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })?;
        self.retired_attempts.insert(retired.attempt());
        self.attempts.insert(
            request,
            ReconstitutedToolAttempt::Current(authorized.attempt().clone()),
        );
        Ok(PreparedClaimedToolAttemptReplacement {
            batch: self,
            retired,
            approved,
            authorized,
        })
    }

    /// Authorizes one exact prepared attempt only through this freshly
    /// validated complete batch.
    pub fn authorize_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current) if current.attempt() == attempt => {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let runner_issuance =
            self.runner_issuance
                .get(&attempt)
                .map(Arc::clone)
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::AttemptMissing,
                })?;
        current
            .authorize_with_runner_issuance(runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })
    }

    /// Authorizes one exact prepared attempt and binds its canonical request.
    pub fn authorize_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError> {
        let authorized = self.authorize_attempt(attempt)?;
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.attempt().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        ToolDispatchAuthority::try_new(request, &authorized).ok_or(ToolBatchExecutionError {
            failure: ToolBatchExecutionFailure::AttemptStageMismatch,
        })
    }

    /// Restores in-flight authority after an ambiguous authorization
    /// acknowledgement only through this freshly validated complete batch.
    pub fn resume_in_flight_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current) if current.attempt() == attempt => {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let runner_issuance =
            self.runner_issuance
                .get(&attempt)
                .map(Arc::clone)
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::AttemptMissing,
                })?;
        current
            .resume_in_flight_with_runner_issuance(runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })
    }

    /// Restores one exact in-flight attempt with its canonical request.
    pub fn resume_in_flight_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError> {
        let authorized = self.resume_in_flight_attempt(attempt)?;
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.attempt().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        ToolDispatchAuthority::try_new(request, &authorized).ok_or(ToolBatchExecutionError {
            failure: ToolBatchExecutionFailure::AttemptStageMismatch,
        })
    }

    /// Authorizes one runner dispatch while pairing it with this batch's
    /// canonical immutable request and approval.
    pub fn authorize_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let authorized = self.authorize_attempt(attempt)?;
        self.bind_runner_authorization(authorized)
    }

    /// Restores one runner dispatch while pairing it with this batch's
    /// canonical immutable request and approval.
    pub fn resume_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let authorized = self.resume_in_flight_attempt(attempt)?;
        self.bind_runner_authorization(authorized)
    }

    pub(crate) fn reauthorize_unclaimed_runner_attempt(
        mut self,
        attempt: ToolAttemptId,
    ) -> Result<(Self, RunnerToolAttemptAuthorization), ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current)
                    if current.attempt() == attempt
                        && current.state() == CurrentToolAttemptState::InFlight =>
                {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let prior_issuance = Arc::clone(self.runner_issuance.get(&attempt).ok_or(
            ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            },
        )?);
        if prior_issuance
            .compare_exchange(
                RUNNER_ISSUANCE_ISSUED,
                RUNNER_ISSUANCE_RETIRED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            });
        }
        let successor_issuance = Arc::new(AtomicU8::new(RUNNER_ISSUANCE_AVAILABLE));
        self.runner_issuance
            .insert(attempt, Arc::clone(&successor_issuance));
        let authorized = current
            .resume_in_flight_with_runner_issuance(successor_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })?;
        let authorization = self.bind_runner_authorization(authorized)?;
        Ok((self, authorization))
    }

    fn bind_runner_authorization(
        &self,
        authorized: AuthorizedToolAttempt,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.correlation().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let approval =
            self.approvals
                .get(&request.id())
                .cloned()
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                })?;
        let approved =
            ApprovedToolRequest::try_from_resolution(request, approval).map_err(|_| {
                ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                }
            })?;
        RunnerToolAttemptAuthorization::try_new(approved, authorized).map_err(|_| {
            ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            }
        })
    }
}
