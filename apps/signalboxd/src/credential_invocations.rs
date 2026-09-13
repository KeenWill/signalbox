//! Connects invocation capacity to supervised process lifetimes.
use signalbox_application::{EligibilityNudge, InProcessEligibilityNudge};
use signalbox_domain::{ModelCallId, SessionId};
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
        }
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
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
        self.nudge_eligible_waits().await
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
        session_titles::{SessionTitleCall, SessionTitleRepository},
    };

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
            assert!(titles.prepare(&mut call, &Default::default()).await?);
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
