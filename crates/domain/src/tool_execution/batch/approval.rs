//! Tool-batch user and delegate approval transitions for `docs/spec/tool-loop.md`.

use super::{
    DelegateToolApprovalTransitionError, DelegateToolApprovalTransitionFailure,
    PreparedDelegateToolApproval, PreparedToolBatchDecision, ToolBatch, ToolBatchDecisionError,
    ToolBatchDecisionFailure, ToolBatchPhase,
};
use crate::{
    ActiveTurnPhase, DecideToolRequest, DecideToolRequestResult, DelegateToolApproval,
    PreparedDecideToolRequest, ToolRequest, TurnAttemptId,
};

impl ToolBatch {
    /// Applies or authoritatively rejects one user decision against complete
    /// proposal-order state.
    pub fn prepare_user_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        self.prepare_decision(command, continuation_attempt, |command, request| {
            command.prepare_applied(request)
        })
    }

    /// Applies a committed lifecycle closure's denial to the exact parked
    /// request without attributing user agency.
    pub fn prepare_lifecycle_closure_denial(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        self.prepare_decision(command, continuation_attempt, |command, request| {
            command.prepare_lifecycle_closure_applied(request)
        })
    }

    fn prepare_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
        prepare: impl FnOnce(
            DecideToolRequest,
            &crate::ToolRequest,
        ) -> Result<
            PreparedDecideToolRequest,
            crate::DecideToolRequestPreparationError,
        >,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        let ToolBatchPhase::AwaitingApproval {
            request: waiting_on,
        } = self.phase
        else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command,
                failure: ToolBatchDecisionFailure::NoUndecidedRequest,
            });
        };
        let request = command.request();
        let Some(request_record) = self
            .requests
            .iter()
            .find(|candidate| candidate.id() == request)
        else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command,
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            });
        };
        if request_record.inadmissible_reason().is_some() || self.approvals.contains_key(&request) {
            return Ok(PreparedToolBatchDecision::rejected(
                self,
                command.prepare_already_resolved(),
                waiting_on,
            ));
        }
        let earliest = self
            .requests
            .iter()
            .find(|candidate| {
                candidate.inadmissible_reason().is_none()
                    && !self.approvals.contains_key(&candidate.id())
            })
            .map(ToolRequest::id);
        if earliest != Some(request) {
            let earliest = earliest.ok_or(ToolBatchDecisionError {
                batch: Box::new(self.clone()),
                command: command.clone(),
                failure: ToolBatchDecisionFailure::NoUndecidedRequest,
            })?;
            return Ok(PreparedToolBatchDecision::rejected(
                self,
                command.prepare_not_earliest(earliest),
                waiting_on,
            ));
        }
        let prepared =
            prepare(command, request_record).map_err(|error| ToolBatchDecisionError {
                batch: Box::new(self.clone()),
                command: error.command().clone(),
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            })?;
        let DecideToolRequestResult::Applied(applied) = prepared.result() else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command: prepared.command().clone(),
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            });
        };
        let mut approvals = self.approvals.clone();
        approvals.insert(request, applied.resolution().clone());
        let next_undecided = self
            .requests
            .iter()
            .find(|candidate| {
                candidate.inadmissible_reason().is_none()
                    && !approvals.contains_key(&candidate.id())
            })
            .map(ToolRequest::id);
        let (phase, active_phase) = match (next_undecided, continuation_attempt) {
            (Some(next), None) => (
                ToolBatchPhase::AwaitingApproval { request: next },
                ActiveTurnPhase::AwaitingApproval { request: next },
            ),
            (None, Some(turn_attempt)) => (
                ToolBatchPhase::Executing { turn_attempt },
                ActiveTurnPhase::Running {
                    current_attempt: crate::CurrentTurnAttempt::prepared(turn_attempt),
                },
            ),
            _ => {
                return Err(ToolBatchDecisionError {
                    batch: Box::new(self),
                    command: prepared.command().clone(),
                    failure: ToolBatchDecisionFailure::ContinuationAttemptMismatch,
                });
            }
        };
        let batch = Self {
            approvals,
            phase,
            ..self
        };
        Ok(PreparedToolBatchDecision {
            batch,
            prepared_command: prepared,
            active_phase,
        })
    }

    /// Applies one authority-checked delegate result to the exact parked request.
    pub fn prepare_delegate_decision(
        self,
        approval: DelegateToolApproval,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedDelegateToolApproval, DelegateToolApprovalTransitionError> {
        let ToolBatchPhase::AwaitingApproval {
            request: waiting_on,
        } = self.phase
        else {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::NoUndecidedRequest,
            ));
        };
        let Some(request) = self
            .requests
            .iter()
            .find(|request| request.id() == approval.request())
        else {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::RequestMismatch,
            ));
        };
        if waiting_on != approval.request()
            || self.approvals.contains_key(&approval.request())
            || request.approval_posture() != approval.posture()
        {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::RequestMismatch,
            ));
        }
        let resolution = match crate::ToolApprovalResolution::delegate(&approval) {
            Some(resolution) => resolution,
            None if continuation_attempt.is_none() => {
                return Ok(PreparedDelegateToolApproval {
                    batch: self,
                    approval,
                    resolution: None,
                    active_phase: ActiveTurnPhase::AwaitingApproval {
                        request: waiting_on,
                    },
                });
            }
            None => {
                return Err(DelegateToolApprovalTransitionError::new(
                    self,
                    approval,
                    DelegateToolApprovalTransitionFailure::ContinuationAttemptMismatch,
                ));
            }
        };
        let mut approvals = self.approvals.clone();
        approvals.insert(approval.request(), resolution.clone());
        let next_undecided = self
            .requests
            .iter()
            .find(|candidate| {
                candidate.inadmissible_reason().is_none()
                    && !approvals.contains_key(&candidate.id())
            })
            .map(ToolRequest::id);
        let (phase, active_phase) = match (next_undecided, continuation_attempt) {
            (Some(next), None) => (
                ToolBatchPhase::AwaitingApproval { request: next },
                ActiveTurnPhase::AwaitingApproval { request: next },
            ),
            (None, Some(turn_attempt)) => (
                ToolBatchPhase::Executing { turn_attempt },
                ActiveTurnPhase::Running {
                    current_attempt: crate::CurrentTurnAttempt::prepared(turn_attempt),
                },
            ),
            _ => {
                return Err(DelegateToolApprovalTransitionError::new(
                    self,
                    approval,
                    DelegateToolApprovalTransitionFailure::ContinuationAttemptMismatch,
                ));
            }
        };
        Ok(PreparedDelegateToolApproval {
            batch: Self {
                approvals,
                phase,
                ..self
            },
            approval,
            resolution: Some(resolution),
            active_phase,
        })
    }
}
