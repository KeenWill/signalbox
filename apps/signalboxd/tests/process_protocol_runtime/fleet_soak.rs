//! Fleet soak coverage.

use super::*;

// Fleet-soak coverage for issue #1027. Slow/failing tools, boundary loss, an
// unprovisioned workspace, and scheduled goal resumption are named follow-on
// slices: they need the same fleet census but not more boot infrastructure.

pub(crate) struct WitnessedEligibilityPass<Pass> {
    pub(crate) inner: Pass,
    pub(crate) witness: ReconciliationWitness,
}

impl<Pass> WitnessedEligibilityPass<Pass> {
    pub(crate) fn new(inner: Pass, witness: ReconciliationWitness) -> Self {
        Self { inner, witness }
    }
}

impl<Pass> EligibilityPass for WitnessedEligibilityPass<Pass>
where
    Pass: EligibilityPass + Send,
{
    type Error = Pass::Error;

    fn failure_stage(error: &Self::Error) -> &'static str {
        Pass::failure_stage(error)
    }

    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        Pass::failure_turn(error)
    }

    // The decorator must forward the inner pass's occupancy-expiry handoff.
    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        self.inner.occupancy_expiry_handler()
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.inner.run(session);
        let witness = self.witness.clone();
        async move {
            let outcome = execution.await;
            witness.record_processed_session(session);
            outcome
        }
    }
}

// config/signalboxd.example.toml supplies scheduler_pass_admission_cap to this fixture.
pub(crate) const FLEET_PASS_ADMISSION_CAP: usize = 16;
pub(crate) const FLEET_SESSION_COUNT: usize = FLEET_PASS_ADMISSION_CAP;
pub(crate) const FLEET_BASELINE_OCCUPANCY_BOUND: Duration = Duration::from_secs(900);
pub(crate) const FLEET_OCCUPANCY_BOUND: Duration = Duration::from_secs(1);
// Allow the occupancy expiry, detached database recovery, and scheduling under load.
pub(crate) const FLEET_ASSERTION_BOUND: Duration = Duration::from_secs(30);
pub(crate) const FLEET_SETUP_BOUND: Duration = Duration::from_secs(120);

pub(crate) struct FleetPrepared {
    pub(crate) correlation: ModelCallId,
    pub(crate) inner: ScriptedPrepared<ModelCallId>,
}

/// How many scripted executions complete and how many hang.
///
/// The completions are served first: a scenario that stands a healthy baseline
/// fleet up before injecting one fault gets the fault on the last execution.
#[derive(Clone, Copy)]
pub(crate) struct FleetModelCardinality {
    pub(crate) hanging: usize,
    pub(crate) completing: usize,
}

#[derive(Clone)]
pub(crate) struct FleetScriptedModel {
    pub(crate) inner: ScriptedModel<ModelCallId>,
    pub(crate) completions_before_hangs: Arc<AtomicUsize>,
    pub(crate) hangs_remaining: Arc<AtomicUsize>,
    pub(crate) in_flight_hangs: Arc<AtomicUsize>,
    pub(crate) completed_calls: Arc<Mutex<Vec<ModelCallId>>>,
}

