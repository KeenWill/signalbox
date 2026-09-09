//! Workflow effects share repository-watch configuration and command serialization.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::*;
use crate::{
    repo_watch_dispatch::CheckoutCommandSink, workflows::repo_watch::effects::RepoWatchEffects,
};
use signalbox_domain::{InlineFramePayload, ProgramRunId};
use signalbox_module_repo_watch_v2::workflow::EffectReceipt;
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_workflow_runtime::{
    LiveDeliveryFailure,
    effects::{EffectExecutor, EffectInvocation},
};

impl RepositoryWatchRuntime {
    pub(crate) async fn adopt_workflow_effect(
        &self,
        invocation: EffectInvocation<'_>,
    ) -> Result<Option<InlineFramePayload>, LiveDeliveryFailure> {
        self.state
            .lock()
            .await
            .workflow_effects()
            .adopt(invocation)
            .await
    }

    pub(crate) async fn execute_workflow_effect(
        &self,
        invocation: EffectInvocation<'_>,
    ) -> Result<InlineFramePayload, LiveDeliveryFailure> {
        self.state
            .lock()
            .await
            .workflow_effects()
            .execute(invocation)
            .await
    }

    pub(crate) async fn acknowledge_workflow_receipt(
        &self,
        journals: &ProgramJournalRepository,
        run: ProgramRunId,
        receipt: &EffectReceipt,
    ) -> Result<(), LiveDeliveryFailure> {
        self.state
            .lock()
            .await
            .workflow_effects()
            .acknowledge_receipt(journals, run, receipt)
            .await
    }
}

impl RuntimeState {
    fn workflow_effects(
        &mut self,
    ) -> RepoWatchEffects<
        RepositoryWatchDispatchIds,
        RepositoryWatchCommandFactory,
        RepositoryWatchCommandCodec,
        CheckoutCommandSink<'_, signalbox_tools_exec::TokioProcessRunner>,
    > {
        let configuration = self
            .configuration
            .as_ref()
            .filter(|configuration| configuration.enabled());
        let rules = configuration
            .into_iter()
            .flat_map(|configuration| {
                configuration.repositories().iter().map(|repository| {
                    (
                        repository.repository().clone(),
                        configuration.rules().to_vec(),
                    )
                })
            })
            .collect();
        let runner = self.sink.checkout_runner.clone();
        RepoWatchEffects {
            store: self.store.clone(),
            rules,
            ids: RepositoryWatchDispatchIds,
            factory: self.factory.clone(),
            codec: RepositoryWatchCommandCodec,
            sink: CheckoutCommandSink {
                store: &self.store,
                configuration,
                core: &mut self.sink,
                runner,
            },
            source: self.lifecycle.clone(),
        }
    }
}
