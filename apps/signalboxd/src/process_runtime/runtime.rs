use super::*;

/// The hub-owned local protocol runtime: one outbox dispatcher, one bounded
/// durable and streaming fan-outs, and one guarded Unix listener.
#[derive(Debug)]
pub struct ProcessRuntime {
    workflows: Option<crate::workflows::WorkflowService>,
    configuration_reload: Option<crate::configuration_reload::ConfigurationReload>,
    recovery_reporter: Option<FatalRecoveryReporter>,
    oauth_service: Option<Arc<crate::OauthCredentialService>>,
    listener: LocalProcessListener,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    goal_resumption: Option<PostgresGoalPassDisposition>,
    model_configuration: HubModelConfiguration,
    context_compaction_model: Arc<dyn ContextCompactionModel>,
    template_configuration: SessionTemplateConfiguration,
    fanouts: ProcessFanouts,
    metrics: Option<TelemetryMetrics>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
}

#[derive(Clone, Debug)]
pub(super) struct ProcessFanouts {
    pub(super) durable: broadcast::Sender<ProcessUpdate>,
    pub(super) streaming: broadcast::Sender<ProcessUpdate>,
    pub(super) monitor: broadcast::Sender<ProcessMonitorUpdate>,
    pub(super) runner_recovery: watch::Sender<()>,
}

impl ProcessRuntime {
    /// Shares admission with the daemon-owned workflow runner.
    pub fn with_workflows(mut self, workflows: crate::workflows::WorkflowService) -> Self {
        self.workflows = Some(workflows);
        self
    }

    /// Shares model dispatch's OAuth cache with credential administration.
    pub fn with_oauth_service(mut self, service: Arc<crate::OauthCredentialService>) -> Self {
        self.oauth_service = Some(service);
        self
    }

    /// Composes the guarded listener, fenced database, nudge, and static models.
    pub fn new(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
        model_configuration: HubModelConfiguration,
    ) -> Self {
        Self::new_with_templates(
            listener,
            pool,
            eligibility_nudge,
            tool_dispatch_gate,
            model_configuration,
            SessionTemplateConfiguration::default(),
        )
    }

