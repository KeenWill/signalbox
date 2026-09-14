//! Provisioning journal execution and exact receipt acknowledgement.

use super::*;
use crate::workspace::provision::{CheckedProvision, prepared_receipt};
use signalbox_runner_wire::{WorkspaceProvision, WorkspaceReady, WorkspaceRecorded};

pub(super) struct WorkspacePreparationWorker {
    task: tokio::task::JoinHandle<Result<WorkspaceReady, crate::WorkspaceProvisionError>>,
}

impl Drop for WorkspacePreparationWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// In-flight release work that can move between physical connections.
#[doc(hidden)]
pub struct WorkspaceReleaseWorker {
    task: tokio::task::JoinHandle<(signalbox_runner_wire::ReleaseCorrelation, bool)>,
}

impl WorkspaceReleaseWorker {
    pub(super) fn new(
        task: tokio::task::JoinHandle<(signalbox_runner_wire::ReleaseCorrelation, bool)>,
    ) -> Self {
        Self { task }
    }
}

pub(super) enum WorkspaceExecution {
    Preparing(WorkspacePreparationWorker),
    Releasing(WorkspaceReleaseWorker),
    Ready,
}

impl WorkspaceExecution {
    pub(super) fn preparing(
        task: tokio::task::JoinHandle<Result<WorkspaceReady, crate::WorkspaceProvisionError>>,
    ) -> Self {
        Self::Preparing(WorkspacePreparationWorker { task })
    }
}

pub(super) enum WorkspaceCompletion {
    Ready(Box<Result<WorkspaceReady, crate::WorkspaceProvisionError>>),
    Released {
        correlation: signalbox_runner_wire::ReleaseCorrelation,
        succeeded: bool,
    },
}

