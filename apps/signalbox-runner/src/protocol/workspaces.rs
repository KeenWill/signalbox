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
        state: &mut RunnerStateRoot,
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
        let Some((request, ready)) = state.retained_provision() else {
            return Ok(());
        };
        let has_ready_receipt = ready.is_some();
        let request = request.clone();
        let pinned_clone_url_digest = state
            .retained_provision_clone_url_digest()
            .cloned()
            .ok_or(RunnerStateError::CorruptState)?;
        let checked = match self.check_workspace(request) {
            Ok(checked) if checked.canonical_clone_url_digest() == &pinned_clone_url_digest => {
                checked
            }
            Ok(_) | Err(RunnerConnectionError::Workspace(_)) => {
                if has_ready_receipt {
                    return Err(RunnerConnectionError::Workspace(
                        crate::WorkspaceProvisionError::ManifestConflict,
                    ));
                }
                self.finish_provision(
                    state,
                    Err(crate::WorkspaceProvisionError::ManifestConflict),
                )?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
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
        if let Some((correlation, phase)) = state.retained_release()
            && phase == signalbox_runner_wire::ReleasePhase::ReleaseCompleted
        {
            send_message(
                &mut self.io,
                Message::WorkspaceReleased(signalbox_runner_wire::WorkspaceReleased {
                    correlation: correlation.clone(),
                }),
            )
            .await?;
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
                    return Err(RunnerConnectionError::Workspace(
                        crate::WorkspaceProvisionError::Storage,
                    ));
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
    ) -> Result<(), RunnerConnectionError> {
        if self.last_release_recorded.as_ref() == Some(&correlation) {
            return Ok(());
        }
        state.acknowledge_release(&correlation)?;
        self.workspace = None;
        self.last_release_recorded = Some(correlation);
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

pub(super) fn apply_workspace_directives(
    state: &mut RunnerStateRoot,
    directives: &signalbox_runner_wire::ReconnectDirectives,
) -> Result<(), RunnerConnectionError> {
    let Some(directive) = &directives.workspace_operation else {
        return Ok(());
    };
    if let Some((correlation, phase)) = state.retained_release() {
        use signalbox_runner_wire::{DirectiveAction as A, ReleasePhase as P};
        let correlation = correlation.clone();
        match directive.action {
            A::Await if phase == P::ReleaseAccepted => {}
            A::Resend if phase == P::ReleaseCompleted => {}
            A::DiscardAsRecorded if phase == P::ReleaseCompleted => {
                state.acknowledge_release(&correlation)?
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
