//! Connects invocation capacity to supervised process lifetimes.
use signalbox_application::{EligibilityNudge, InProcessEligibilityNudge};
use signalbox_domain::{ModelCallId, SessionId, TurnId};
use signalbox_model_provider_runtime::InvocationProcessObserver;
use signalbox_persistence::{credential_invocations, model_execution::ModelCallRepositoryError};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::sync::watch;

mod capacity;
mod process_identity;
pub use capacity::CodexCapacityRefresh;

// Process-group absence is polled once per second until shutdown.
const PROCESS_GROUP_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct CredentialInvocationProcesses {
    pool: sqlx::PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    observed: Arc<Mutex<BTreeMap<ModelCallId, ObservedInvocation>>>,
    pending_titles: Arc<Mutex<BTreeMap<SessionId, TurnId>>>,
    title_tasks: Option<tokio::sync::mpsc::Sender<crate::web_http::SessionTitleTask>>,
}

enum ObservedInvocation {
    Process(u32, String),
    AbandonedTitle,
}

impl CredentialInvocationProcesses {
    pub fn new(pool: sqlx::PgPool, eligibility_nudge: InProcessEligibilityNudge) -> Self {
        Self {
            pool,
            eligibility_nudge,
            observed: Arc::default(),
            pending_titles: Arc::default(),
            title_tasks: None,
        }
    }

    /// Submits automatic title work to the daemon incarnation's runtime task set.
    pub fn with_session_title_tasks(
        mut self,
        tasks: tokio::sync::mpsc::Sender<crate::web_http::SessionTitleTask>,
    ) -> Self {
        self.title_tasks = Some(tasks);
        self
    }

    pub(crate) async fn submit_title(&self, task: crate::web_http::SessionTitleTask) -> bool {
        match &self.title_tasks {
            Some(tasks) => tasks.send(task).await.is_ok(),
            None => false,
        }
    }

    pub async fn run(
        &self,
        mut shutdown: watch::Receiver<bool>,
        configuration: crate::configuration_reload::ConfigurationReload,
    ) {
        if *shutdown.borrow() {
            return;
        }
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(PROCESS_GROUP_RECHECK_INTERVAL) => {
                    if let Err(error) = self.recover().await {
                        tracing::error!(%error, "invocation reservation reconciliation failed");
                        continue;
                    }
                    if let Some(titles) = configuration.session_titles(self.pool.clone()) {
                        for prepared in self.prepare_pending_titles(&titles).await {
                            titles.submit_initial(prepared).await;
                        }
                    }
                }
            }
        }
    }

    pub async fn recover(&self) -> Result<(), ModelCallRepositoryError> {
        let abandoned_titles = self
            .observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|(call, observation)| {
                matches!(observation, ObservedInvocation::AbandonedTitle).then_some(*call)
            })
            .collect::<Vec<_>>();
        for call in abandoned_titles {
            self.finish_registered_title(call).await?;
        }
        credential_invocations::release_unregistered_terminal_calls(&self.pool).await?;
        for (call, group, start_time) in
            credential_invocations::active_processes(&self.pool).await?
        {
            if group_absent(group, &start_time) {
                credential_invocations::release(&self.pool, call).await?;
            }
        }
        self.nudge_eligible_waits().await?;
        Ok(())
    }

    pub(crate) fn retain_initial_title(&self, session: SessionId, turn: TurnId) {
        self.pending_titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session)
            .or_insert(turn);
    }

    async fn prepare_pending_titles(
        &self,
        titles: &crate::session_titles::SessionTitles,
    ) -> Vec<crate::session_titles::PreparedTitle> {
        let pending = self
            .pending_titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(session, turn)| (*session, *turn))
            .collect::<Vec<_>>();
        let mut ready = Vec::new();
        for (session, turn) in pending {
            match titles.prepare(session, Some(turn)).await {
                Ok(prepared) => {
                    self.pending_titles
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&session);
                    ready.extend(prepared);
                }
                Err(
                    crate::session_titles::TitleError::Unavailable
                    | crate::session_titles::TitleError::Database,
                ) => {}
                Err(error) => {
                    self.pending_titles
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&session);
                    tracing::warn!(session_id = %session.into_uuid(), ?error, "initial session title recovery failed");
                }
            }
        }
        ready
    }

    pub(crate) async fn abandon_title(&self, call: ModelCallId) -> Result<(), sqlx::Error> {
        self.observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call, ObservedInvocation::AbandonedTitle);
        self.finish_registered_title(call).await
    }

    async fn finish_registered_title(&self, call: ModelCallId) -> Result<(), sqlx::Error> {
        let result =
            signalbox_persistence::session_titles::SessionTitleRepository::new(self.pool.clone())
                .abandon(call)
                .await;
        match result {
            Ok(()) | Err(sqlx::Error::RowNotFound) => {
                self.observed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&call);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn nudge_eligible_waits(&self) -> Result<(), ModelCallRepositoryError> {
        let sessions: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT waiting.session_id FROM credential_availability_wait waiting
             JOIN turn_lifecycle active ON active.turn_id = waiting.turn_id AND active.session_id = waiting.session_id
             WHERE active.state_kind = 'active' AND NOT active.delegation_runtime_terminal
               AND goal_turn_is_runtime_relevant(active.session_id, active.turn_id)
               AND credential_wait_is_eligible(waiting.wait_attempt_id)",
        )
        .fetch_all(&self.pool)
        .await?;
        for session in sessions {
            self.eligibility_nudge.nudge(SessionId::from_uuid(session));
        }
        Ok(())
    }
}

