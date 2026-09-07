//! Daemon composition of repository ingestion, dispatch, and lifecycle consumption.

use std::{collections::BTreeMap, sync::Arc};

use ring::rand::{SecureRandom, SystemRandom};
use signalbox_application::{InProcessEligibilityNudge, InProcessToolDispatchGate};
use signalbox_domain::RepositorySlug;
use signalbox_module_repo_watch_v2::{
    RepoWatchStore, RepositoryRuleSet, RuleReconciliationAdmission, ingest::run_repository_task,
    provider::GitHubRepositoryTask,
};
use signalbox_ownership_seam::{LifecycleEventSource, OffsetDateTime};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    sync::{Mutex, Notify, watch},
    task::JoinSet,
};

use crate::{
    HubModelConfiguration, RepositoryWatchConfiguration, SessionTemplateConfiguration,
    repo_watch_credentials::RepositoryWatchClientLoader,
    repo_watch_dispatch::{
        RepositoryWatchCommandCodec, RepositoryWatchCommandFactory, RepositoryWatchCommandSink,
        RepositoryWatchDispatchIds,
    },
    repo_watch_webhook::WebhookListener,
};

/// Core capabilities remain in daemon-owned adapters; the module receives its own pool.
pub struct RepositoryWatchServices {
    pub core_pool: PgPool,
    pub models: Arc<HubModelConfiguration>,
    pub templates: Arc<SessionTemplateConfiguration>,
    pub eligibility_nudge: InProcessEligibilityNudge,
    pub tool_dispatch_gate: InProcessToolDispatchGate,
}

/// Closed startup, reload, and worker failures contain no provider or credential data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchRuntimeError {
    ModuleConnection,
    Rules,
    Listener,
    RepositoryWorker,
    Lifecycle,
    Dispatch,
}

/// Opens an independently authenticated module login without sharing the core password.
pub async fn connect_repository_watch_pool(
    core: &PgPool,
) -> Result<PgPool, RepositoryWatchRuntimeError> {
    // A fresh 256-bit login secret belongs only to this daemon's module pool.
    let mut secret = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    let password = hex::encode(secret);
    let mut transaction = core
        .begin()
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    // Parameter binding keeps the secret out of statement text and diagnostics.
    sqlx::query("SELECT set_config('signalbox.repository_watch_password', $1, true)")
        .bind(&password)
        .execute(&mut *transaction)
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    sqlx::query("DO $$ BEGIN EXECUTE format('ALTER ROLE mod_repo_watch PASSWORD %L', current_setting('signalbox.repository_watch_password')); END $$")
        .execute(&mut *transaction).await.map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    transaction
        .commit()
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET search_path = mod_repo_watch, pg_catalog")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(
            core.connect_options()
                .as_ref()
                .clone()
                .username("mod_repo_watch")
                .password(&password),
        )
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)
}

/// A reload handle and one serialized command worker for the compiled-in module.
#[derive(Clone)]
pub struct RepositoryWatchRuntime {
    state: Arc<Mutex<RuntimeState>>,
}

enum WorkerState {
    Prepared,
    Running,
}

struct RuntimeState {
    workers: WorkerState,
    module_pool: PgPool,
    store: RepoWatchStore,
    lifecycle: LifecycleEventSource,
    factory: RepositoryWatchCommandFactory,
    sink: RepositoryWatchCommandSink,
    configuration: Option<RepositoryWatchConfiguration>,
    wakes: BTreeMap<RepositorySlug, Arc<Notify>>,
    repositories: JoinSet<()>,
    repository_shutdown: watch::Sender<bool>,
    listener: WebhookListener,
}

impl RepositoryWatchRuntime {
    /// Reconciles the configured revision set before starting repository tasks.
    pub async fn new(
        module_pool: PgPool,
        configuration: Option<RepositoryWatchConfiguration>,
        services: RepositoryWatchServices,
    ) -> Result<Self, RepositoryWatchRuntimeError> {
        let (repository_shutdown, _) = watch::channel(false);
        let runtime = Self {
            state: Arc::new(Mutex::new(RuntimeState {
                workers: WorkerState::Prepared,
                store: RepoWatchStore::new(module_pool.clone()),
                module_pool,
                lifecycle: LifecycleEventSource::new(services.core_pool.clone()),
                factory: RepositoryWatchCommandFactory(services.templates),
                sink: RepositoryWatchCommandSink {
                    pool: services.core_pool,
                    models: services.models,
                    eligibility_nudge: services.eligibility_nudge,
                    tool_dispatch_gate: services.tool_dispatch_gate,
                },
                configuration: None,
                wakes: BTreeMap::new(),
                repositories: JoinSet::new(),
                repository_shutdown,
                listener: WebhookListener::default(),
            })),
        };
        runtime.reload_configuration(configuration).await?;
        Ok(runtime)
    }