    /// Composes the guarded runtime with startup-resolved session templates.
    pub fn new_with_templates(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
        model_configuration: HubModelConfiguration,
        template_configuration: SessionTemplateConfiguration,
    ) -> Self {
        let snapshot_reader_budget = shared_snapshot_reader_budget(
            pool.options().get_max_connections(),
            Some(&model_configuration),
        );
        let (durable_updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let (streaming_updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let (monitor_updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let (runner_recovery, _) = watch::channel(());
        Self {
            workflows: None,
            configuration_reload: None,
            recovery_reporter: None,
            oauth_service: None,
            listener,
            pool,
            eligibility_nudge,
            tool_dispatch_gate,
            goal_resumption: None,
            model_configuration,
            context_compaction_model: Arc::new(UnavailableContextCompactionModel),
            template_configuration,
            metrics: None,
            blob_store_registry: None,
            snapshot_reader_budget,
            fanouts: ProcessFanouts {
                durable: durable_updates,
                streaming: streaming_updates,
                monitor: monitor_updates,
                runner_recovery,
            },
        }
    }

    /// Shares the daemon's serial configuration reload and atomic catalog holder.
    pub fn with_configuration_reload(
        mut self,
        reload: crate::configuration_reload::ConfigurationReload,
    ) -> Self {
        self.configuration_reload = Some(reload);
        self
    }

    /// Wires the goal-mode disposition that arms automatic resumption when an adopt
    /// takes a blocked goal.
    #[must_use]
    pub fn with_goal_resumption(mut self, disposition: PostgresGoalPassDisposition) -> Self {
        self.goal_resumption = Some(disposition);
        self
    }

    /// Returns the nonblocking sink that places already-redacted provider text
    /// on this runtime incarnation's ordered follow fan-out.
    /// Installs the dedicated summary-call adapter used by explicit and automatic compaction.
    pub fn with_context_compaction_model(
        mut self,
        model: impl ContextCompactionModel + 'static,
    ) -> Self {
        self.context_compaction_model = Arc::new(model);
        self
    }

    /// Installs the handle raising the daemon's fatal recovery signal.
    ///
    /// A connection handler has no execution role, so without this a durable
    /// outcome it cannot decide would end at the client response and nothing
    /// would stop the process for the next incarnation's startup scan.
    #[must_use]
    pub fn with_recovery_reporter(mut self, reporter: FatalRecoveryReporter) -> Self {
        self.recovery_reporter = Some(reporter);
        self
    }

    /// Installs the private Prometheus counters fed by durable outbox events.
    #[must_use]
    pub fn with_metrics(mut self, metrics: TelemetryMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Installs the startup-authenticated immutable-blob registry.
    #[must_use]
    pub fn with_blob_store_registry(mut self, registry: Arc<BlobStoreRegistry>) -> Self {
        self.blob_store_registry = Some(registry);
        self
    }

    /// Installs the daemon-wide admission budget shared with browser snapshots.
    #[must_use]
    pub fn with_snapshot_reader_budget(mut self, budget: Arc<Semaphore>) -> Self {
        self.snapshot_reader_budget = Some(budget);
        self
    }

    pub fn provider_text_delta_sink(&self) -> ProcessProviderTextDeltaSink {
        ProcessProviderTextDeltaSink {
            updates: self.fanouts.streaming.clone(),
            monitor: self.fanouts.monitor.clone(),
        }
    }

    /// Returns the daemon's one bounded browser monitor source.
    pub fn monitor(&self) -> ProcessMonitor {
        ProcessMonitor {
            updates: self.fanouts.monitor.clone(),
        }
    }

    /// Shares committed runner-authority wakeups with continuation boundary waiters.
    pub fn runner_recovery_notifications(&self) -> watch::Receiver<()> {
        self.fanouts.runner_recovery.subscribe()
    }

    /// Serves requests and dispatches durable updates until `shutdown` changes
    /// to true or its sender closes.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<(), ProcessRuntimeError> {
        let mut recovery_listener = sqlx::postgres::PgListener::connect_with(&self.pool)
            .await
            .map_err(ProcessRuntimeError::DatabaseNotifications)?;
        recovery_listener
            .listen_all(["runner_recovery", "credential_wait_changed"])
            .await
            .map_err(ProcessRuntimeError::DatabaseNotifications)?;
        let oauth = signalbox_persistence::oauth_credential::OauthCredentialRepository::new(
            self.pool.clone(),
        );
        oauth
            .abandon_pending()
            .await
            .map_err(ProcessRuntimeError::OauthRecovery)?;
        oauth
            .replace_registrations(&self.model_configuration.oauth_registrations())
            .await
            .map_err(ProcessRuntimeError::OauthRecovery)?;
        let fanouts = self.fanouts;
        let recovery_store = signalbox_persistence::runner_protocol::RunnerProtocolStore::new(
            self.pool.clone(),
            crate::runner_protocol_runtime::registration_only_catalog().map_err(|error| {
                ProcessRuntimeError::RunnerRecoveryCommands(
                    signalbox_persistence::runner_protocol::RunnerProtocolStoreError::Domain(error)
                        .into(),
                )
            })?,
        );
        resume_runner_replacements_and_notify(&recovery_store, &fanouts.runner_recovery).await?;
        let recovery_notifications = forward_database_notifications(
            recovery_listener,
            recovery_store,
            fanouts.runner_recovery.clone(),
            fanouts.streaming.clone(),
            self.eligibility_nudge.clone(),
            shutdown.clone(),
        );
        let connection_dependencies = ConnectionDependencies {
            workflows: self.workflows,
            configuration_reload: self.configuration_reload,
            recovery_reporter: self.recovery_reporter,
            oauth_service: self.oauth_service,
            pool: self.pool.clone(),
            eligibility_nudge: self.eligibility_nudge.clone(),
            tool_dispatch_gate: self.tool_dispatch_gate,
            goal_resumption: self.goal_resumption.clone(),
            model_configuration: self.model_configuration,
            context_compaction_model: self.context_compaction_model,
            template_configuration: self.template_configuration,
            fanouts: fanouts.clone(),
            blob_store_registry: self.blob_store_registry,
            snapshot_reader_budget: self.snapshot_reader_budget,
        };
        let server = serve_connections(&self.listener, connection_dependencies, shutdown.clone());
        let dispatcher = dispatch_updates(
            self.pool,
            self.eligibility_nudge,
            fanouts,
            self.metrics,
            shutdown,
        );
        let result = tokio::try_join!(server, dispatcher, recovery_notifications);
        let cleanup = self.listener.cleanup();

        result?;
        cleanup.map_err(ProcessRuntimeError::CleanupSocket)
    }
}

async fn resume_runner_replacements_and_notify(
    store: &signalbox_persistence::runner_protocol::RunnerProtocolStore,
    notifications: &watch::Sender<()>,
) -> Result<(), ProcessRuntimeError> {
    store
        .resume_runner_replacements()
        .await
        .map_err(ProcessRuntimeError::RunnerRecoveryCommands)?;
    notifications.send_replace(());
    Ok(())
}

async fn forward_database_notifications(
    mut listener: sqlx::postgres::PgListener,
    store: signalbox_persistence::runner_protocol::RunnerProtocolStore,
    notifications: watch::Sender<()>,
    credential_wait_updates: broadcast::Sender<ProcessUpdate>,
    eligibility_nudge: InProcessEligibilityNudge,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let notification = tokio::select! {
            notification = listener.try_recv() => {
                notification.map_err(ProcessRuntimeError::DatabaseNotifications)?
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                continue;
            }
        };
        if let Some(notification) = notification {
            if notification.channel() == "credential_wait_changed" {
                let session = uuid::Uuid::parse_str(notification.payload())
                    .ok()
                    .map(SessionId::from_uuid);
                if let Some(session) = session {
                    eligibility_nudge.nudge(session);
                }
                let _ = credential_wait_updates.send(ProcessUpdate::ResyncRequired { session });
                continue;
            }
        } else {
            let _ = credential_wait_updates.send(ProcessUpdate::ResyncRequired { session: None });
        }
        resume_runner_replacements_and_notify(&store, &notifications).await?;
    }
}

/// Daemon-owned nonblocking bridge from provider observations to follow streams.
#[derive(Clone, Debug)]
pub struct ProcessProviderTextDeltaSink {
    updates: broadcast::Sender<ProcessUpdate>,
    pub(super) monitor: broadcast::Sender<ProcessMonitorUpdate>,
}

impl ProviderTextDeltaSink for ProcessProviderTextDeltaSink {
    fn publish(&self, delta: ProviderTextDelta) {
        let monitor = ProcessMonitorUpdate::ProviderTextDelta {
            session: delta.session(),
            turn: delta.turn(),
            call: delta.call(),
            part_index: delta.part_index(),
            text: delta.shared_text(),
        };
        let _ = self.updates.send(ProcessUpdate::ProviderTextDelta(delta));
        let _ = self.monitor.send(monitor);
    }
}

pub(super) async fn dispatch_updates(
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    fanouts: ProcessFanouts,
    metrics: Option<TelemetryMetrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let dispatcher = OutboxDispatcher::new(pool);
    let mut last_metric_sequence = None;
    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let outcome = dispatcher
            .dispatch_next(|event| {
                observe_outbox_metrics_once(
                    metrics.as_ref(),
                    &mut last_metric_sequence,
                    event.sequence(),
                    event.kind(),
                );
                // A sessionless receipt has no follower to reach.
                if let Some(session) = event.session() {
                    let outcome =
                        nudge_eligible_outbox_wake(&eligibility_nudge, session, event.kind());
                    if outcome
                        == Some(signalbox_application::EligibilityNudgeOutcome::DroppedAtCapacity)
                        && matches!(
                            event.kind(),
                            DispatchedOutboxEventKind::RunnerStateTransition { .. }
                        )
                    {
                        let nudge = eligibility_nudge.clone();
                        tokio::spawn(
                            async move { nudge.nudge_waiting_for_capacity(session).await },
                        );
                    }
                    let _ = fanouts.monitor.send(ProcessMonitorUpdate::Durable {
                        cursor: event.sequence(),
                        session,
                        kind: monitor_event_kind(event.kind()),
                    });
                    if let Some(update) = ProcessUpdate::from_outbox(event) {
                        let _ = fanouts.durable.send(update.clone());
                        let _ = fanouts.streaming.send(update);
                    }
                }
                OutboxDeliveryDecision::Delivered
            })
            .await;
        match outcome {
            Ok(OutboxDispatchOutcome::Delivered { .. }) => {}
            Ok(OutboxDispatchOutcome::Idle)
            | Err(OutboxDispatchError::Database(sqlx::Error::PoolTimedOut)) => {
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    () = sleep(OUTBOX_IDLE_POLL_INTERVAL) => {}
                }
            }
            Ok(OutboxDispatchOutcome::Retry { .. }) => {
                return Err(ProcessRuntimeError::UnexpectedDispatcherRetry);
            }
            Err(error) => return Err(ProcessRuntimeError::Dispatch(error)),
        }
    }
}