fn group_absent(group: u32, start_time: &str) -> bool {
    #[cfg(unix)]
    {
        let absent = rustix::process::Pid::from_raw(group as i32).is_some_and(|group| {
            rustix::process::test_kill_process_group(group) == Err(rustix::io::Errno::SRCH)
        });
        absent
            || process_identity::start_time(group)
                .ok()
                .flatten()
                .is_some_and(|current| current != start_time)
    }
    #[cfg(not(unix))]
    {
        let _ = (group, start_time);
        false
    }
}

impl InvocationProcessObserver for CredentialInvocationProcesses {
    fn register(
        &self,
        call: ModelCallId,
        group: u32,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let start_time = process_identity::start_time(group).ok().flatten();
        if let Some(start_time) = &start_time {
            self.observed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(call, ObservedInvocation::Process(group, start_time.clone()));
        }
        Box::pin(async move {
            let Some(start_time) = start_time else {
                return false;
            };
            credential_invocations::register_process(&self.pool, call, group, &start_time)
                .await
                .is_ok()
        })
    }
    fn finished(
        &self,
        call: ModelCallId,
        _process_group: Option<u32>,
        proven_unsent: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&call);
        Box::pin(async move {
            let result = async {
                let group = match observed {
                    Some(ObservedInvocation::Process(group, start_time)) => {
                        Some((group, start_time))
                    }
                    Some(ObservedInvocation::AbandonedTitle) | None => {
                        credential_invocations::process_group(&self.pool, call).await?
                    }
                };
                if group
                    .as_ref()
                    .is_some_and(|(group, start_time)| group_absent(*group, start_time))
                    || (group.is_none() && proven_unsent)
                {
                    credential_invocations::release(&self.pool, call).await?;
                    self.nudge_eligible_waits().await?;
                } else if let Some((group, start_time)) = group {
                    credential_invocations::register_process(&self.pool, call, group, &start_time)
                        .await?;
                }
                Ok::<_, ModelCallRepositoryError>(())
            }
            .await;
            if let Err(error) = result {
                tracing::error!(%error, "invocation reservation release failed");
            }
        })
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use signalbox_domain::{
        CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
        ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
        SessionCreationCause, SessionCreationProvenance, TranscriptAncestry,
    };
    use signalbox_persistence::{
        create_session::CreateSessionRepository,
        scheduler::PostgresEligibilitySweep,
        session_titles::{PrepareSessionTitleOutcome, SessionTitleCall, SessionTitleRepository},
    };