impl FleetScriptedModel {
    pub(crate) fn new(cardinality: FleetModelCardinality) -> Self {
        Self {
            inner: ScriptedModel::following(std::iter::repeat_n(
                completed_script(
                    "fixture-model",
                    "fleet session completed",
                    TokenUsage::unreported(),
                ),
                cardinality.hanging + cardinality.completing,
            )),
            completions_before_hangs: Arc::new(AtomicUsize::new(cardinality.completing)),
            hangs_remaining: Arc::new(AtomicUsize::new(cardinality.hanging)),
            in_flight_hangs: Arc::new(AtomicUsize::new(0)),
            completed_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn in_flight_hangs(&self) -> usize {
        self.in_flight_hangs.load(Ordering::SeqCst)
    }

    pub(crate) fn completed_call_ids(&self) -> Vec<ModelCallId> {
        self.completed_calls
            .lock()
            .expect("the fleet completion lock is available")
            .clone()
    }

    pub(crate) fn record_completed_call(&self, correlation: ModelCallId) {
        self.completed_calls
            .lock()
            .expect("the fleet completion lock is available")
            .push(correlation);
    }
}

pub(crate) struct FleetHangGuard(pub(crate) Arc<AtomicUsize>);

impl Drop for FleetHangGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ModelRuntime<ModelCallId> for FleetScriptedModel {
    type Prepared = FleetPrepared;

    async fn prepare(
        &self,
        operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        let correlation = operation.correlation;
        match self.inner.prepare(operation, cancellation).await {
            PreparationOutcome::Prepared(inner) => {
                PreparationOutcome::Prepared(FleetPrepared { correlation, inner })
            }
            PreparationOutcome::Defect {
                correlation,
                defect,
            } => PreparationOutcome::Defect {
                correlation,
                defect,
            },
            PreparationOutcome::Cancelled { correlation } => {
                PreparationOutcome::Cancelled { correlation }
            }
            PreparationOutcome::Failed {
                correlation,
                failure,
            } => PreparationOutcome::Failed {
                correlation,
                failure,
            },
        }
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        let correlation = prepared.correlation;
        let completes = self
            .completions_before_hangs
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if completes {
            let report = self.inner.execute(prepared.inner, sink, cancellation).await;
            self.record_completed_call(correlation);
            return report;
        }
        let hangs = self
            .hangs_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if hangs {
            sink.observe(Observation {
                correlation,
                fact: ObservationFact::SendCommenced,
            });
            self.in_flight_hangs.fetch_add(1, Ordering::SeqCst);
            let _guard = FleetHangGuard(Arc::clone(&self.in_flight_hangs));
            pending::<TerminalReport<ModelCallId>>().await
        } else {
            let report = self.inner.execute(prepared.inner, sink, cancellation).await;
            self.record_completed_call(correlation);
            report
        }
    }
}

#[derive(Debug)]
pub(crate) struct CommissionedFleet {
    requested_session_count: usize,
    pub(crate) sessions: Vec<CanonicalUuid>,
}

impl CommissionedFleet {
    fn requested_session_count(&self) -> usize {
        self.requested_session_count
    }
}

pub(crate) async fn commission_fleet(
    runtime: &RunningRuntime,
    first_index: usize,
    session_count: usize,
) -> Result<CommissionedFleet, Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    let mut sessions = Vec::with_capacity(session_count);
    for offset in 0..session_count {
        let index = first_index + offset;
        connection
            .request(
                u64::try_from(index + 2)?,
                ClientRequest::CommissionSession {
                    command_id: command()?,
                    template_name: String::from("merge-forward"),
                    fence: CommissionedSessionFence::Branch {
                        repository: String::from("sample-user/sample-repository"),
                        branch: format!("agent/fleet-soak-{index}"),
                    },
                    statement: format!("complete fleet soak session {index}"),
                    content: InputContent::new(String::from("return the scripted reply")),
                },
            )
            .await?;
        let response = response_within(&mut connection).await?.message().clone();
        let ServerMessage::SessionCommissioned { session_id, .. } = response else {
            panic!("fleet commission returned {response:?}");
        };
        sessions.push(session_id);
    }
    Ok(CommissionedFleet {
        sessions,
        requested_session_count: session_count,
    })
}

pub(crate) struct FleetRuntimeTasks {
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) scheduler: JoinHandle<SchedulerLoopExit>,
    pub(crate) turn_liveness: JoinHandle<()>,
}

impl FleetRuntimeTasks {
    pub(crate) async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send_replace(true);
        let scheduler_exit = timeout(RUNTIME_SETTLE_ALLOWANCE, self.scheduler).await??;
        timeout(RUNTIME_SETTLE_ALLOWANCE, self.turn_liveness).await??;
        if scheduler_exit != SchedulerLoopExit::Shutdown {
            return Err(io::Error::other("fleet scheduler returned a non-shutdown exit").into());
        }
        Ok(())
    }

    pub(crate) async fn kill(self) -> Result<(), Box<dyn Error>> {
        self.scheduler.abort();
        self.turn_liveness.abort();
        let scheduler = self.scheduler.await;
        let turn_liveness = self.turn_liveness.await;
        let scheduler = scheduler.expect_err("the killed scheduler task must not return normally");
        let turn_liveness =
            turn_liveness.expect_err("the killed turn-liveness task must not return normally");
        assert!(
            scheduler.is_cancelled(),
            "the scheduler task must stop by cancellation, got {scheduler}"
        );
        assert!(
            turn_liveness.is_cancelled(),
            "the turn-liveness task must stop by cancellation, got {turn_liveness}"
        );
        Ok(())
    }
}

