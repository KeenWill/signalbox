//! Provisioning journal execution and exact receipt acknowledgement.

use super::*;
use crate::workspace::provision::{CheckedProvision, prepared_receipt};
use signalbox_runner_wire::{WorkspaceProvision, WorkspaceReady, WorkspaceRecorded};

pub(super) enum WorkspaceExecution {
    Preparing(tokio::task::JoinHandle<Result<WorkspaceReady, crate::WorkspaceProvisionError>>),
    Ready,
}

impl Drop for WorkspaceExecution {
    fn drop(&mut self) {
        if let Self::Preparing(task) = self {
            task.abort();
        }
    }
}

pub(super) async fn workspace_finished(
    workspace: &mut Option<WorkspaceExecution>,
) -> Result<WorkspaceReady, RunnerConnectionError> {
    let Some(WorkspaceExecution::Preparing(task)) = workspace else {
        return std::future::pending().await;
    };
    task.await
        .map_err(|_| RunnerConnectionError::Workspace(crate::WorkspaceProvisionError::Storage))?
        .map_err(RunnerConnectionError::Workspace)
}

impl<S: AsyncRead + AsyncWrite + Unpin> RunnerConnection<S> {
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
        let Some((request, _)) = state.retained_provision() else {
            return Ok(());
        };
        let checked = self.check_workspace(request.clone())?;
        let store = state.workspace_store()?;
        self.workspace = Some(WorkspaceExecution::Preparing(tokio::spawn(
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
