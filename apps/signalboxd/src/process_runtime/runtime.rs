use super::*;

/// The hub-owned local protocol runtime: one outbox dispatcher, one bounded
/// durable and streaming fan-outs, and one guarded Unix listener.
#[derive(Debug)]
pub struct ProcessRuntime {
    recovery_reporter: Option<FatalRecoveryReporter>,
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
}

impl ProcessRuntime {
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
        Self {
            recovery_reporter: None,
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
            },
        }
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

    /// Serves requests and dispatches durable updates until `shutdown` changes
    /// to true or its sender closes.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<(), ProcessRuntimeError> {
        let fanouts = self.fanouts;
        let connection_dependencies = ConnectionDependencies {
            recovery_reporter: self.recovery_reporter,
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
        let result = tokio::try_join!(server, dispatcher);
        let cleanup = self.listener.cleanup();

        result?;
        cleanup.map_err(ProcessRuntimeError::CleanupSocket)
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
                    nudge_delegation_wake(&eligibility_nudge, session, event.kind());
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
            .await
            .map_err(ProcessRuntimeError::Dispatch)?;
        match outcome {
            OutboxDispatchOutcome::Delivered { .. } => {}
            OutboxDispatchOutcome::Idle => {
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    () = sleep(OUTBOX_IDLE_POLL_INTERVAL) => {}
                }
            }
            OutboxDispatchOutcome::Retry { .. } => {
                return Err(ProcessRuntimeError::UnexpectedDispatcherRetry);
            }
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

pub(super) fn nudge_delegation_wake(
    eligibility_nudge: &impl EligibilityNudge,
    session: SessionId,
    event: &DispatchedOutboxEventKind,
) {
    if matches!(event, DispatchedOutboxEventKind::DelegationWake(_)) {
        let _ = eligibility_nudge.nudge(session);
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
        DispatchedOutboxEventKind::SessionCreated(_)
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