pub(crate) async fn wait_for_fleet_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Commissions one extra session after a restart, as a readiness control that
/// proves the replacement scheduler is admitting fresh work.
pub(crate) async fn commission_fleet_control(
    runtime: &RunningRuntime,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence: CommissionedSessionFence::Branch {
                    repository: String::from("sample-user/sample-repository"),
                    branch: String::from("agent/fleet-soak-control"),
                },
                statement: String::from("complete the fleet scheduler readiness control"),
                content: InputContent::new(String::from("return the scripted reply")),
            },
        )
        .await?;
    let response = response_within(&mut connection).await?.message().clone();
    let ServerMessage::SessionCommissioned { session_id, .. } = response else {
        return Err(
            io::Error::other(format!("fleet control commission returned {response:?}")).into(),
        );
    };
    Ok(session_id)
}

pub(crate) fn start_fleet_scheduler(
    runtime: &mut RunningRuntime,
    model: FleetScriptedModel,
    occupancy_bound: SchedulerPassOccupancyBound,
) -> Result<FleetRuntimeTasks, Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let bounds = configuration.numeric_bounds();
    let expired_pass_recovery_policy = ExpiredPassRecoveryPolicy::new(
        bounds
            .integer("expired_pass_recovery_attempts")
            .flatten()
            .and_then(|value| u32::try_from(value).ok()),
        bounds
            .duration("expired_pass_recovery_attempt_bound")
            .flatten(),
        bounds
            .duration("expired_pass_recovery_lock_retry_delay")
            .flatten(),
        bounds
            .duration("expired_pass_recovery_conservative_retry_delay")
            .flatten(),
    );
    let turn_liveness_persistence_bounds = TurnLivenessPersistenceBounds::new(
        bounds.duration("terminalization_lock_wait").flatten(),
        bounds.duration("terminalization_acquire_wait").flatten(),
        bounds.duration("terminalization_write_lock_wait").flatten(),
    );
    let turn_liveness_numeric_bounds = TurnLivenessNumericBounds::new(
        bounds
            .integer("terminalizations_per_liveness_scan")
            .flatten()
            .and_then(|value| usize::try_from(value).ok()),
        bounds
            .duration("turn_liveness_recovery_attempt_bound")
            .flatten(),
        bounds
            .integer("automatic_reconciliations_per_liveness_scan")
            .flatten()
            .and_then(|value| usize::try_from(value).ok()),
        bounds
            .duration("automatic_reconciliation_attempt_bound")
            .flatten(),
        turn_liveness_persistence_bounds,
    );
    let stale_active_turn_bound = bounds
        .duration("stale_active_turn_bound")
        .flatten()
        .map(StaleActiveTurnBound::try_new)
        .transpose()?;
    let turn_liveness_scan_interval = bounds
        .duration("turn_liveness_scan_interval")
        .flatten()
        .map(TurnLivenessScanInterval::try_new)
        .transpose()?;
    let automatic_reconciliation_attempt_budget = bounds
        .integer("automatic_reconciliation_attempt_budget")
        .flatten()
        .and_then(|value| u32::try_from(value).ok());
    let automatic_reconciliation_base_backoff = bounds
        .duration("automatic_reconciliation_base_backoff")
        .flatten();
    let automatic_reconciliation_backoff_cap = bounds
        .duration("automatic_reconciliation_backoff_cap")
        .flatten();
    let provider =
        RuntimeModelCallProvider::new(model, configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    configuration.target_catalog(),
                    ModelCallCredentialReference::new("fleet-soak-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        ),
        execution,
    )
    .with_occupancy_recovery(
        runtime.pool.clone(),
        runtime.eligibility_nudge.clone(),
        expired_pass_recovery_policy,
        turn_liveness_persistence_bounds,
    );
    let pass = WitnessedEligibilityPass::new(pass, runtime.reconciliation_witness());
    let mut scheduler =
        SchedulerLoop::new(runtime.take_work_source(), pass).with_occupancy_bound(occupancy_bound);
    let turn_liveness = TurnLivenessRuntime::new(
        runtime.pool.clone(),
        stale_active_turn_bound,
        turn_liveness_scan_interval,
        automatic_reconciliation_attempt_budget,
        automatic_reconciliation_base_backoff,
        automatic_reconciliation_backoff_cap,
        turn_liveness_numeric_bounds,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let scheduler_shutdown = shutdown_receiver.clone();
    let scheduler = tokio::spawn(async move {
        scheduler
            .run_until(async move {
                tokio::select! {
                    () = fatal_execution.wait() => {}
                    () = wait_for_fleet_shutdown(scheduler_shutdown) => {}
                }
            })
            .await
    });
    let turn_liveness = tokio::spawn(turn_liveness.run(shutdown_receiver));
    Ok(FleetRuntimeTasks {
        shutdown,
        scheduler,
        turn_liveness,
    })
}

pub(crate) async fn wait_for_hangs(
    model: &FleetScriptedModel,
    expected: usize,
) -> Result<(), Box<dyn Error>> {
    let observed = timeout(FLEET_SETUP_BOUND, async {
        while model.in_flight_hangs() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if observed.is_err() {
        return Err(io::Error::other(format!(
            "fleet hang setup expected {expected} in flight, observed {}",
            model.in_flight_hangs()
        ))
        .into());
    }
    Ok(())
}

/// Waits for one drained eligibility cycle, so a replacement scheduler's
/// reconciliation pass is observed as completed rather than slept for.
pub(crate) async fn wait_for_reconciliation(
    witness: &ReconciliationWitness,
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        while witness.completed_cycles() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

/// Tears the scheduler and turn-liveness tasks down from whatever state they
/// are in, so a panicking scenario still releases the fixture.
pub(crate) async fn abort_fleet_scheduler(tasks: FleetRuntimeTasks) -> Result<(), Box<dyn Error>> {
    let FleetRuntimeTasks {
        shutdown,
        scheduler,
        turn_liveness,
    } = tasks;
    shutdown.send_replace(true);
    scheduler.abort();
    turn_liveness.abort();
    let stopped = scheduler.await;
    let liveness = turn_liveness.await;
    if !matches!(&stopped, Ok(SchedulerLoopExit::Shutdown))
        && !matches!(&stopped, Err(error) if error.is_cancelled())
    {
        return Err(io::Error::other(format!(
            "the fleet scheduler must stop by cancellation or fatal-driven shutdown: {stopped:?}"
        ))
        .into());
    }
    if !matches!(&liveness, Ok(())) && !matches!(&liveness, Err(error) if error.is_cancelled()) {
        return Err(io::Error::other(format!(
            "the fleet turn-liveness runtime must stop by cancellation or shutdown: {liveness:?}"
        ))
        .into());
    }
    Ok(())
}

pub(crate) async fn wait_for_completed_calls(
    model: &FleetScriptedModel,
    expected: usize,
) -> Result<Vec<ModelCallId>, Box<dyn Error>> {
    Ok(timeout(FLEET_SETUP_BOUND, async {
        loop {
            let completed = model.completed_call_ids();
            if completed.len() == expected {
                return completed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?)
}

pub(crate) async fn wait_for_model_call_for_session(
    repository: &FleetSoakCensusRepository,
    session: CanonicalUuid,
) -> Result<ModelCallId, Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if let Some(model_call) = repository
                .model_call_id_for_session(SessionId::from_uuid(session.into_uuid()))
                .await?
            {
                return Ok::<ModelCallId, Box<dyn Error>>(model_call);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

pub(crate) async fn wait_for_completed_call(
    model: &FleetScriptedModel,
    expected: ModelCallId,
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        while !model.completed_call_ids().contains(&expected) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

pub(crate) async fn wait_for_terminal_calls(
    repository: &FleetSoakCensusRepository,
    model_calls: &[ModelCallId],
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if repository
                .census_for(model_calls)
                .await?
                .terminal_model_calls()
                == i64::try_from(model_calls.len())?
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

pub(crate) async fn wait_for_terminal_turns(
    repository: &FleetSoakCensusRepository,
    model_calls: &[ModelCallId],
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if repository.census_for(model_calls).await?.terminal_turns()
                == i64::try_from(model_calls.len())?
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

/// Waits for exactly `hung_model_call` to reach its typed ambiguity park with
/// its execution released, rather than counting parks across the database.
pub(crate) async fn wait_for_ambiguity_park(
    repository: &FleetSoakCensusRepository,
    model: &FleetScriptedModel,
    hung_model_call: ModelCallId,
    bound: Duration,
) -> Result<(), Box<dyn Error>> {
    timeout(bound, async {
        loop {
            if repository
                .has_ambiguous_recovery_park(hung_model_call)
                .await?
                && model.in_flight_hangs() == 0
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| io::Error::other("fleet model call did not reach its ambiguity park"))??;
    Ok(())
}

pub(crate) fn assert_hung_fleet_outcome(
    model: &FleetScriptedModel,
    census: FleetSoakCensus,
    hung_call_has_ambiguity_park: bool,
) -> Result<(), Box<dyn Error>> {
    let active = census.active_turns();
    let terminal = census.terminal_turns();
    let typed_terminal_calls = census.terminal_model_calls();
    if model.in_flight_hangs() != 0
        || active != 1
        || terminal != i64::try_from(FLEET_SESSION_COUNT - 1)?
        || typed_terminal_calls != i64::try_from(FLEET_SESSION_COUNT)?
        || census.awaiting_model_call_recovery_turns() != 1
        || census.ambiguous_model_calls() != 1
        || !hung_call_has_ambiguity_park
    {
        return Err(io::Error::other(format!(
            "fleet liveness failed: hangs={}, active={active}, terminal={terminal}, typed_terminal_calls={typed_terminal_calls}, recovery_parks={}, ambiguous_calls={}, hung_call_has_ambiguity_park={hung_call_has_ambiguity_park}",
            model.in_flight_hangs(),
            census.awaiting_model_call_recovery_turns(),
            census.ambiguous_model_calls()
        ))
        .into());
    }
    Ok(())
}

pub(crate) fn assert_restarted_fleet_outcome(
    census: FleetSoakCensus,
    original_model: &FleetScriptedModel,
    replacement_model: &FleetScriptedModel,
) -> Result<(), Box<dyn Error>> {
    if census.active_turns() != 0
        || census.terminal_turns() != i64::try_from(FLEET_SESSION_COUNT)?
        || census.awaiting_model_call_recovery_turns() != 0
        || census.terminal_model_calls() != i64::try_from(FLEET_SESSION_COUNT)?
        || original_model.in_flight_hangs() != 0
        || replacement_model.in_flight_hangs() != 0
    {
        return Err(io::Error::other(format!(
            "restart must release every original execution and reconcile every ambiguous operation into a terminal turn without a user decision: census={census:?}, original_hangs={}, replacement_hangs={}",
            original_model.in_flight_hangs(),
            replacement_model.in_flight_hangs()
        ))
        .into());
    }
    Ok(())
}

/// Issue #1027: a post-acceptance model hang releases its authoritative pass
/// and reaches a durable typed ambiguity park inside the occupancy bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn fleet_soak_hung_model_call_has_bounded_pass_occupancy_and_typed_disposition()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut tasks: Option<FleetRuntimeTasks> = None;
    let scenario = AssertUnwindSafe(async {
        let census_repository = FleetSoakCensusRepository::new(runtime.pool.clone());
        let baseline_fleet = commission_fleet(&runtime, 0, FLEET_SESSION_COUNT - 1).await?;
        let model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: 1,
            completing: FLEET_SESSION_COUNT - 1,
        });
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        let completed_calls = wait_for_completed_calls(&model, FLEET_SESSION_COUNT - 1).await?;
        wait_for_terminal_calls(&census_repository, &completed_calls).await?;
        wait_for_terminal_turns(&census_repository, &completed_calls).await?;
        tasks
            .take()
            .expect("the baseline fleet scheduler was installed")
            .stop()
            .await?;
        runtime.restart().await?;
        let fault_fleet = commission_fleet(&runtime, FLEET_SESSION_COUNT - 1, 1).await?;
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_OCCUPANCY_BOUND)?,
        )?);
        wait_for_hangs(&model, 1).await?;
        let model_calls = census_repository.model_call_ids().await?;
        assert_eq!(
            baseline_fleet.sessions.len(),
            baseline_fleet.requested_session_count(),
            "baseline fleet session cardinality mismatch"
        );
        assert_eq!(
            fault_fleet.sessions.len(),
            fault_fleet.requested_session_count(),
            "fault fleet session cardinality mismatch"
        );
        assert_eq!(
            model_calls.len(),
            FLEET_SESSION_COUNT,
            "fleet model-call cardinality mismatch"
        );
        let hung_model_calls = model_calls
            .iter()
            .copied()
            .filter(|model_call| !completed_calls.contains(model_call))
            .collect::<Vec<_>>();
        let [hung_model_call] = hung_model_calls.as_slice() else {
            return Err(io::Error::other(format!(
                "expected one hung model call, observed {hung_model_calls:?}"
            ))
            .into());
        };
        wait_for_ambiguity_park(
            &census_repository,
            &model,
            *hung_model_call,
            FLEET_ASSERTION_BOUND,
        )
        .await?;
        let census = census_repository.census_for(&model_calls).await?;
        let hung_call_has_ambiguity_park = census_repository
            .has_ambiguous_recovery_park(*hung_model_call)
            .await?;
        assert_hung_fleet_outcome(&model, census, hung_call_has_ambiguity_park)
    })
    .catch_unwind()
    .await;

    let scheduler_cleanup = match tasks {
        Some(tasks) => abort_fleet_scheduler(tasks).await,
        None => Ok(()),
    };
    let runtime_cleanup = runtime.stop().await;
    match scenario {
        Ok(outcome) => {
            scheduler_cleanup?;
            runtime_cleanup?;
            outcome
        }
        Err(panic) => {
            if let Err(error) = scheduler_cleanup {
                eprintln!("fleet scheduler cleanup after panic failed: {error}");
            }
            if let Err(error) = runtime_cleanup {
                eprintln!("fleet runtime cleanup after panic failed: {error}");
            }
            resume_unwind(panic)
        }
    }
}

/// Issue #1027: killing the daemon with a full fleet in model
/// execution leaves every model call ambiguous. Ambiguous-operation
/// reconciliation must release local scheduler ownership and then resume or
/// terminalize every such turn once a replacement daemon takes over, without
/// waiting on a user decision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn fleet_soak_kill_restart_resumes_or_terminalizes_every_active_turn()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut tasks: Option<FleetRuntimeTasks> = None;
    let scenario = AssertUnwindSafe(async {
        let census_repository = FleetSoakCensusRepository::new(runtime.pool.clone());
        let fleet = commission_fleet(&runtime, 0, FLEET_SESSION_COUNT).await?;
        let hanging_model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: FLEET_SESSION_COUNT,
            completing: 0,
        });
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            hanging_model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        wait_for_hangs(&hanging_model, FLEET_SESSION_COUNT).await?;
        let pre_kill_model_call_ids = census_repository.model_call_ids().await?;
        assert_eq!(
            pre_kill_model_call_ids.len(),
            FLEET_SESSION_COUNT,
            "pre-kill model-call cardinality mismatch"
        );
        tasks
            .take()
            .expect("the first fleet scheduler was installed")
            .kill()
            .await?;
        wait_for_hangs(&hanging_model, 0).await?;
        let _recovered = runtime.kill_and_restart().await?;
        // One script per recoverable turn plus the readiness control, so the
        // fixture does not decide whether reconciliation reissues a call.
        let replacement_model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: 0,
            completing: FLEET_SESSION_COUNT + 1,
        });
        let replacement_reconciliation = runtime.reconciliation_witness();
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            replacement_model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        wait_for_reconciliation(&replacement_reconciliation).await?;
        let control_session = commission_fleet_control(&runtime).await?;
        let control_model_call =
            wait_for_model_call_for_session(&census_repository, control_session).await?;
        wait_for_completed_call(&replacement_model, control_model_call).await?;
        wait_for_terminal_turns(&census_repository, &pre_kill_model_call_ids).await?;
        let census = census_repository
            .census_for(&pre_kill_model_call_ids)
            .await?;
        assert_eq!(
            fleet.sessions.len(),
            FLEET_SESSION_COUNT,
            "fleet session cardinality mismatch"
        );
        assert_restarted_fleet_outcome(census, &hanging_model, &replacement_model)
    })
    .catch_unwind()
    .await;

    let scheduler_cleanup = match tasks {
        Some(tasks) => abort_fleet_scheduler(tasks).await,
        None => Ok(()),
    };
    let runtime_cleanup = runtime.stop().await;
    match scenario {
        Ok(outcome) => {
            scheduler_cleanup?;
            runtime_cleanup?;
            outcome
        }
        Err(panic) => {
            if let Err(error) = scheduler_cleanup {
                eprintln!("fleet scheduler cleanup after panic failed: {error}");
            }
            if let Err(error) = runtime_cleanup {
                eprintln!("fleet runtime cleanup after panic failed: {error}");
            }
            resume_unwind(panic)
        }
    }
}
