//! Daemon composition of repository ingestion, dispatch, and lifecycle consumption.

use std::{collections::BTreeMap, sync::Arc};

use ring::rand::{SecureRandom, SystemRandom};
use signalbox_application::{
    EligibilityNudge, InProcessEligibilityNudge, InProcessToolDispatchGate,
};
use signalbox_domain::RepositorySlug;
use signalbox_module_repo_watch_v2::{
    ReloadIntentInput, RepoWatchStore, RepositoryRuleSet, RuleReconciliationAdmission,
    ingest::run_repository_task, provider::GitHubRepositoryTask,
};
use signalbox_ownership_seam::{LifecycleEventSource, OffsetDateTime};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    sync::{Mutex, Notify, watch},
    task::{JoinHandle, JoinSet},
};

use crate::{
    HubModelConfiguration, RepositoryWatchConfiguration, SessionTemplateConfiguration,
    configuration_reload::ConfigurationCatalogs,
    convergence_sweep_runtime::{ConvergenceSweepNumericBounds, ConvergenceSweepRuntime},
    repo_watch_credentials::RepositoryWatchClientLoader,
    repo_watch_dispatch::{
        RepositoryWatchCommandCodec, RepositoryWatchCommandFactory, RepositoryWatchCommandSink,
        RepositoryWatchDispatchIds,
    },
    repo_watch_webhook::{PreparedListener, WebhookListener},
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
    Sweep,
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
    paused: bool,
    changed: Arc<Notify>,
    core_pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    sweep_bounds: Option<ConvergenceSweepNumericBounds>,
    sweep: Option<(watch::Sender<bool>, JoinHandle<()>)>,
    prepared_sweep: Option<ConvergenceSweepRuntime>,
}

pub(crate) struct PreparedRepositoryWatchReload {
    configuration: Option<RepositoryWatchConfiguration>,
    catalogs: ConfigurationCatalogs,
    wakes: BTreeMap<RepositorySlug, Arc<Notify>>,
    listener: PreparedListener,
    sweep: Option<ConvergenceSweepRuntime>,
}

impl RepositoryWatchRuntime {
    /// Reconciles the configured revision set before starting repository tasks.
    pub async fn new(
        module_pool: PgPool,
        configuration: Option<RepositoryWatchConfiguration>,
        services: RepositoryWatchServices,
    ) -> Result<Self, RepositoryWatchRuntimeError> {
        let runtime = Self::unstarted(module_pool, services);
        runtime.reload_configuration(configuration).await?;
        Ok(runtime)
    }

