//! Production effect routing and acknowledgement after durable journal delivery.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::*;
use crate::{
    repo_watch_runtime::RepositoryWatchRuntime,
    workflows::repo_watch::effects::{self, RepoWatchRequest},
};
use signalbox_module_repo_watch_v2::workflow::EffectReceipt;

pub(super) trait AttemptEffects: EffectExecutor {
    async fn acknowledge(&mut self) -> Result<(), LiveDeliveryFailure> {
        Ok(())
    }
}

pub(super) struct RuntimeEffects {
    repository_watch: Option<RepositoryWatchRuntime>,
    journal: ProgramJournalRepository,
    pending: Option<(ProgramRunId, EffectReceipt)>,
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
            pending: None,
            rejected: None,
        }
    }

    async fn flush(&mut self) -> Result<(), LiveDeliveryFailure> {
        if let (Some(runtime), Some((run, receipt))) = (&self.repository_watch, &self.pending) {
            runtime
                .acknowledge_workflow_receipt(&self.journal, *run, receipt)
                .await?;
            self.pending = None;
        }
        Ok(())
    }

    fn retain_answer(&mut self, invocation: EffectInvocation<'_>, result: &InlineFramePayload) {
        if let Some(
            RepoWatchRequest::CommitEvaluation { effect, .. }
            | RepoWatchRequest::SubmitPending { effect, .. },
        ) = RepoWatchRequest::decode(invocation.request)
        {
            self.pending = Some((
                invocation.run,
                EffectReceipt {
                    effect,
                    input: invocation.request.payload().as_bytes().to_vec(),
                    result: result.as_bytes().to_vec(),
                },
            ));
        }
    }
}

impl AttemptEffects for RuntimeEffects {
    async fn acknowledge(&mut self) -> Result<(), LiveDeliveryFailure> {
        self.flush().await
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
            self.flush().await?;
            if RepoWatchRequest::decode(invocation.request).is_some()
                && let Some(runtime) = &self.repository_watch
            {
                let result = runtime.adopt_workflow_effect(invocation).await?;
                if let Some(result) = &result {
                    self.retain_answer(invocation, result);
                }
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
            self.flush().await?;
            if RepoWatchRequest::decode(invocation.request).is_some()
                && let Some(runtime) = &self.repository_watch
            {
                let result = runtime.execute_workflow_effect(invocation).await?;
                self.retain_answer(invocation, &result);
                return Ok(result);
            }
            self.rejected = Some(invocation.request.capability());
            Err(LiveDeliveryFailure::new(
                "daemon workflow effect is unavailable",
            ))
        })
    }
}
