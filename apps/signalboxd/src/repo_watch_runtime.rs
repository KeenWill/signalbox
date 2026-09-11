//! Daemon composition of repository ingestion, dispatch, and lifecycle consumption.

mod observation;
mod workflows;

use std::{collections::BTreeMap, sync::Arc};

use ring::rand::{SecureRandom, SystemRandom};
use signalbox_application::{
    EligibilityNudgeOutcome, InProcessEligibilityNudge, InProcessToolDispatchGate,
};
use signalbox_domain::RepositorySlug;
use signalbox_module_repo_watch_v2::{
    ReloadIntentInput, RepoWatchStore, RepositoryRuleSet, RuleReconciliationAdmission,
    ingest::run_repository_task, provider::GitHubRepositoryTask,
};
use signalbox_session_ownership::{LifecycleEventSource, OffsetDateTime};
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

/// Selects the configured repository granting push authority for the retained PR head.
pub(crate) fn git_push_repository<'a>(
    configuration: &'a RepositoryWatchConfiguration,
    event: &signalbox_domain::RepoWatchEvent,
) -> Option<&'a crate::WatchedRepositoryConfiguration> {
    let signalbox_domain::RepoWatchEventTarget::PullRequest(context) = event.target() else {
        return None;
    };
    configuration.repositories().iter().find(|repository| {
        repository.repository() == event.repository()
            && repository.repository() == context.head_repository()
            && repository.admits_push()
    })
}

/// Core capabilities remain in daemon-owned adapters; the module receives its own pool.
pub struct RepositoryWatchServices {
    pub goal_resumption: crate::PostgresGoalPassDisposition,
    pub checkout_runner: Option<signalbox_tools_exec::TokioProcessRunner>,
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
    // Each database has its own login; PostgreSQL role passwords are cluster-wide.
    let mut secret = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    let password = hex::encode(secret);
    let mut transaction = core
        .begin()
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    let login: String = sqlx::query_scalar(
        "SELECT 'mod_repo_watch_' || oid::text FROM pg_database WHERE datname = current_database()",
    )
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    sqlx::query("SELECT set_config('signalbox.repository_watch_login', $1, true)")
        .bind(&login)
        .execute(&mut *transaction)
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    // Parameter binding keeps the secret out of statement text and diagnostics.
    sqlx::query("SELECT set_config('signalbox.repository_watch_password', $1, true)")
        .bind(&password)
        .execute(&mut *transaction)
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    sqlx::query(
        "DO $$
         DECLARE module_login text := current_setting('signalbox.repository_watch_login');
         BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = module_login) THEN
                 EXECUTE format('CREATE ROLE %I LOGIN NOINHERIT', module_login);
             END IF;
             EXECUTE format('ALTER ROLE %I PASSWORD %L', module_login,
                            current_setting('signalbox.repository_watch_password'));
             EXECUTE format('GRANT mod_repo_watch TO %I', module_login);
         END $$",
    )
    .execute(&mut *transaction)
    .await
    .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    transaction
        .commit()
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)?;
    PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::raw_sql(
                    "SET ROLE mod_repo_watch; SET search_path = mod_repo_watch, pg_catalog",
                )
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .connect_with(
            core.connect_options()
                .as_ref()
                .clone()
                .username(&login)
                .password(&password),
        )
        .await
        .map_err(|_| RepositoryWatchRuntimeError::ModuleConnection)
}

/// A reload handle and one serialized command worker for the compiled-in module.
#[derive(Clone)]
pub struct RepositoryWatchRuntime {
    store: RepoWatchStore,
    state: Arc<Mutex<RuntimeState>>,
    observers: observation::Observers,
    workflow_service: Arc<std::sync::OnceLock<crate::workflows::WorkflowService>>,
}

impl std::fmt::Debug for RepositoryWatchRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryWatchRuntime")
            .finish_non_exhaustive()
    }
}

enum WorkerState {
    Prepared,
    Running,
}

