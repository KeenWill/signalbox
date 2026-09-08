//! Connects invocation capacity to supervised process lifetimes.
use signalbox_domain::ModelCallId;
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

mod process_identity;

// Process-group absence is polled once per second until shutdown.
const PROCESS_GROUP_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct CredentialInvocationProcesses {
    pool: sqlx::PgPool,
    observed: Arc<Mutex<BTreeMap<ModelCallId, (u32, String)>>>,
}

impl CredentialInvocationProcesses {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
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
        credential_invocations::release_unregistered_terminal_calls(&self.pool).await?;
        for (call, group, start_time) in
            credential_invocations::active_processes(&self.pool).await?
        {
            if group_absent(group, &start_time) {
                credential_invocations::release(&self.pool, call).await?;
            }
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
                .insert(call, (group, start_time.clone()));
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
                    Some(group) => Some(group),
                    None => credential_invocations::process_group(&self.pool, call).await?,
                };
                if group
                    .as_ref()
                    .is_some_and(|(group, start_time)| group_absent(*group, start_time))
                    || (group.is_none() && proven_unsent)
                {
                    credential_invocations::release(&self.pool, call).await?;
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