    /// Reconciles rules and changes the listener inside the serialized reload.
    ///
    /// Same-address changes atomically replace routing. Address changes bind first;
    /// a bind failure leaves the running configuration intact. Interrupted repository
    /// deliveries retry from the committed baseline in the replacement task.
    pub async fn reload_configuration(
        &self,
        configuration: Option<RepositoryWatchConfiguration>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        let mut state = self.state.lock().await;
        let enabled = configuration
            .as_ref()
            .filter(|configuration| configuration.enabled());
        if enabled
            .into_iter()
            .flat_map(|configuration| configuration.rules())
            .flat_map(|rule| rule.actions())
            .any(|action| state.factory.0.resolve(action.template()).is_none())
        {
            return Err(RepositoryWatchRuntimeError::Rules);
        }
        let wakes: BTreeMap<_, _> = enabled
            .into_iter()
            .flat_map(|configuration| configuration.repositories())
            .map(|repository| {
                (
                    repository.repository().clone(),
                    state
                        .wakes
                        .get(repository.repository())
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        let listener = state
            .listener
            .prepare(configuration.as_ref(), &wakes, &state.store)
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Listener)?;
        let rules = enabled
            .into_iter()
            .flat_map(|configuration| {
                configuration.repositories().iter().map(move |repository| {
                    RepositoryRuleSet::new(repository.repository(), configuration.rules())
                })
            })
            .collect::<Vec<_>>();
        state.stop_repositories().await;
        match state
            .store
            .reconcile_rules(&rules, OffsetDateTime::now_utc())
            .await
        {
            Ok(RuleReconciliationAdmission::Applied { .. }) => {}
            Ok(
                RuleReconciliationAdmission::ConflictingReuse | RuleReconciliationAdmission::Stale,
            )
            | Err(_) => {
                if matches!(state.workers, WorkerState::Running) {
                    state.start_repositories();
                }
                return Err(RepositoryWatchRuntimeError::Rules);
            }
        }
        state.wakes = wakes;
        state.configuration = configuration;
        state.listener.apply(listener).await;
        if matches!(state.workers, WorkerState::Running) {
            state.listener.start();
            state.start_repositories();
        }
        Ok(())
    }

    /// Runs one lifecycle/evaluation/submission worker until daemon shutdown.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        {
            let mut state = self.state.lock().await;
            state.workers = WorkerState::Running;
            state.listener.start();
            state.start_repositories();
        }
        let outcome = loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break Ok(()),
                result = async { self.state.lock().await.tick().await } => {
                    if let Err(error) = result {
                        if matches!(error, RepositoryWatchRuntimeError::Listener | RepositoryWatchRuntimeError::RepositoryWorker) { break Err(error); }
                        tracing::warn!(?error, "repository-watch command attempt failed");
                    }
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break Ok(()),
                _ = tokio::time::sleep(crate::process_runtime::OUTBOX_IDLE_POLL_INTERVAL) => {}
            }
        };
        let mut state = self.state.lock().await;
        state.stop_repositories().await;
        state.listener.shutdown().await;
        state.workers = WorkerState::Prepared;
        state.module_pool.close().await;
        outcome
    }
}

impl RuntimeState {
    async fn stop_repositories(&mut self) {
        let _ = self.repository_shutdown.send(true);
        while self.repositories.join_next().await.is_some() {}
    }

    fn start_repositories(&mut self) {
        let (shutdown, _) = watch::channel(false);
        self.repository_shutdown = shutdown;
        if let Some(configuration) = self
            .configuration
            .as_ref()
            .filter(|configuration| configuration.enabled())
        {
            for repository in configuration.repositories() {
                let Some(wake) = self.wakes.get(repository.repository()).cloned() else {
                    continue;
                };
                let task = GitHubRepositoryTask {
                    repository: repository.repository().clone(),
                    signal_reviewers: configuration.signal_reviewers().to_vec(),
                    clients: RepositoryWatchClientLoader::new(repository),
                    store: self.store.clone(),
                };
                self.repositories.spawn(run_repository_task(
                    task,
                    repository.poll_interval(),
                    wake,
                    self.repository_shutdown.subscribe(),
                ));
            }
        }
    }

    async fn tick(&mut self) -> Result<(), RepositoryWatchRuntimeError> {
        if self.listener.failed() {
            return Err(RepositoryWatchRuntimeError::Listener);
        }
        if self.repositories.try_join_next().is_some() {
            return Err(RepositoryWatchRuntimeError::RepositoryWorker);
        }
        let Some(configuration) = self
            .configuration
            .as_ref()
            .filter(|configuration| configuration.enabled())
        else {
            return Ok(());
        };
        let mut codec = RepositoryWatchCommandCodec;
        if let Some(event) = self
            .lifecycle
            .next()
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Lifecycle)?
        {
            self.store
                .react_to_lifecycle(&event, &mut self.factory, &mut codec)
                .await
                .map_err(|_| RepositoryWatchRuntimeError::Lifecycle)?;
            self.lifecycle
                .acknowledge(&event)
                .await
                .map_err(|_| RepositoryWatchRuntimeError::Lifecycle)?;
        }
        for repository in configuration.repositories() {
            for rule in configuration.rules() {
                self.store
                    .evaluate_next(
                        repository.repository(),
                        rule,
                        &mut RepositoryWatchDispatchIds,
                        &mut self.factory,
                        &mut codec,
                        OffsetDateTime::now_utc(),
                    )
                    .await
                    .map_err(|_| RepositoryWatchRuntimeError::Dispatch)?;
            }
        }
        crate::repo_watch_dispatch::submit_pending(&self.store, configuration, &mut self.sink)
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Dispatch)?;
        Ok(())
    }
}