pub(super) async fn workspace_finished(
    workspace: &mut Option<WorkspaceExecution>,
) -> Result<WorkspaceCompletion, RunnerConnectionError> {
    match workspace {
        Some(WorkspaceExecution::Preparing(worker)) => (&mut worker.task)
            .await
            .map_err(|_| RunnerConnectionError::Workspace(crate::WorkspaceProvisionError::Storage))
            .map(|result| WorkspaceCompletion::Ready(Box::new(result))),
        Some(WorkspaceExecution::Releasing(worker)) => {
            let (correlation, succeeded) = (&mut worker.task).await.map_err(|_| {
                RunnerConnectionError::Workspace(crate::WorkspaceProvisionError::Storage)
            })?;
            Ok(WorkspaceCompletion::Released {
                correlation,
                succeeded,
            })
        }
        _ => std::future::pending().await,
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> RunnerConnection<S> {
    /// Restores the exact release executor retained across a reconnect.
    #[doc(hidden)]
    pub fn restore_workspace_release_worker(&mut self, worker: WorkspaceReleaseWorker) {
        debug_assert!(self.workspace.is_none());
        self.workspace = Some(WorkspaceExecution::Releasing(worker));
    }

    /// Removes an in-flight release executor so reconnect does not duplicate it.
    #[doc(hidden)]
    pub fn take_workspace_release_worker(&mut self) -> Option<WorkspaceReleaseWorker> {
        match self.workspace.take() {
            Some(WorkspaceExecution::Releasing(worker)) => Some(worker),
            other => {
                self.workspace = other;
                None
            }
        }
    }

    pub(super) fn check_workspace(
        &self,
        operation: WorkspaceProvision,
    ) -> Result<CheckedProvision, RunnerConnectionError> {
        let config = self
            .configuration
            .as_ref()
            .ok_or(RunnerConnectionError::Workspace(
                crate::WorkspaceProvisionError::RepositoryUnavailable,
            ))?;
        CheckedProvision::check(config, operation).map_err(RunnerConnectionError::Workspace)
    }

    pub(super) fn ensure_workspace(
        &mut self,
        state: &RunnerStateRoot,
    ) -> Result<(), RunnerConnectionError> {
        if self.workspace.is_some() {
            return Ok(());
        }
        if state.retained_provision_failure().is_some() {
            self.workspace = Some(WorkspaceExecution::Ready);
            return Ok(());
        }
        if state.retained_release().is_some() {
            self.workspace = if let Some(accepted) = state.accepted_release() {
                let store = state.workspace_store()?;
                Some(WorkspaceExecution::Releasing(WorkspaceReleaseWorker::new(
                    tokio::task::spawn_blocking(move || {
                        let correlation = accepted.correlation().clone();
                        (correlation, store.release(&accepted).is_ok())
                    }),
                )))
            } else {
                Some(WorkspaceExecution::Ready)
            };
            return Ok(());
        }
        let Some((request, _)) = state.retained_provision() else {
            return Ok(());
        };
        let checked = self.check_workspace(request.clone())?;
        let store = state.workspace_store()?;
        self.workspace = Some(WorkspaceExecution::preparing(tokio::spawn(
            checked.prepare(store),
        )));
        Ok(())
    }

    pub(super) async fn send_retained_workspace(
        &mut self,
        state: &RunnerStateRoot,
    ) -> Result<(), RunnerConnectionError> {
        if matches!(self.workspace, Some(WorkspaceExecution::Ready))
            && let Some((_, Some(ready))) = state.retained_provision()
        {
            send_message(&mut self.io, Message::WorkspaceReady(ready.clone())).await?;
        }
        if let Some(failure) = state.retained_provision_failure() {
            send_message(
                &mut self.io,
                Message::OperationFailed(signalbox_runner_wire::OperationFailed {
                    failure: failure.clone(),
                }),
            )
            .await?;
        }
        if let Some((correlation, phase, failure)) = state.retained_release() {
            if let Some(failure) = failure {
                send_message(
                    &mut self.io,
                    Message::OperationFailed(signalbox_runner_wire::OperationFailed {
                        failure: failure.clone(),
                    }),
                )
                .await?;
            } else if phase == signalbox_runner_wire::ReleasePhase::ReleaseCompleted {
                send_message(
                    &mut self.io,
                    Message::WorkspaceReleased(signalbox_runner_wire::WorkspaceReleased {
                        correlation: correlation.clone(),
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn finish_workspace(
        &mut self,
        state: &mut RunnerStateRoot,
        completion: WorkspaceCompletion,
    ) -> Result<(), RunnerConnectionError> {
        match completion {
            WorkspaceCompletion::Ready(ready) => self.finish_provision(state, *ready)?,
            WorkspaceCompletion::Released {
                correlation,
                succeeded,
            } => {
                if succeeded {
                    state.complete_release(&correlation)?;
                } else {
                    use signalbox_runner_wire::{
                        DetailName, FailureCategory, FailureDetail, OperationCorrelation,
                        OperationFailure,
                    };
                    state.fail_release(OperationFailure {
                        correlation: OperationCorrelation::Release(correlation),
                        category: FailureCategory::WorkspaceCleanupFailed,
                        detail: FailureDetail::try_new(
                            DetailName::try_new("workspace-cleanup-failed".to_owned())
                                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
                            "The accepted workspace cleanup failed".to_owned(),
                            serde_json::json!({}),
                        )
                        .map_err(RunnerConnectionError::InvalidLocalFrame)?,
                    })?;
                }
            }
        }
        self.workspace = Some(WorkspaceExecution::Ready);
        self.send_retained_workspace(state).await
    }

    pub(super) fn acknowledge_release(
        &mut self,
        state: &mut RunnerStateRoot,
        correlation: signalbox_runner_wire::ReleaseCorrelation,
        failed: bool,
    ) -> Result<(), RunnerConnectionError> {
        if self.last_release_recorded.as_ref() == Some(&(correlation.clone(), failed)) {
            return Ok(());
        }
        state.acknowledge_release(&correlation, failed)?;
        self.workspace = None;
        self.last_release_recorded = Some((correlation, failed));
        Ok(())
    }

    pub(super) fn finish_provision(
        &mut self,
        state: &mut RunnerStateRoot,
        result: Result<WorkspaceReady, crate::WorkspaceProvisionError>,
    ) -> Result<(), RunnerConnectionError> {
        use signalbox_runner_wire::{
            DetailName, FailureCategory, FailureDetail, OperationCorrelation, OperationFailure,
        };
        match result {
            Ok(ready) => state.record_workspace_ready(ready)?,
            Err(error) => {
                use crate::WorkspaceProvisionError as Error;
                let (category, code) = match error {
                    Error::RepositoryUnavailable => (
                        FailureCategory::RepositoryUnavailable,
                        "repository-unavailable",
                    ),
                    Error::CredentialUnavailable => (
                        FailureCategory::CredentialUnavailable,
                        "credential-unavailable",
                    ),
                    Error::SandboxUnavailable => {
                        (FailureCategory::SandboxUnavailable, "sandbox-unavailable")
                    }
                    Error::ManifestConflict => {
                        (FailureCategory::WorkspaceConflict, "manifest-conflict")
                    }
                    Error::Storage => return Err(RunnerConnectionError::Workspace(error)),
                };
                let (request, _) = state
                    .retained_provision()
                    .ok_or(RunnerStateError::InvalidTransition)?;
                let failure = OperationFailure {
                    correlation: OperationCorrelation::Provision(request.correlation.clone()),
                    category,
                    detail: FailureDetail::try_new(
                        DetailName::try_new(code.to_owned())
                            .map_err(RunnerConnectionError::InvalidLocalFrame)?,
                        error.to_string(),
                        serde_json::json!({}),
                    )
                    .map_err(RunnerConnectionError::InvalidLocalFrame)?,
                };
                state.record_provision_failure(failure)?;
            }
        }
        self.workspace = Some(WorkspaceExecution::Ready);
        Ok(())
    }

    pub(super) fn record_workspace(
        &mut self,
        state: &mut RunnerStateRoot,
        recorded: WorkspaceRecorded,
    ) -> Result<(), RunnerConnectionError> {
        if self.last_workspace_recorded.as_ref() == Some(&recorded) {
            return Ok(());
        }
        let Some((_, Some(ready))) = state.retained_provision() else {
            return Err(RunnerStateError::InvalidTransition.into());
        };
        if ready.correlation != recorded.correlation
            || ready.ready.manifest.manifest_id != recorded.manifest_id
            || ready.ready.manifest_digest != recorded.manifest_digest
        {
            return Err(RunnerStateError::InvalidTransition.into());
        }
        let prepared = prepared_receipt(ready).map_err(RunnerConnectionError::Workspace)?;
        state
            .workspace_store()?
            .activate(&prepared)
            .map_err(|error| RunnerConnectionError::Workspace(error.into()))?;
        state.acknowledge_workspace(&recorded)?;
        self.workspace = None;
        self.last_workspace_recorded = Some(recorded);
        Ok(())
    }
}

pub(super) fn heartbeat_phase(
    state: &RunnerStateRoot,
) -> Option<signalbox_runner_wire::HeartbeatWorkspacePhase> {
    use signalbox_runner_wire::{
        HeartbeatWorkspacePhase as H, ProvisionPhase as P, ReleasePhase as R,
        WorkspaceFailureCorrelation as F, WorkspaceOperation as W,
    };
    let inventory = state.reconnect_inventory();
    if let Some(failure) = state.retained_provision_failure()
        && let signalbox_runner_wire::OperationCorrelation::Provision(correlation) =
            &failure.correlation
    {
        return Some(H::FailureUnrecorded {
            correlation: F::Provision(correlation.clone()),
        });
    }
    if let Some((correlation, _, Some(_))) = state.retained_release() {
        return Some(H::FailureUnrecorded {
            correlation: F::Release(correlation.clone()),
        });
    }
    inventory
        .workspace_operation
        .map(|operation| match operation {
            W::Provision {
                correlation,
                phase: P::Provisioning,
            } => H::Provisioning { correlation },
            W::Provision {
                correlation,
                phase: P::ReadyUnrecorded,
            } => H::ReadyUnrecorded { correlation },
            W::Release {
                correlation,
                phase: R::ReleaseAccepted,
            } => H::ReleaseAccepted { correlation },
            W::Release {
                correlation,
                phase: R::ReleaseCompleted,
            } => H::ReleaseCompleted { correlation },
        })
}

pub(super) fn apply_workspace_directives(
    state: &mut RunnerStateRoot,
    directives: &signalbox_runner_wire::ReconnectDirectives,
) -> Result<(), RunnerConnectionError> {
    let Some(directive) = &directives.workspace_operation else {
        return Ok(());
    };
    if let Some((correlation, phase, failure)) = state.retained_release() {
        use signalbox_runner_wire::{DirectiveAction as A, ReleasePhase as P};
        let correlation = correlation.clone();
        let failed = failure.is_some();
        match directive.action {
            A::Await if phase == P::ReleaseAccepted && !failed => {}
            A::Resend
                if phase == P::ReleaseCompleted
                    || (failed
                        && directives
                            .operation_failure
                            .as_ref()
                            .is_some_and(|failure| failure.action == A::Resend)) => {}
            A::DiscardAsRecorded
                if phase == P::ReleaseCompleted
                    || (failed
                        && directives
                            .operation_failure
                            .as_ref()
                            .is_some_and(|failure| failure.action == A::DiscardAsRecorded)) =>
            {
                state.acknowledge_release(&correlation, failed)?
            }
            A::FailStale
                if directives
                    .operation_failure
                    .as_ref()
                    .is_none_or(|failure| failure.action == A::FailStale) =>
            {
                let signalbox_runner_wire::OperationCorrelation::Release(expected) =
                    &directive.correlation
                else {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ResumeDirectives,
                    ));
                };
                state.discard_reconciled_release(expected)?;
            }
            _ => {
                return Err(RunnerConnectionError::Violation(
                    ProtocolViolation::ResumeDirectives,
                ));
            }
        }
    } else {
        use signalbox_runner_wire::DirectiveAction as A;
        let failure = directives.operation_failure.as_ref();
        match directive.action {
            A::Await if failure.is_none() => {}
            A::Resend if failure.is_some_and(|failure| failure.action == A::Resend) => {}
            A::DiscardAsRecorded
                if failure.is_some_and(|failure| failure.action == A::DiscardAsRecorded) =>
            {
                state.acknowledge_provision_failure(&directive.correlation)?
            }
            _ => {
                return Err(RunnerConnectionError::Violation(
                    ProtocolViolation::ResumeDirectives,
                ));
            }
        }
    }
    Ok(())
}
