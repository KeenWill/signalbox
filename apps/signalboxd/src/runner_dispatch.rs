//! One serial daemon-to-runner dispatch and durable completion notification.

use signalbox_domain::{RunnerLeaseId, RunnerLeaseState, ToolDispatchAuthority};
use signalbox_persistence::runner_protocol::{RunnerProtocolStore, RunnerProtocolStoreError};
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard, Semaphore, watch};

pub(crate) enum RunnerDispatchOutcome {
    Daemon,
    Completed,
    RecoveryWait,
}

/// Shared local dispatch permit and durable-state wakeups for the runner connection.
#[derive(Clone, Debug)]
pub struct RunnerDispatchService {
    pub(crate) store: RunnerProtocolStore,
    permit: Arc<Semaphore>,
    admission: Arc<Mutex<()>>,
    changes: watch::Sender<()>,
}

impl RunnerDispatchService {
    pub(crate) fn new(store: RunnerProtocolStore) -> Self {
        let (changes, _) = watch::channel(());
        Self {
            store,
            permit: Arc::new(Semaphore::new(1)),
            admission: Arc::new(Mutex::new(())),
            changes,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }
    pub(crate) fn changed(&self) {
        self.changes.send_replace(());
    }

    pub(crate) async fn lock_admission(&self) -> MutexGuard<'_, ()> {
        self.admission.lock().await
    }

    pub(crate) async fn execute(
        &self,
        authority: &ToolDispatchAuthority,
    ) -> Result<RunnerDispatchOutcome, RunnerProtocolStoreError> {
        if self
            .store
            .load_placement(authority.correlation().session())
            .await?
            .is_none()
        {
            return Ok(RunnerDispatchOutcome::Daemon);
        }
        let permit = Arc::clone(&self.permit)
            .acquire_owned()
            .await
            .map_err(|_| {
                RunnerProtocolStoreError::Domain(signalbox_domain::RunnerDomainError::InvalidState)
            })?;
        let service = self.clone();
        let authority = authority.clone();
        finish_with_permit(
            permit,
            async move { service.execute_admitted(&authority).await },
        )
        .await
    }

    async fn execute_admitted(
        &self,
        authority: &ToolDispatchAuthority,
    ) -> Result<RunnerDispatchOutcome, RunnerProtocolStoreError> {
        let mut changes = self.subscribe();
        let offered = {
            let _admission = self.lock_admission().await;
            self.store
                .offer_tool_dispatch(authority, RunnerLeaseId::from_uuid(uuid::Uuid::now_v7()))
                .await
        };
        let mut lease = match offered {
            Ok(None) => return Ok(RunnerDispatchOutcome::Daemon),
            Ok(Some(lease)) => lease,
            Err(error @ RunnerProtocolStoreError::CommitAmbiguous(_)) => loop {
                match self
                    .store
                    .load_attempt_lease(authority.correlation().attempt())
                    .await
                {
                    Ok(Some(lease)) => break lease,
                    Ok(None) => return Err(error),
                    Err(_) => {
                        let _ = changes.changed().await;
                    }
                }
            },
            Err(error) => return Err(error),
        };
        self.changed();
        loop {
            if lease.state() == RunnerLeaseState::Completed {
                return Ok(RunnerDispatchOutcome::Completed);
            }
            if matches!(
                lease.state(),
                RunnerLeaseState::LostUnclaimed
                    | RunnerLeaseState::LostExecutionPossible
                    | RunnerLeaseState::LostClaimed
            ) {
                return Ok(RunnerDispatchOutcome::RecoveryWait);
            }
            let _ = changes.changed().await;
            // The issued lease remains authority during a database outage; an
            // infrastructure error cannot be reported as executor evidence.
            if let Ok(Some(current)) = self
                .store
                .load_attempt_lease(authority.correlation().attempt())
                .await
            {
                lease = current;
            }
        }
    }
}

// Once admission can write a lease, caller cancellation cannot release its slot.
async fn finish_with_permit<F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    completion: F,
) -> Result<RunnerDispatchOutcome, RunnerProtocolStoreError>
where
    F: std::future::Future<Output = Result<RunnerDispatchOutcome, RunnerProtocolStoreError>>
        + Send
        + 'static,
{
    tokio::spawn(async move {
        let _permit = permit;
        completion.await
    })
    .await
    .map_err(|_| {
        RunnerProtocolStoreError::Domain(signalbox_domain::RunnerDomainError::InvalidState)
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_the_caller_keeps_the_slot_until_completion() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.expect("single slot");
        let (entered, started) = tokio::sync::oneshot::channel();
        let (complete, completion) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(finish_with_permit(permit, async move {
            entered.send(()).expect("caller waiting");
            completion.await.expect("durable completion");
            Ok(RunnerDispatchOutcome::Completed)
        }));
        started.await.expect("admission started");
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "pending durable work owns the slot"
        );
        complete.send(()).expect("completion survives cancellation");
        let _successor =
            tokio::time::timeout(std::time::Duration::from_secs(5), slots.acquire_owned())
                .await
                .expect("completed work releases the slot")
                .expect("successor owns slot");
    }
}

/// Applies runner-owned approval to the one mirrored tool at model admission.
#[derive(Clone, Debug)]
pub(crate) struct RunnerToolCatalog<C> {
    pub(crate) catalog: C,
    pub(crate) posture: Option<signalbox_domain::ToolApprovalPosture>,
}

impl<C: signalbox_application::ToolCatalog> signalbox_application::ToolCatalog
    for RunnerToolCatalog<C>
{
    fn definitions(&self) -> Box<[signalbox_application::ToolDefinition]> {
        self.catalog
            .definitions()
            .into_vec()
            .into_iter()
            .map(|definition| self.project(definition))
            .collect()
    }
    fn definition(
        &self,
        name: &signalbox_domain::ToolName,
    ) -> Option<signalbox_application::ToolDefinition> {
        self.catalog
            .definition(name)
            .map(|definition| self.project(definition))
    }
    fn validate_arguments(
        &self,
        name: &signalbox_domain::ToolName,
        arguments: &signalbox_domain::NormalizedToolArguments,
    ) -> Result<(), signalbox_application::ToolCatalogValidationFailure> {
        self.catalog.validate_arguments(name, arguments)
    }
    fn preauthorization(
        &self,
        name: &signalbox_domain::ToolName,
        arguments: &signalbox_domain::NormalizedToolArguments,
    ) -> Result<
        signalbox_application::ToolPreauthorization,
        signalbox_application::ToolCatalogValidationFailure,
    > {
        self.catalog.preauthorization(name, arguments)
    }
}

impl<C> RunnerToolCatalog<C> {
    fn project(
        &self,
        definition: signalbox_application::ToolDefinition,
    ) -> signalbox_application::ToolDefinition {
        if definition.name().as_str() == signalbox_tools_basic::ECHO_NAME
            && let Some(posture) = self.posture
        {
            return definition.with_approval_posture(posture);
        }
        definition
    }
}
