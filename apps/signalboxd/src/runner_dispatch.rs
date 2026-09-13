//! One serial daemon-to-runner dispatch and durable completion notification.

use signalbox_domain::{RunnerLeaseId, RunnerLeaseState, ToolDispatchAuthority};
use signalbox_persistence::runner_protocol::{RunnerProtocolStore, RunnerProtocolStoreError};
use std::sync::Arc;
use tokio::sync::{Semaphore, watch};

/// Shared local dispatch permit and durable-state wakeups for the runner connection.
#[derive(Clone, Debug)]
pub struct RunnerDispatchService {
    pub(crate) store: RunnerProtocolStore,
    permit: Arc<Semaphore>,
    changes: watch::Sender<()>,
}

impl RunnerDispatchService {
    pub(crate) fn new(store: RunnerProtocolStore) -> Self {
        let (changes, _) = watch::channel(());
        Self {
            store,
            permit: Arc::new(Semaphore::new(1)),
            changes,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }
    pub(crate) fn changed(&self) {
        self.changes.send_replace(());
    }

    pub(crate) async fn execute(
        &self,
        authority: &ToolDispatchAuthority,
    ) -> Result<bool, RunnerProtocolStoreError> {
        if self
            .store
            .load_placement(authority.correlation().session())
            .await?
            .is_none()
        {
            return Ok(false);
        }
        let _permit = self.permit.acquire().await.map_err(|_| {
            RunnerProtocolStoreError::Domain(signalbox_domain::RunnerDomainError::InvalidState)
        })?;
        let mut changes = self.subscribe();
        let offered = self
            .store
            .offer_tool_dispatch(authority, RunnerLeaseId::from_uuid(uuid::Uuid::now_v7()))
            .await;
        let mut lease = match offered {
            Ok(None) => return Ok(false),
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
            match lease.state() {
                RunnerLeaseState::Completed
                | RunnerLeaseState::LostUnclaimed
                | RunnerLeaseState::LostExecutionPossible
                | RunnerLeaseState::LostClaimed => return Ok(true),
                RunnerLeaseState::Offered | RunnerLeaseState::Claimed => {}
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