struct RuntimeState {
    observers: observation::Observers,
    workflow_service: Arc<std::sync::OnceLock<crate::workflows::WorkflowService>>,
    workers: WorkerState,
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
    commands: Option<(watch::Sender<bool>, JoinHandle<()>)>,
}

pub(crate) struct PreparedRepositoryWatchReload {
    configuration: Option<RepositoryWatchConfiguration>,
    catalogs: ConfigurationCatalogs,
    wakes: BTreeMap<RepositorySlug, Arc<Notify>>,
    listener: PreparedListener,
    sweep: Option<ConvergenceSweepRuntime>,
}

impl RepositoryWatchRuntime {
    /// Loads the pull-request fence retained for the session's creation dispatch.
    pub async fn approval_judge_authority(
        &self,
        session: signalbox_domain::SessionId,
    ) -> Result<
        Option<signalbox_application::ApprovalJudgeDispatchAuthority>,
        signalbox_module_repo_watch_v2::StoreError,
    > {
        use signalbox_application::{
            ApprovalJudgeDispatchAuthority, ApprovalJudgeDispatchProvenance,
            ApprovalJudgePullRequestAuthority, ApprovalJudgePullRequestAuthorityInput,
        };
        use signalbox_module_repo_watch_v2::StoreError;
        let (store, core) = {
            let state = self.state.lock().await;
            (state.store.clone(), state.core_pool.clone())
        };
        let origin = signalbox_persistence::session::SessionRepository::new(core)
            .repository_watch_creation_dispatch(session)
            .await
            .map_err(|error| match error {
                signalbox_persistence::session::SessionRepositoryError::Database(error) => {
                    StoreError::Database(error)
                }
                signalbox_persistence::session::SessionRepositoryError::Corruption(_) => {
                    StoreError::InvalidRetainedCommand
                }
            })?;
        let Some(origin) = origin else {
            return Ok(None);
        };
        let checkout = store
            .dispatch_checkout(origin.command)
            .await?
            .ok_or(StoreError::InvalidRetainedCommand)?;
        if checkout.dispatch != origin.dispatch {
            return Err(StoreError::InvalidRetainedCommand);
        }
        let signalbox_domain::RepoWatchEventTarget::PullRequest(context) = checkout.event.target()
        else {
            return Ok(None);
        };
        Ok(Some(ApprovalJudgeDispatchAuthority::PullRequest(
            ApprovalJudgePullRequestAuthority::new(ApprovalJudgePullRequestAuthorityInput {
                dispatch: ApprovalJudgeDispatchProvenance::RepoWatch(checkout.dispatch),
                repository: checkout.event.repository().clone(),
                pull_request: context.number(),
                head_sha: context.head_sha().clone(),
                head_repository: context.head_repository().clone(),
                head_branch: context.head_branch().clone(),
                base_branch: context.base_branch().clone(),
            }),
        )))
    }