/// Cloneable source for the daemon's one bounded browser monitor fan-out.
#[derive(Clone, Debug)]
pub struct ProcessMonitor {
    updates: broadcast::Sender<ProcessMonitorUpdate>,
}

impl ProcessMonitor {
    pub fn subscribe(&self) -> ProcessMonitorSubscription {
        ProcessMonitorSubscription {
            receiver: self.updates.subscribe(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> Self {
        let (updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        Self { updates }
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, update: ProcessMonitorUpdate) {
        let _ = self.updates.send(update);
    }

    #[cfg(test)]
    pub(crate) fn fill_for_test(&self, update: ProcessMonitorUpdate) {
        for _ in 0..=PROCESS_UPDATE_CAPACITY {
            let _ = self.updates.send(update.clone());
        }
    }
}

/// One monitor subscriber; lag is explicit and requires resynchronization.
#[derive(Debug)]
pub struct ProcessMonitorSubscription {
    receiver: broadcast::Receiver<ProcessMonitorUpdate>,
}

impl ProcessMonitorSubscription {
    pub fn queued_len(&self) -> usize {
        self.receiver.len()
    }

    /// True when this subscriber's unread queue has reached the bounded
    /// fan-out capacity, so the next broadcast drops its oldest unread record.
    pub fn is_saturated(&self) -> bool {
        self.receiver.len() >= PROCESS_UPDATE_CAPACITY
    }

    pub async fn recv(&mut self) -> Result<ProcessMonitorUpdate, ProcessMonitorReceiveError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(skipped) => {
                ProcessMonitorReceiveError::Lagged(usize::try_from(skipped).unwrap_or(usize::MAX))
            }
            broadcast::error::RecvError::Closed => ProcessMonitorReceiveError::Closed,
        })
    }
}

/// Current-runtime update exposed to browser HTTP without process frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessMonitorUpdate {
    Durable {
        cursor: u64,
        session: SessionId,
        kind: SessionTimelineEventKind,
    },
    ProviderTextDelta {
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        part_index: u32,
        text: Arc<str>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessMonitorReceiveError {
    Lagged(usize),
    Closed,
}

fn monitor_event_kind(event: &DispatchedOutboxEventKind) -> SessionTimelineEventKind {
    match event {
        DispatchedOutboxEventKind::CredentialPoolExhausted(_) => {
            SessionTimelineEventKind::TurnFailed
        }
        DispatchedOutboxEventKind::SessionCreated(_) => SessionTimelineEventKind::SessionCreated,
        DispatchedOutboxEventKind::SessionStateChanged(_) => {
            SessionTimelineEventKind::SessionStateChanged
        }
        DispatchedOutboxEventKind::SessionTerminal(_) => SessionTimelineEventKind::SessionTerminal,
        DispatchedOutboxEventKind::GoalChanged(_) => SessionTimelineEventKind::GoalChanged,
        DispatchedOutboxEventKind::CommandSettled { .. } => {
            SessionTimelineEventKind::CommandSettled
        }
        DispatchedOutboxEventKind::InjectionSettled { .. } => {
            SessionTimelineEventKind::InjectionSettled
        }
        DispatchedOutboxEventKind::SessionOwnershipChanged(_) => {
            SessionTimelineEventKind::SessionOwnershipChanged
        }
        DispatchedOutboxEventKind::SessionModelSettingsChanged(_) => {
            SessionTimelineEventKind::SessionModelSettingsChanged
        }
        DispatchedOutboxEventKind::TurnModelSettingsResolved(_) => {
            SessionTimelineEventKind::TurnModelSettingsResolved
        }
        DispatchedOutboxEventKind::InputAccepted { .. } => SessionTimelineEventKind::InputAccepted,
        DispatchedOutboxEventKind::TurnActivated { .. } => SessionTimelineEventKind::TurnActivated,
        DispatchedOutboxEventKind::TurnTerminal { disposition, .. } => match disposition {
            DispatchedTurnTerminalDisposition::Completed { .. } => {
                SessionTimelineEventKind::TurnCompleted
            }
            DispatchedTurnTerminalDisposition::Refused { .. } => {
                SessionTimelineEventKind::TurnRefused
            }
            DispatchedTurnTerminalDisposition::Failed { .. } => {
                SessionTimelineEventKind::TurnFailed
            }
            DispatchedTurnTerminalDisposition::Cancelled { .. } => {
                SessionTimelineEventKind::TurnCancelled
            }
            DispatchedTurnTerminalDisposition::ReconciliationRequired { .. } => {
                SessionTimelineEventKind::TurnReconciliationRequired
            }
            DispatchedTurnTerminalDisposition::Retired => SessionTimelineEventKind::GoalTurnRetired,
        },
        DispatchedOutboxEventKind::ModelCallTransition { .. } => {
            SessionTimelineEventKind::ModelCallTransition
        }
        DispatchedOutboxEventKind::ToolBatchTransition { .. } => {
            SessionTimelineEventKind::ToolBatchTransition
        }
        DispatchedOutboxEventKind::ToolApprovalDecided { .. } => {
            SessionTimelineEventKind::ToolApprovalDecided
        }
        DispatchedOutboxEventKind::ContextCompacted { .. } => {
            SessionTimelineEventKind::ContextCompacted
        }
        DispatchedOutboxEventKind::RunnerStateTransition { .. } => {
            SessionTimelineEventKind::RunnerStateTransition
        }
        DispatchedOutboxEventKind::DelegationUpdate(_) => {
            SessionTimelineEventKind::DelegationUpdate
        }
        DispatchedOutboxEventKind::DelegationWake(_) => SessionTimelineEventKind::DelegationWake,
    }
}

pub(super) fn nudge_eligible_outbox_wake(
    eligibility_nudge: &impl EligibilityNudge,
    session: SessionId,
    event: &DispatchedOutboxEventKind,
) -> Option<signalbox_application::EligibilityNudgeOutcome> {
    if matches!(
        event,
        DispatchedOutboxEventKind::DelegationWake(_)
            | DispatchedOutboxEventKind::RunnerStateTransition {
                state: DispatchedRunnerState::Replaced
                    | DispatchedRunnerState::WorkingDirectoryChanged
                    | DispatchedRunnerState::Abandoned,
                ..
            }
    ) {
        Some(eligibility_nudge.nudge(session))
    } else {
        None
    }
}

pub(super) fn nudge_delegation_issuer(
    eligibility_nudge: &impl EligibilityNudge,
    session: SessionId,
) {
    let _ = eligibility_nudge.nudge(session);
}

pub(super) fn observe_outbox_metrics_once(
    metrics: Option<&TelemetryMetrics>,
    last_sequence: &mut Option<u64>,
    sequence: u64,
    event: &DispatchedOutboxEventKind,
) {
    if *last_sequence == Some(sequence) {
        return;
    }
    *last_sequence = Some(sequence);
    observe_outbox_metrics(metrics, event);
}

fn observe_outbox_metrics(metrics: Option<&TelemetryMetrics>, event: &DispatchedOutboxEventKind) {
    let Some(metrics) = metrics else {
        return;
    };
    match event {
        DispatchedOutboxEventKind::TurnActivated { .. } => metrics.observe_turn_started(),
        DispatchedOutboxEventKind::TurnTerminal { disposition, .. } => match disposition {
            DispatchedTurnTerminalDisposition::Completed { .. } => {
                metrics.observe_turn_terminal(TurnMetricOutcome::Completed);
            }
            DispatchedTurnTerminalDisposition::Failed { .. } => {
                metrics.observe_turn_terminal(TurnMetricOutcome::Failed);
            }
            DispatchedTurnTerminalDisposition::Refused { .. } => {
                metrics.observe_turn_terminal(TurnMetricOutcome::Refused);
            }
            DispatchedTurnTerminalDisposition::Cancelled { .. } => {
                metrics.observe_turn_terminal(TurnMetricOutcome::Cancelled);
            }
            DispatchedTurnTerminalDisposition::ReconciliationRequired { .. } => {
                metrics.observe_turn_terminal(TurnMetricOutcome::ReconciliationRequired);
            }
            // A retired turn never ran, so it is not a turn outcome.
            DispatchedTurnTerminalDisposition::Retired => {}
        },
        DispatchedOutboxEventKind::ModelCallTransition { state, .. } => {
            observe_model_call_metrics(metrics, *state);
        }
        DispatchedOutboxEventKind::CredentialPoolExhausted(_)
        | DispatchedOutboxEventKind::SessionCreated(_)
        | DispatchedOutboxEventKind::SessionStateChanged(_)
        | DispatchedOutboxEventKind::SessionTerminal(_)
        | DispatchedOutboxEventKind::GoalChanged(_)
        | DispatchedOutboxEventKind::CommandSettled { .. }
        | DispatchedOutboxEventKind::InjectionSettled { .. }
        | DispatchedOutboxEventKind::SessionOwnershipChanged(_)
        | DispatchedOutboxEventKind::SessionModelSettingsChanged(_)
        | DispatchedOutboxEventKind::TurnModelSettingsResolved(_)
        | DispatchedOutboxEventKind::InputAccepted { .. }
        | DispatchedOutboxEventKind::ToolBatchTransition { .. }
        | DispatchedOutboxEventKind::RunnerStateTransition { .. }
        | DispatchedOutboxEventKind::ContextCompacted { .. }
        | DispatchedOutboxEventKind::DelegationUpdate(_)
        | DispatchedOutboxEventKind::ToolApprovalDecided { .. }
        | DispatchedOutboxEventKind::DelegationWake(_) => {}
    }
}

fn observe_model_call_metrics(metrics: &TelemetryMetrics, state: DispatchedModelCallState) {
    let disposition = match state {
        DispatchedModelCallState::Terminal(disposition) => disposition,
        DispatchedModelCallState::Prepared
        | DispatchedModelCallState::InFlight
        | DispatchedModelCallState::CancellationRequested => return,
    };
    let disposition = match disposition {
        DispatchedModelCallDisposition::Completed => ModelMetricDisposition::Completed,
        DispatchedModelCallDisposition::KnownFailed => ModelMetricDisposition::KnownFailed,
        DispatchedModelCallDisposition::Refused => ModelMetricDisposition::Refused,
        DispatchedModelCallDisposition::Cancelled => ModelMetricDisposition::Cancelled,
        DispatchedModelCallDisposition::Ambiguous => ModelMetricDisposition::Ambiguous,
    };
    metrics.observe_model_terminal(disposition);
}

#[cfg(test)]
mod runner_recovery_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn recovery_waiters_share_one_listener_and_leave_pool_capacity_for_progress()
    -> Result<(), Box<dyn Error>> {
        const POOL_CONNECTIONS: u32 = 2;
        const PENDING_REPLAYS: usize = 128;
        const COMPLETION_DEADLINE: Duration = Duration::from_secs(10);
        let (_container, pool, _database_url) =
            signalbox_persistence::test_support::postgres::migrated_postgres(POOL_CONNECTIONS)
                .await?;
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;

        listener.listen("runner_recovery").await?;
        let (notifications, _) = watch::channel(());
        let mut waiters: Vec<_> = (0..PENDING_REPLAYS)
            .map(|_| notifications.subscribe())
            .collect();
        let (shutdown, receiver) = watch::channel(false);
        let store = signalbox_persistence::runner_protocol::RunnerProtocolStore::new(
            pool.clone(),
            crate::runner_protocol_runtime::registration_only_catalog()
                .expect("registration catalog is valid"),
        );
        resume_runner_replacements_and_notify(&store, &notifications).await?;
        let (updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let (nudge, _source) = signalbox_application::InProcessEligibilityWorkSource::new(
            signalbox_persistence::scheduler::PostgresEligibilitySweep::new(pool.clone()),
        );
        let forwarder = tokio::spawn(forward_database_notifications(
            listener,
            store,
            notifications,
            updates,
            nudge,
            receiver,
        ));
        for waiter in &mut waiters {
            tokio::time::timeout(COMPLETION_DEADLINE, waiter.changed()).await??;
        }
        tokio::time::timeout(
            COMPLETION_DEADLINE,
            sqlx::query("SELECT pg_notify('runner_recovery', '')").execute(&pool),
        )
        .await??;
        for waiter in &mut waiters {
            tokio::time::timeout(COMPLETION_DEADLINE, waiter.changed()).await??;
        }
        let probe: i32 = tokio::time::timeout(
            COMPLETION_DEADLINE,
            sqlx::query_scalar("SELECT 1").fetch_one(&pool),
        )
        .await??;
        assert_eq!(probe, 1);
        shutdown.send(true)?;
        tokio::time::timeout(COMPLETION_DEADLINE, forwarder).await???;
        pool.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod dispatcher_tests {
    use super::*;
    use signalbox_application::InProcessEligibilityWorkSource;
    use signalbox_persistence::scheduler::PostgresEligibilitySweep;
    use sqlx::postgres::PgPoolOptions;
    use tokio::time::timeout;
    use uuid::Uuid;

    fn dispatcher_fanouts() -> ProcessFanouts {
        ProcessFanouts {
            durable: broadcast::channel(8).0,
            streaming: broadcast::channel(8).0,
            monitor: broadcast::channel(8).0,
            runner_recovery: watch::channel(()).0,
        }
    }

    #[tokio::test]
    async fn dispatcher_pool_closure_remains_fatal() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@localhost/fixture")
            .expect("syntactically valid fixture URL");
        pool.close().await;
        let (nudge, _source) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
        let (_shutdown, receiver) = watch::channel(false);
        let result = dispatch_updates(pool, nudge, dispatcher_fanouts(), None, receiver).await;
        assert!(matches!(
            result,
            Err(ProcessRuntimeError::Dispatch(
                OutboxDispatchError::Database(sqlx::Error::PoolClosed)
            ))
        ));
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn dispatcher_delivers_the_pending_event_after_pool_pressure_clears()
    -> Result<(), Box<dyn Error>> {
        use signalbox_domain::{
            CreateSession, SessionCreationCause, SessionCreationProvenance, TranscriptAncestry,
        };
        use signalbox_persistence::{
            SessionCredentialPin, SessionModelCredential, local_test_connection_options,
        };

        const FIXTURE_FAMILY: &str = "fixture-family";
        const FIXTURE_CREDENTIAL: &str = "fixture-primary";
        let (container, pool, url) =
            signalbox_persistence::test_support::postgres::migrated_postgres(1).await?;
        pool.close().await;
        let acquisition_timeout = Duration::from_millis(250);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(acquisition_timeout)
            .connect_with(local_test_connection_options(&url)?)
            .await?;

        let session = SessionId::from_uuid(Uuid::now_v7());
        let command = CreateSession::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::now_v7()),
            )),
        )
        .prepare(session)
        .expect("interactive creation without ancestry is preparable");
        let pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
            FIXTURE_FAMILY,
            FIXTURE_CREDENTIAL,
        )])
        .expect("the fixture names one credential family");
        CreateSessionRepository::new(pool.clone(), pin)
            .handle(command)
            .await?;
        let held = pool.acquire().await?;
        let (nudge, _source) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
        let fanouts = dispatcher_fanouts();
        let mut monitor = fanouts.monitor.subscribe();
        let (shutdown, receiver) = watch::channel(false);
        let mut dispatch = Box::pin(dispatch_updates(
            pool.clone(),
            nudge,
            fanouts,
            None,
            receiver,
        ));
        let pressure_window = acquisition_timeout * 3 + OUTBOX_IDLE_POLL_INTERVAL * 2;
        assert!(
            timeout(pressure_window, dispatch.as_mut()).await.is_err(),
            "pool exhaustion must leave the dispatcher running"
        );
        assert!(matches!(
            monitor.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(held);
        let event = tokio::select! {
            result = dispatch.as_mut() => panic!("dispatcher stopped before delivery: {result:?}"),
            event = timeout(Duration::from_secs(5), monitor.recv()) => event??,
        };
        assert!(
            matches!(event, ProcessMonitorUpdate::Durable { session: delivered, .. } if delivered == session)
        );
        shutdown.send_replace(true);
        timeout(Duration::from_secs(5), dispatch).await??;
        pool.close().await;
        drop(container);
        Ok(())
    }
}
