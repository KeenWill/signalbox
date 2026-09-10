//! Production effect routing and acknowledgement after durable journal delivery.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::*;
use crate::{
    repo_watch_runtime::RepositoryWatchRuntime,
    workflows::repo_watch::effects::{self, RepoWatchRequest},
};

pub(super) trait AttemptEffects: EffectExecutor {
    async fn acknowledge(&mut self) -> Result<(), LiveDeliveryFailure> {
        Ok(())
    }
}

pub(super) struct RuntimeEffects {
    repository_watch: Option<RepositoryWatchRuntime>,
    journal: ProgramJournalRepository,
    pub(super) rejected: Option<ProgramCapability>,
}

impl RuntimeEffects {
    pub(super) fn new(
        repository_watch: Option<RepositoryWatchRuntime>,
        journal: ProgramJournalRepository,
    ) -> Self {
        Self {
            repository_watch,
            journal,
            rejected: None,
        }
    }
}

impl AttemptEffects for RuntimeEffects {
    async fn acknowledge(&mut self) -> Result<(), LiveDeliveryFailure> {
        if let Some(runtime) = &self.repository_watch {
            runtime.acknowledge_workflow_receipts(&self.journal).await?;
        }
        Ok(())
    }
}

impl EffectExecutor for RuntimeEffects {
    fn recovery(&self, request: &EffectRequest) -> EffectRecovery {
        if RepoWatchRequest::decode(request).is_some() && self.repository_watch.is_some() {
            effects::recovery(request)
        } else {
            EffectRecovery::Idempotent
        }
    }

    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async move {
            self.acknowledge().await?;
            if RepoWatchRequest::decode(invocation.request).is_some()
                && let Some(runtime) = &self.repository_watch
            {
                let result = runtime.adopt_workflow_effect(invocation).await?;
                return Ok(result);
            }
            Ok(None)
        })
    }

    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            self.acknowledge().await?;
            if RepoWatchRequest::decode(invocation.request).is_some()
                && let Some(runtime) = &self.repository_watch
            {
                let result = runtime.execute_workflow_effect(invocation).await?;
                return Ok(result);
            }
            self.rejected = Some(invocation.request.capability());
            Err(LiveDeliveryFailure::new(
                "daemon workflow effect is unavailable",
            ))
        })
    }
}