    /// Composes the idle supervisor without activating on-disk rules before recovery.
    pub fn unstarted(module_pool: PgPool, services: RepositoryWatchServices) -> Self {
        let (repository_shutdown, _) = watch::channel(false);
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                workers: WorkerState::Prepared,
                paused: true,
                changed: Arc::new(Notify::new()),
                core_pool: services.core_pool.clone(),
                eligibility_nudge: services.eligibility_nudge.clone(),
                sweep_bounds: None,
                sweep: None,
                prepared_sweep: None,
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
        }
    }

    /// Supplies the startup-only convergence transport bounds.
    pub async fn set_sweep_bounds(&self, bounds: ConvergenceSweepNumericBounds) {
        self.state.lock().await.sweep_bounds = Some(bounds);
    }

    pub(crate) async fn prepare_reload(
        &self,
        catalogs: ConfigurationCatalogs,
    ) -> Result<PreparedRepositoryWatchReload, RepositoryWatchRuntimeError> {
        let state = self.state.lock().await;
        let configuration = catalogs.models.repository_watch().cloned();
        let enabled = configuration
            .as_ref()
            .filter(|configuration| configuration.enabled());
        let wakes = enabled
            .into_iter()
            .flat_map(|watch| watch.repositories())
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
            .collect::<BTreeMap<_, _>>();
        let listener = state
            .listener
            .prepare(configuration.as_ref(), &wakes, &state.store)
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Listener)?;
        let sweep = match (enabled, state.sweep_bounds) {
            (Some(watch), Some(bounds)) => ConvergenceSweepRuntime::try_new(
                state.core_pool.clone(),
                watch,
                (*catalogs.templates).clone(),
                (*catalogs.models).clone(),
                state.eligibility_nudge.clone(),
                bounds,
            )
            .map_err(|_| RepositoryWatchRuntimeError::Sweep)?,
            _ => None,
        };
        Ok(PreparedRepositoryWatchReload {
            configuration,
            catalogs,
            wakes,
            listener,
            sweep,
        })
    }

    pub(crate) async fn nudge_restored(&self, sessions: Vec<signalbox_domain::SessionId>) {
        let state = self.state.lock().await;
        for session in sessions {
            let _ = state.eligibility_nudge.nudge(session);
        }
    }

    pub(crate) async fn activate_reload(
        &self,
        input: ReloadIntentInput<'_>,
    ) -> Result<RuleReconciliationAdmission, RepositoryWatchRuntimeError> {
        let mut state = self.state.lock().await;
        state.pause().await;
        state
            .store
            .activate_reload(input, OffsetDateTime::now_utc())
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Rules)
    }

    pub(crate) async fn install_reload(&self, prepared: PreparedRepositoryWatchReload) {
        let mut state = self.state.lock().await;
        state.factory.0 = prepared.catalogs.templates;
        state.sink.models = prepared.catalogs.models;
        state.configuration = prepared.configuration;
        state.wakes = prepared.wakes;
        state.listener.apply_joined(prepared.listener).await;
        state.paused = false;
        if matches!(state.workers, WorkerState::Running) {
            state.listener.resume().await;
            state.listener.start();
            state.start_repositories();
            state.start_sweep(prepared.sweep);
        } else {
            // Startup workers are composed only after recovery settles.
            state.prepared_sweep = prepared.sweep;
        }
        state.changed.notify_one();
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
        state.paused = false;
        state.changed.notify_one();
        state.listener.apply(listener).await;
        if matches!(state.workers, WorkerState::Running) {
            state.listener.start();
            state.start_repositories();
        }
        Ok(())
    }

    async fn begin(&self) {
        {
            let mut state = self.state.lock().await;
            if matches!(state.workers, WorkerState::Running) {
                return;
            }
            state.workers = WorkerState::Running;
            if !state.paused {
                state.listener.resume().await;
                state.listener.start();
                state.start_repositories();
                let sweep = state.prepared_sweep.take();
                state.start_sweep(sweep);
            }
        }
    }

    /// Starts idle supervision before recovery so resumed workers precede terminal receipts.
    pub async fn spawn(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<Result<(), RepositoryWatchRuntimeError>> {
        self.begin().await;
        tokio::spawn(self.run(shutdown))
    }

    /// Runs one lifecycle/evaluation/submission worker until daemon shutdown.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        self.begin().await;
        let outcome = loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            let (active, changed) = {
                let state = self.state.lock().await;
                (
                    !state.paused && state.configuration.as_ref().is_some_and(|c| c.enabled()),
                    state.changed.clone(),
                )
            };
            if !active {
                tokio::select! {
                    _ = shutdown.changed() => break Ok(()),
                    _ = changed.notified() => continue,
                }
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
        state.pause().await;
        state.listener.shutdown().await;
        state.workers = WorkerState::Prepared;
        state.module_pool.close().await;
        outcome
    }
}

impl RuntimeState {
    async fn pause(&mut self) {
        self.paused = true;
        if let Some((shutdown, task)) = self.sweep.take() {
            let _ = shutdown.send(true);
            let _ = task.await;
        }
        self.listener.pause().await;
        self.stop_repositories().await;
    }

    fn start_sweep(&mut self, sweep: Option<ConvergenceSweepRuntime>) {
        if let Some(sweep) = sweep {
            let (shutdown, receiver) = watch::channel(false);
            self.sweep = Some((shutdown, tokio::spawn(sweep.run(receiver))));
        }
    }

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
        if self.paused {
            return Ok(());
        }
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
        self.store
            .submit_pending(&mut codec, &mut self.sink)
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Dispatch)?;
        Ok(())
    }
}
