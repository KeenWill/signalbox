//! Poll/webhook workflow admission over the existing serialized repository worker.
//! Governed by docs/spec/repo-watch.md and docs/spec/workflows.md.

use super::*;
use crate::workflows::{
    WorkflowService,
    repo_watch::observe::{self, ObserveAnswer, ObserveInput, RepositoryObserver},
};
use signalbox_domain::{ProgramRegistrationId, ProgramRunId};
use signalbox_module_repo_watch_v2::{
    EventProducer, ingest::RepositoryTask, measurements::PollOutcome,
    observation_workflow::ObservationOutcome,
};
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_workflow_runtime::{LiveDeliveryFailure, native::NativeValue};
use uuid::Uuid;

pub(super) type Observers = Arc<Mutex<BTreeMap<RepositorySlug, Arc<Mutex<ConfiguredObserver>>>>>;

pub(super) struct ConfiguredObserver {
    pub(super) task: GitHubRepositoryTask<RepositoryWatchClientLoader>,
    pub(super) shutdown: watch::Receiver<bool>,
}

impl RepositoryObserver for ConfiguredObserver {
    fn repository(&self) -> &RepositorySlug {
        &self.task.repository
    }
    async fn observe(
        &mut self,
        store: RepoWatchStore,
        producer: EventProducer,
    ) -> Result<ObservationOutcome, LiveDeliveryFailure> {
        if *self.shutdown.borrow() {
            return Err(LiveDeliveryFailure::new("repository observation stopped"));
        }
        self.task.store = store;
        let result = tokio::select! {
            biased;
            _ = self.shutdown.changed() => return Err(LiveDeliveryFailure::new("repository observation stopped")),
            result = self.task.poll_outcome(producer) => result.map_err(observe::failure)?,
        };
        Ok(match result {
            PollOutcome::Succeeded => ObservationOutcome::Succeeded,
            PollOutcome::Partial => ObservationOutcome::Partial,
            PollOutcome::InProgress
            | PollOutcome::ClientFailed
            | PollOutcome::ObservationFailed
            | PollOutcome::StoreFailed
            | PollOutcome::FrontierConflict
            | PollOutcome::Cancelled => ObservationOutcome::Failed,
        })
    }
}

impl RepositoryWatchRuntime {
    pub fn set_workflow_service(&self, service: WorkflowService) {
        let _ = self.workflow_service.set(service);
    }

    pub(crate) async fn adopt_observation(
        &self,
        input: &ObserveInput,
    ) -> Result<Option<signalbox_domain::InlineFramePayload>, LiveDeliveryFailure> {
        observe::adopt(&self.measurements_store, input).await
    }

    pub(crate) async fn execute_observation(
        &self,
        input: &ObserveInput,
    ) -> Result<signalbox_domain::InlineFramePayload, LiveDeliveryFailure> {
        if let Some(result) = self.adopt_observation(input).await? {
            return Ok(result);
        }
        let observer = self.observers.lock().await.get(input.repository()).cloned();
        let Some(observer) = observer else {
            return Ok(signalbox_domain::InlineFramePayload::new(
                ObserveAnswer::Ambiguous
                    .encode()
                    .map_err(observe::failure)?,
            ));
        };
        observe::execute(&self.measurements_store, input, &mut *observer.lock().await).await
    }
}

pub(super) struct WorkflowRepositoryTask {
    pub(super) repository: RepositorySlug,
    pub(super) store: RepoWatchStore,
    pub(super) core: PgPool,
    pub(super) service: WorkflowService,
    pub(super) registration: Option<ProgramRegistrationId>,
    pub(super) production: Option<GitHubRepositoryTask<RepositoryWatchClientLoader>>,
    pub(super) current: Option<ProgramRunId>,
}

impl RepositoryTask for WorkflowRepositoryTask {
    type Error = LiveDeliveryFailure;

    async fn poll(&mut self, producer: EventProducer) -> Result<(), Self::Error> {
        let journal = ProgramJournalRepository::new(self.core.clone());
        loop {
            observe::acknowledge(&self.store, &journal).await?;
            let receipt = self
                .store
                .observation_receipt(&self.repository)
                .await
                .map_err(observe::failure)?;
            let recovering = receipt.is_some();
            if !recovering && let Some(production) = &mut self.production {
                return production.poll(producer).await.map_err(observe::failure);
            }
            let input = match receipt {
                Some(receipt) => ObserveInput::decode(&receipt.input).map_err(observe::failure)?,
                None => ObserveInput::new(self.repository.clone(), producer)
                    .map_err(observe::failure)?,
            };
            let mut listener = journal.listen_all().await.map_err(observe::failure)?;
            let registration = match self.registration {
                Some(registration) => registration,
                None => {
                    let registration = self
                        .service
                        .observation_registration()
                        .await
                        .map_err(observe::failure)?;
                    self.registration = Some(registration);
                    registration
                }
            };
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            self.current = Some(run);
            self.service
                .start(
                    run,
                    registration,
                    &input.encode().map_err(observe::failure)?,
                )
                .await
                .map_err(observe::failure)?;
            let result = loop {
                let loaded = journal
                    .load(run)
                    .await
                    .map_err(observe::failure)?
                    .ok_or_else(|| LiveDeliveryFailure::new("observation run is missing"))?;
                if let Some(result) = loaded.result() {
                    break ObserveAnswer::decode(result.as_bytes()).map_err(observe::failure)?;
                }
                if loaded.terminal_delivery().is_some() {
                    return Err(LiveDeliveryFailure::new(
                        "observation run ended without a result",
                    ));
                }
                listener.changed().await.map_err(observe::failure)?;
            };
            self.current = None;
            observe::acknowledge(&self.store, &journal).await?;
            if !recovering {
                return match result {
                    ObserveAnswer::Observed(result)
                        if result.outcome != ObservationOutcome::Failed =>
                    {
                        Ok(())
                    }
                    ObserveAnswer::Observed(_) | ObserveAnswer::Ambiguous => Err(
                        LiveDeliveryFailure::new("repository observation did not complete"),
                    ),
                };
            }
        }
    }

    async fn shutdown(&mut self) {
        if let Some(run) = self.current.take()
            && let Err(error) = self.service.cancel_observation(&self.core, run).await
        {
            tracing::warn!(?run, %error, "repository observation cancellation failed");
        }
    }
}