    pub(crate) fn ingestion_measurements(
        &self,
        repository: &RepositorySlug,
    ) -> signalbox_module_repo_watch_v2::measurements::IngestionMeasurements {
        self.store.ingestion_measurements(repository)
    }
    pub(crate) async fn session_origin(
        &self,
        session: signalbox_domain::SessionId,
        core: &PgPool,
    ) -> Result<
        Option<signalbox_module_repo_watch_v2::RetainedDispatchAction>,
        signalbox_module_repo_watch_v2::StoreError,
    > {
        let origin: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "SELECT session.dispatch_ref, creation.command_id
               FROM session
               JOIN create_session_command AS creation
                 ON creation.created_session_id = session.session_id
              WHERE session.session_id = $1
                AND session.creation_cause = 'module_dispatched'
                AND session.dispatching_module = 'repo_watch'",
        )
        .bind(session.into_uuid())
        .fetch_optional(core)
        .await?;
        let Some((dispatch, command)) = origin else {
            return Ok(None);
        };
        self.store
            .origin_for_create_command(
                signalbox_session_ownership::RepoWatchDispatchId::from_uuid(dispatch),
                signalbox_session_ownership::DurableCommandId::from_uuid(command),
            )
            .await?
            .map(Some)
            .ok_or(signalbox_module_repo_watch_v2::StoreError::InvalidRetainedCommand)
    }

    pub(crate) async fn git_push_authority(
        &self,
        session: signalbox_domain::SessionId,
    ) -> Result<
        Option<(
            crate::WatchedRepositoryConfiguration,
            signalbox_domain::BranchName,
            signalbox_domain::CommitSha,
        )>,
        signalbox_module_repo_watch_v2::StoreError,
    > {
        use signalbox_application::ApprovalJudgeDispatchAuthority;
        use signalbox_module_repo_watch_v2::StoreError;
        use signalbox_persistence::approval_judge::{
            ApprovalJudgeRepositoryError, load_commissioned_dispatch_authority,
        };
        let (core, configuration) = {
            let state = self.state.lock().await;
            (state.core_pool.clone(), state.configuration.clone())
        };
        let Some(configuration) = configuration else {
            return Ok(None);
        };
        let commissioned = load_commissioned_dispatch_authority(
            &mut *core.acquire().await?, session,
        ).await.map_err(|error| match error {
            ApprovalJudgeRepositoryError::Database { source, .. } => StoreError::Database(source),
            error => {
                tracing::error!(session_id = %session.into_uuid(), cause = %error, "retained push fence could not be loaded");
                StoreError::InvalidRetainedCommand
            }
        })?;
        let authority = match commissioned {
            Some(authority) => Some(authority),
            None => self.approval_judge_authority(session).await?,
        };
        let Some(ApprovalJudgeDispatchAuthority::PullRequest(context)) = authority else {
            return Ok(None);
        };
        Ok(configuration
            .repositories()
            .iter()
            .find(|repository| {
                repository.repository() == context.repository()
                    && repository.repository() == context.head_repository()
                    && repository.admits_push()
            })
            .map(|repository| {
                (
                    repository.clone(),
                    context.head_branch().clone(),
                    context.head_sha().clone(),
                )
            }))
    }

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
        let store = RepoWatchStore::new(module_pool);
        let observers = observation::Observers::default();
        let workflow_service = Arc::new(std::sync::OnceLock::new());
        Self {
            store: store.clone(),
            observers: observers.clone(),
            workflow_service: workflow_service.clone(),
            state: Arc::new(Mutex::new(RuntimeState {
                observers,
                workflow_service,
                workers: WorkerState::Prepared,
                paused: true,
                changed: Arc::new(Notify::new()),
                core_pool: services.core_pool.clone(),
                eligibility_nudge: services.eligibility_nudge.clone(),
                sweep_bounds: None,
                sweep: None,
                prepared_sweep: None,
                commands: None,
                store,
                lifecycle: LifecycleEventSource::new(services.core_pool.clone()),
                factory: RepositoryWatchCommandFactory(services.templates),
                sink: RepositoryWatchCommandSink {
                    goal_resumption: services.goal_resumption,
                    checkout_runner: services.checkout_runner,
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
        if enabled
            .into_iter()
            .flat_map(|watch| watch.rules())
            .flat_map(|rule| rule.actions())
            .any(|action| catalogs.templates.resolve(action.template()).is_none())
        {
            return Err(RepositoryWatchRuntimeError::Rules);
        }
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
        let (nudge, store) = {
            let state = self.state.lock().await;
            (
                state.eligibility_nudge.clone(),
                signalbox_persistence::convergence_sweep::PostgresConvergenceSweepStore::new(
                    state.core_pool.clone(),
                ),
            )
        };
        // Startup recovery hands off before the scheduler drains the buffer.
        // The target references remain durable if this task stops with the process.
        tokio::spawn(async move {
            for session in sessions {
                if nudge.nudge_waiting_for_capacity(session).await
                    == EligibilityNudgeOutcome::WorkSourceClosed
                {
                    break;
                }
                if let Err(error) = store.acknowledge_removed_target_nudge(session).await {
                    tracing::error!(cause = %error, "removed-target nudge acknowledgement failed; handoff remains pending");
                }
            }
        });
    }

    pub(crate) async fn activate_startup(
        &self,
        configuration: Option<&RepositoryWatchConfiguration>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        let mut state = self.state.lock().await;
        state.pause().await;
        let sets = configuration
            .filter(|configuration| configuration.enabled())
            .into_iter()
            .flat_map(|watch| {
                watch.repositories().iter().map(move |repository| {
                    RepositoryRuleSet::new(repository.repository(), watch.rules())
                })
            })
            .collect::<Vec<_>>();
        match state
            .store
            .reconcile_rules(&sets, OffsetDateTime::now_utc())
            .await
        {
            Ok(RuleReconciliationAdmission::Applied { .. }) => Ok(()),
            _ => Err(RepositoryWatchRuntimeError::Rules),
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

    pub(crate) async fn install_reload(
        &self,
        prepared: PreparedRepositoryWatchReload,
    ) -> Result<(), RepositoryWatchRuntimeError> {
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
            state.start_repositories().await?;
            state.start_commands(self.clone());
            state.start_sweep(prepared.sweep);
        } else {
            // Startup workers are composed only after recovery settles.
            state.prepared_sweep = prepared.sweep;
        }
        state.changed.notify_one();
        Ok(())
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
        state.stop_commands().await;
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
                    state.start_repositories().await?;
                    state.start_commands(self.clone());
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
            state.start_repositories().await?;
            state.start_commands(self.clone());
        }
        Ok(())
    }

    async fn begin(&self) -> Result<(), RepositoryWatchRuntimeError> {
        {
            let mut state = self.state.lock().await;
            if matches!(state.workers, WorkerState::Running) {
                return Ok(());
            }
            state.workers = WorkerState::Running;
            if !state.paused {
                state.listener.resume().await;
                state.listener.start();
                state.start_repositories().await?;
                state.start_commands(self.clone());
                let sweep = state.prepared_sweep.take();
                state.start_sweep(sweep);
            }
        }
        Ok(())
    }

    /// Starts idle supervision before recovery so resumed workers precede terminal receipts.
    pub async fn spawn(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<Result<(), RepositoryWatchRuntimeError>> {
        let begun = self.begin().await;
        tokio::spawn(async move {
            begun?;
            self.run(shutdown).await
        })
    }

    async fn command_tick(&self) -> Result<(), RepositoryWatchRuntimeError> {
        let prepared = {
            let state = self.state.lock().await;
            if state.paused {
                return Ok(());
            }
            state
                .configuration
                .as_ref()
                .filter(|configuration| configuration.enabled())
                .map(|_| {
                    (
                        state.store.clone(),
                        state.lifecycle.clone(),
                        RepositoryWatchCommandFactory(state.factory.0.clone()),
                    )
                })
        };
        if let Some((store, source, mut factory)) = prepared {
            store
                .drain_lifecycle(&mut factory, &mut RepositoryWatchCommandCodec, &source)
                .await
                .map_err(|_| RepositoryWatchRuntimeError::Lifecycle)?;
        }
        self.state.lock().await.tick().await
    }

    async fn run_commands(self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                result = self.command_tick() => {
                    if let Err(error) = result { tracing::warn!(?error, "repository-watch command attempt failed"); }
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(crate::process_runtime::OUTBOX_IDLE_POLL_INTERVAL) => {}
            }
        }
    }

    /// Supervises configured workers and checkout cleanup while disabled.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        self.begin().await?;
        let outcome = loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            let (active, changed) = {
                let state = self.state.lock().await;
                (!state.paused, state.changed.clone())
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
                result = async { self.state.lock().await.health() } => {
                    if let Err(error) = result { break Err(error); }
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
        outcome
    }
}

impl RuntimeState {
    async fn pause(&mut self) {
        self.paused = true;
        self.stop_commands().await;
        if let Some((shutdown, task)) = self.sweep.take() {
            let _ = shutdown.send(true);
            let _ = task.await;
        }
        self.listener.pause().await;
        self.stop_repositories().await;
    }

    async fn stop_commands(&mut self) {
        if let Some((shutdown, task)) = self.commands.take() {
            let _ = shutdown.send(true);
            let _ = task.await;
        }
    }

    fn start_commands(&mut self, runtime: RepositoryWatchRuntime) {
        let (shutdown, receiver) = watch::channel(false);
        self.commands = Some((shutdown, tokio::spawn(runtime.run_commands(receiver))));
    }

    fn health(&mut self) -> Result<(), RepositoryWatchRuntimeError> {
        if self.listener.failed() {
            return Err(RepositoryWatchRuntimeError::Listener);
        }
        if self.repositories.try_join_next().is_some() {
            return Err(RepositoryWatchRuntimeError::RepositoryWorker);
        }
        if self
            .commands
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            return Err(RepositoryWatchRuntimeError::Dispatch);
        }
        if self
            .sweep
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            return Err(RepositoryWatchRuntimeError::Sweep);
        }
        Ok(())
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
        let observers = self
            .observers
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for observer in observers {
            drop(observer.lock().await);
        }
        self.observers.lock().await.clear();
    }

    async fn start_repositories(&mut self) -> Result<(), RepositoryWatchRuntimeError> {
        let (shutdown, _) = watch::channel(false);
        self.repository_shutdown = shutdown;
        if let Some(configuration) = self
            .configuration
            .as_ref()
            .filter(|configuration| configuration.enabled())
        {
            for repository in configuration.repositories() {
                self.store
                    .prepare_poll_cache(repository.repository(), configuration.signal_reviewers())
                    .await
                    .map_err(|_| RepositoryWatchRuntimeError::RepositoryWorker)?;
            }
            for repository in configuration.repositories() {
                let Some(wake) = self.wakes.get(repository.repository()).cloned() else {
                    continue;
                };
                wake.notify_one();
                let task = GitHubRepositoryTask {
                    repository: repository.repository().clone(),
                    signal_reviewers: configuration.signal_reviewers().to_vec(),
                    subject_retention: configuration.webhook_retention(),
                    poll_request_budget: configuration.poll_request_budget(),
                    clients: RepositoryWatchClientLoader::new(repository),
                    store: self.store.clone(),
                };
                if let Some(service) = self.workflow_service.get().cloned() {
                    let production = if configuration.workflows_enabled() {
                        self.observers.lock().await.insert(
                            repository.repository().clone(),
                            Arc::new(Mutex::new(observation::ConfiguredObserver {
                                task,
                                shutdown: self.repository_shutdown.subscribe(),
                            })),
                        );
                        None
                    } else {
                        Some(task)
                    };
                    self.repositories.spawn(run_repository_task(
                        observation::WorkflowRepositoryTask {
                            repository: repository.repository().clone(),
                            store: self.store.clone(),
                            core: self.core_pool.clone(),
                            service,
                            registration: None,
                            production,
                            current: None,
                        },
                        repository.poll_interval(),
                        wake,
                        self.repository_shutdown.subscribe(),
                    ));
                } else {
                    if configuration.workflows_enabled() {
                        return Err(RepositoryWatchRuntimeError::RepositoryWorker);
                    }
                    self.repositories.spawn(run_repository_task(
                        task,
                        repository.poll_interval(),
                        wake,
                        self.repository_shutdown.subscribe(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn tick(&mut self) -> Result<(), RepositoryWatchRuntimeError> {
        if self.paused {
            return Ok(());
        }
        crate::repo_watch_dispatch::scavenge_checkouts(&self.store, &self.sink.pool)
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Dispatch)?;
        let Some(configuration) = self
            .configuration
            .as_ref()
            .filter(|configuration| configuration.enabled())
        else {
            return Ok(());
        };
        let mut codec = RepositoryWatchCommandCodec;
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
                self.store
                    .retry_due(
                        repository.repository(),
                        rule,
                        &mut RepositoryWatchDispatchIds,
                        &mut self.factory,
                        &mut codec,
                        &self.lifecycle,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptySweep;

    impl signalbox_application::EligibilitySweep for EmptySweep {
        type Error = std::convert::Infallible;

        async fn find_sessions(
            &mut self,
        ) -> Result<signalbox_application::EligibilitySweepBatch, Self::Error> {
            Ok(signalbox_application::EligibilitySweepBatch::new(
                Vec::new(),
                false,
            ))
        }
    }

    #[tokio::test]
    async fn restored_sessions_survive_full_capacity_before_the_scheduler_starts() {
        use signalbox_application::{EligibilityNudge, EligibilityWorkSource};
        use signalbox_domain::SessionId;
        use std::{num::NonZeroUsize, time::Duration};

        const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
        let filler = SessionId::from_uuid(uuid::Uuid::from_u128(0x58_300));
        let restored = SessionId::from_uuid(uuid::Uuid::from_u128(0x58_301));
        let next_restored = SessionId::from_uuid(uuid::Uuid::from_u128(0x58_302));
        let (nudge, mut source) =
            signalbox_application::InProcessEligibilityWorkSource::with_options(
                EmptySweep,
                None,
                NonZeroUsize::new(1),
            );
        assert_eq!(nudge.nudge(filler), EligibilityNudgeOutcome::Enqueued);
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        pool.close().await;
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let runtime = RepositoryWatchRuntime::unstarted(
            pool.clone(),
            RepositoryWatchServices {
                goal_resumption: crate::PostgresGoalPassDisposition::new(
                    pool.clone(),
                    models.clone(),
                    nudge.clone(),
                    crate::GoalModeNumericBounds::new(None, None, None, None, None),
                ),
                core_pool: pool,
                checkout_runner: None,
                models: Arc::new(models),
                templates: Arc::new(SessionTemplateConfiguration::default()),
                eligibility_nudge: nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
        );
        tokio::time::timeout(
            DELIVERY_TIMEOUT,
            runtime.nudge_restored(vec![restored, next_restored]),
        )
        .await
        .expect("recovery returns before scheduler consumption starts");
        assert_eq!(
            tokio::time::timeout(DELIVERY_TIMEOUT, source.next()).await,
            Ok(Ok(filler))
        );
        assert_eq!(
            tokio::time::timeout(DELIVERY_TIMEOUT, source.next()).await,
            Ok(Ok(restored))
        );
        assert_eq!(
            tokio::time::timeout(DELIVERY_TIMEOUT, source.next()).await,
            Ok(Ok(next_restored))
        );
    }

    #[tokio::test]
    async fn lifecycle_database_wait_keeps_runtime_control_available() {
        use std::time::Duration;
        // A silent local endpoint holds the drain at its first database read.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let pool = PgPoolOptions::new()
            .connect_lazy(&format!(
                "postgres://unused:unused@{}/unused",
                listener.local_addr().expect("local address")
            ))
            .expect("lazy pool");
        let base = crate::configuration::checked_in_example_configuration().expect("models");
        let credential = tempfile::NamedTempFile::new().expect("fixture credential path");
        let models = crate::HubModelConfiguration::parse(&format!(
            "{}\n[repository_watch]\nversion = 1\nenabled = true\nsignal_reviewers = []\n[[repository_watch.repositories]]\nrepository = \"fixture/project\"\npoll_interval_seconds = 60\ncredential_file = \"{}\"\n",
            base.source(), credential.path().display()
        ))
        .expect("enabled watch fixture");
        let (nudge, _work) = signalbox_application::InProcessEligibilityWorkSource::new(
            signalbox_persistence::scheduler::PostgresEligibilitySweep::new(pool.clone()),
        );
        let runtime = RepositoryWatchRuntime::unstarted(
            pool.clone(),
            RepositoryWatchServices {
                goal_resumption: crate::PostgresGoalPassDisposition::new(
                    pool.clone(),
                    models.clone(),
                    nudge.clone(),
                    crate::GoalModeNumericBounds::new(None, None, None, None, None),
                ),
                core_pool: pool,
                checkout_runner: None,
                models: Arc::new(models.clone()),
                templates: Arc::new(SessionTemplateConfiguration::default()),
                eligibility_nudge: nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
        );
        {
            let mut state = runtime.state.lock().await;
            state.configuration = models.repository_watch().cloned();
            state.paused = false;
        }
        let worker_runtime = runtime.clone();
        let worker = tokio::spawn(async move { worker_runtime.command_tick().await });
        let (connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("drain reaches its database read")
            .expect("accept connection");
        let available = tokio::time::timeout(Duration::from_secs(5), runtime.state.lock()).await;
        worker.abort();
        let _ = worker.await;
        drop(connection);
        assert!(
            available.is_ok(),
            "session reads and reload control remain available during lifecycle IO"
        );
    }

    #[tokio::test]
    async fn session_origin_read_does_not_wait_for_dispatch_processing() {
        use signalbox_domain::SessionId;
        use signalbox_module_repo_watch_v2::StoreError;
        use std::time::Duration;

        // A closed pool gives a definitive read result without a database fixture.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        pool.close().await;
        let (eligibility_nudge, _work) = signalbox_application::InProcessEligibilityWorkSource::new(
            signalbox_persistence::scheduler::PostgresEligibilitySweep::new(pool.clone()),
        );
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let runtime = RepositoryWatchRuntime::unstarted(
            pool.clone(),
            RepositoryWatchServices {
                goal_resumption: crate::PostgresGoalPassDisposition::new(
                    pool.clone(),
                    models.clone(),
                    eligibility_nudge.clone(),
                    crate::GoalModeNumericBounds::new(None, None, None, None, None),
                ),
                checkout_runner: None,
                core_pool: pool.clone(),
                models: Arc::new(models),
                templates: Arc::new(SessionTemplateConfiguration::default()),
                eligibility_nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
        );
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(0x58_400));
        let _dispatch_processing = runtime.state.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.session_origin(session, &pool),
        )
        .await
        .expect(
            "the descriptor read must reach its database without waiting for dispatch processing",
        );
        assert!(matches!(
            result,
            Err(StoreError::Database(sqlx::Error::PoolClosed))
        ));
    }

    #[tokio::test]
    async fn health_rejects_a_finished_convergence_sweep() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .expect("lazy pool");
        let (eligibility_nudge, _work) = signalbox_application::InProcessEligibilityWorkSource::new(
            signalbox_persistence::scheduler::PostgresEligibilitySweep::new(pool.clone()),
        );
        let models = crate::configuration::checked_in_example_configuration().expect("models");
        let runtime = RepositoryWatchRuntime::unstarted(
            pool.clone(),
            RepositoryWatchServices {
                goal_resumption: crate::PostgresGoalPassDisposition::new(
                    pool.clone(),
                    models.clone(),
                    eligibility_nudge.clone(),
                    crate::GoalModeNumericBounds::new(None, None, None, None, None),
                ),
                checkout_runner: None,
                core_pool: pool,
                models: Arc::new(models),
                templates: Arc::new(SessionTemplateConfiguration::default()),
                eligibility_nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
        );
        let (shutdown, _receiver) = watch::channel(false);
        let mut task = tokio::spawn(async {});
        (&mut task).await.expect("sweep exits unexpectedly");
        let mut state = runtime.state.lock().await;
        state.sweep = Some((shutdown, task));
        assert_eq!(state.health(), Err(RepositoryWatchRuntimeError::Sweep));
    }
}