    #[tokio::test]
    async fn runtime_abort_drains_automatic_title_work() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with(sqlx::postgres::PgConnectOptions::new());
        let (nudge, _source) = signalbox_application::InProcessEligibilityWorkSource::new(
            PostgresEligibilitySweep::new(pool.clone()),
        );
        let (tasks, mut pending) = tokio::sync::mpsc::channel(1);
        let processes =
            CredentialInvocationProcesses::new(pool, nudge).with_session_title_tasks(tasks);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (held, mut ended) = tokio::sync::oneshot::channel::<()>();
        assert!(
            processes
                .submit_title(Box::pin(async move {
                    started.send(()).expect("fixture observes execution");
                    std::future::pending::<()>().await;
                    drop(held);
                }))
                .await
        );
        let mut runtime_tasks = tokio::task::JoinSet::new();
        runtime_tasks.spawn(pending.recv().await.expect("runtime receives title task"));
        ready.await.expect("generation started");
        assert!(matches!(
            ended.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        runtime_tasks.abort_all();
        while runtime_tasks.join_next().await.is_some() {}
        assert!(
            ended.await.is_err(),
            "runtime drain drops automatic generation"
        );
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn contended_initial_title_is_restored_after_restart_without_another_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_application::{
            InProcessAttemptDispatchGate, StartEligibleTurnOutcome, StartEligibleTurnService,
            SubmitInputRequest, SubmitInputService,
        };
        use signalbox_domain::{
            AssistantText, DeliveryRequest, ModelSelectionOverride, PerInputConfigurationChoices,
            SessionConfigurationDefaultsVersion, UserContent,
        };
        use signalbox_persistence::{
            model_execution::PostgresModelCallRepository,
            start_eligible_turn::StartEligibleTurnRepository, submit_input::SubmitInputRepository,
        };
        let (_database, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(6).await?;
        let models = Arc::new(crate::HubModelConfiguration::parse(&format!(
            "{}\n[session_titles]\nselection_id = \"10000000-0000-4000-8000-000000000001\"\n",
            crate::configuration::tests::CONFIGURATION,
        ))?);
        let session = SessionId::from_uuid(uuid::Uuid::now_v7());
        let selection =
            DirectModelSelection::from_uuid(uuid::uuid!("10000000-0000-4000-8000-000000000001"));
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )
        .prepare(session)
        .map_err(|_| "session creation rejected")?;
        CreateSessionRepository::new(pool.clone(), models.session_credential_pin())
            .handle(creation)
            .await?;
        let (nudge, _source) = signalbox_application::InProcessEligibilityWorkSource::new(
            PostgresEligibilitySweep::new(pool.clone()),
        );
        SubmitInputService::new(
            signalbox_application::UuidV7SubmitInputIdGenerator,
            SubmitInputRepository::new(pool.clone()),
            nudge.clone(),
            signalbox_application::InProcessToolDispatchGate::default(),
        )
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            session,
            UserContent::try_text("Describe database indexes".to_owned()).expect("fixture input"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?;
        let StartEligibleTurnOutcome::Activated(activated) = StartEligibleTurnService::new(
            signalbox_application::UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        )
        .execute(session)
        .await?
        else {
            panic!("fixture turn activates")
        };
        let turn = activated.turn();
        crate::workspace_instruction_runtime::WorkspaceInstructionRuntime::new(
            pool.clone(),
            None,
            Vec::new(),
        )
        .prepare(session, turn)
        .await?;
        let route = models
            .resolve_direct_model(selection)
            .expect("fixture route");
        let profile = route.credential_profile().to_owned();
        crate::PostgresScriptedModelExecution::new(
            PostgresModelCallRepository::new(
                pool.clone(),
                models.target_catalog(),
                signalbox_application::ModelCallCredentialReference::new(&profile),
            ),
            InProcessAttemptDispatchGate::default(),
            AssistantText::try_new("Database indexes accelerate queries".to_owned())
                .expect("fixture reply"),
        )
        .execute_all(activated)
        .await?;
        credential_invocations::replace_registrations(
            &pool,
            &[(profile.clone(), std::num::NonZeroU32::new(1))],
        )
        .await?;
        let read_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout = '1s'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(pool.connect_options().as_ref().clone())
            .await?;
        let read_processes = CredentialInvocationProcesses::new(pool.clone(), nudge.clone());
        let read_titles = crate::session_titles::SessionTitles::new(
            read_pool.clone(),
            models.clone(),
            crate::model_catalog_runtime::ModelRuntimeFactory::new(None, None, None),
            read_processes,
        );
        let mut held_read = pool.begin().await?;
        sqlx::query("LOCK TABLE accepted_input_content_part IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *held_read)
            .await?;
        assert!(matches!(
            read_titles.prepare(session, Some(turn)).await,
            Err(crate::session_titles::TitleError::Database)
        ));
        held_read.rollback().await?;
        let abandoned: (bool, bool) = sqlx::query_as(
            "SELECT title.abandoned, reservation.released_at IS NOT NULL FROM session_title_model_call title
             JOIN credential_invocation_reservation reservation USING (model_call_id) WHERE title.session_id = $1",
        ).bind(session.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(abandoned, (true, true));
        read_pool.close().await;
        let repository = SessionTitleRepository::new(pool.clone());
        let mut occupying = SessionTitleCall {
            call: ModelCallId::from_uuid(uuid::Uuid::now_v7()),
            session,
            selection,
            target: route.target(),
            credential_reference: profile,
            input_includes_cache_tokens: false,
            initial_for_turn: None,
        };
        assert_eq!(
            repository
                .prepare(&mut occupying, &models.credential_pool_runtime_catalog())
                .await?,
            PrepareSessionTitleOutcome::Prepared
        );
        let processes = CredentialInvocationProcesses::new(pool.clone(), nudge.clone());
        let unavailable = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        unavailable.close().await;
        let failed_titles = crate::session_titles::SessionTitles::new(
            unavailable,
            models.clone(),
            crate::model_catalog_runtime::ModelRuntimeFactory::new(None, None, None),
            processes.clone(),
        );
        failed_titles.start_initial(session, turn).await;
        assert_eq!(
            *processes.pending_titles.lock().expect("pending titles"),
            BTreeMap::from([(session, turn)]),
            "a pre-claim database failure retains completed-turn work"
        );
        let titles = crate::session_titles::SessionTitles::new(
            pool.clone(),
            models.clone(),
            crate::model_catalog_runtime::ModelRuntimeFactory::new(None, None, None),
            processes.clone(),
        );
        assert!(matches!(
            titles.prepare(session, Some(turn)).await,
            Err(crate::session_titles::TitleError::Unavailable)
        ));
        titles.start_initial(session, turn).await;
        assert!(processes.prepare_pending_titles(&titles).await.is_empty());
        assert_eq!(
            processes
                .pending_titles
                .lock()
                .expect("pending titles")
                .len(),
            1
        );
        drop(titles);
        drop(processes);
        let processes = CredentialInvocationProcesses::new(pool.clone(), nudge);
        let titles = crate::session_titles::SessionTitles::new(
            pool.clone(),
            models.clone(),
            crate::model_catalog_runtime::ModelRuntimeFactory::new(None, None, None),
            processes.clone(),
        );
        assert!(processes.prepare_pending_titles(&titles).await.is_empty());
        titles.restore_pending().await?;
        titles.restore_pending().await?;
        assert_eq!(
            processes
                .pending_titles
                .lock()
                .expect("restored titles")
                .len(),
            1
        );
        assert!(processes.prepare_pending_titles(&titles).await.is_empty());
        let replacement_target = uuid::uuid!("20000000-0000-4000-8000-000000000003");
        let replacement = Arc::new(crate::HubModelConfiguration::parse(
            &models.source().replace(
                &route.target().identity().into_uuid().to_string(),
                &replacement_target.to_string(),
            ),
        )?);
        let titles = crate::session_titles::SessionTitles::new(
            pool.clone(),
            replacement,
            crate::model_catalog_runtime::ModelRuntimeFactory::new(None, None, None),
            processes.clone(),
        );
        repository.abandon(occupying.call).await?;
        let ready = processes.prepare_pending_titles(&titles).await;
        assert_eq!(ready.len(), 1);
        titles.restore_pending().await?;
        assert!(processes.prepare_pending_titles(&titles).await.is_empty());
        assert!(repository.unclaimed_initial_turns().await?.is_empty());
        let recovered: uuid::Uuid = sqlx::query_scalar("SELECT model_call_id FROM session_title_model_call WHERE session_id = $1 AND initial_for_turn = $2 AND NOT abandoned")
            .bind(session.into_uuid()).bind(turn.into_uuid()).fetch_one(&pool).await?;
        let recovered_target: uuid::Uuid = sqlx::query_scalar(
            "SELECT resolved_provider_model_identity_id FROM session_title_model_call WHERE model_call_id = $1",
        ).bind(recovered).fetch_one(&pool).await?;
        assert_eq!(recovered_target, replacement_target);
        assert_eq!(
            repository
                .finish_generated(
                    DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
                    ModelCallId::from_uuid(recovered),
                    Some("Database query indexes".to_owned()),
                    signalbox_application::UsageTokenAxes {
                        input: Some(20),
                        output: Some(4),
                        cache_creation_input: None,
                        cache_read_input: None
                    },
                )
                .await?,
            Some("Database query indexes".to_owned())
        );
        let saved: String =
            sqlx::query_scalar("SELECT title FROM session_metadata WHERE session_id = $1")
                .bind(session.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(saved, "Database query indexes");
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn periodic_recovery_retries_failed_unsent_title_cleanup()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_database, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
        let models =
            crate::HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)?;
        let session = SessionId::from_uuid(uuid::Uuid::now_v7());
        let selection =
            DirectModelSelection::from_uuid(uuid::uuid!("10000000-0000-4000-8000-000000000001"));
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )
        .prepare(session)
        .map_err(|_| "test session creation rejected")?;
        CreateSessionRepository::new(pool.clone(), models.session_credential_pin())
            .handle(creation)
            .await?;
        let profile = "codex-title-recovery-fixture";
        credential_invocations::replace_registrations(
            &pool,
            &[(profile.to_owned(), std::num::NonZeroU32::new(1))],
        )
        .await?;
        let titles = SessionTitleRepository::new(pool.clone());
        let (nudge, _source) = signalbox_application::InProcessEligibilityWorkSource::new(
            PostgresEligibilitySweep::new(pool.clone()),
        );
        let mut observer = CredentialInvocationProcesses::new(pool.clone(), nudge);
        let unavailable = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with((*pool.connect_options()).clone());
        unavailable.close().await;

        for (authorize, cleanup_committed) in [(false, false), (true, false), (false, true)] {
            let mut call = SessionTitleCall {
                call: ModelCallId::from_uuid(uuid::Uuid::now_v7()),
                session,
                selection,
                target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    uuid::Uuid::now_v7(),
                )),
                credential_reference: profile.to_owned(),
                input_includes_cache_tokens: false,
                initial_for_turn: None,
            };
            assert!(
                titles.prepare(&mut call, &Default::default()).await?
                    == PrepareSessionTitleOutcome::Prepared
            );
            observer.recover().await?;
            let prepared: bool = sqlx::query_scalar(
                "SELECT state_kind = 'prepared' FROM session_title_model_call WHERE model_call_id = $1",
            )
            .bind(call.call.into_uuid())
            .fetch_one(&pool)
            .await?;
            assert!(prepared, "recovery leaves active preparations alone");
            if authorize {
                titles.authorize(call.call).await?;
            }
            observer.pool = unavailable.clone();
            assert!(matches!(
                observer.abandon_title(call.call).await,
                Err(sqlx::Error::PoolClosed)
            ));
            assert!(observer.recover().await.is_err());
            if cleanup_committed {
                titles
                    .finish(
                        call.call,
                        None,
                        signalbox_application::UsageTokenAxes {
                            input: None,
                            output: None,
                            cache_creation_input: None,
                            cache_read_input: None,
                        },
                    )
                    .await?;
            }
            observer.pool = pool.clone();
            observer.recover().await?;
            observer.recover().await?;
            let recovered: (bool, bool, bool) = sqlx::query_as(
                "SELECT call.state_kind = 'terminal', reservation.released_at IS NOT NULL,
                    call.title IS NULL AND call.input_tokens IS NULL AND call.output_tokens IS NULL
                    AND call.cache_creation_input_tokens IS NULL AND call.cache_read_input_tokens IS NULL
                 FROM session_title_model_call call JOIN credential_invocation_reservation reservation USING (model_call_id)
                 WHERE model_call_id = $1",
            )
            .bind(call.call.into_uuid())
            .fetch_one(&pool)
            .await?;
            assert_eq!(recovered, (true, true, true));
        }
        pool.close().await;
        Ok(())
    }
}
