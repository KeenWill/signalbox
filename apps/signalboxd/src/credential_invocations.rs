//! Connects invocation capacity to supervised process lifetimes.
use signalbox_domain::ModelCallId;
use signalbox_model_provider_runtime::InvocationProcessObserver;
use signalbox_persistence::{credential_invocations, model_execution::ModelCallRepositoryError};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::sync::watch;

// Process-group absence is polled once per second until shutdown.
const PROCESS_GROUP_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct CredentialInvocationProcesses {
    pool: sqlx::PgPool,
}

impl CredentialInvocationProcesses {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
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
        for (call, group) in credential_invocations::active_processes(&self.pool).await? {
            if group_absent(group) {
                credential_invocations::release(&self.pool, call).await?;
            }
        }
        Ok(())
    }
}

fn group_absent(group: u32) -> bool {
    #[cfg(unix)]
    {
        rustix::process::Pid::from_raw(group as i32).is_some_and(|group| {
            rustix::process::test_kill_process_group(group) == Err(rustix::io::Errno::SRCH)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = group;
        false
    }
}

impl InvocationProcessObserver for CredentialInvocationProcesses {
    fn register(
        &self,
        call: ModelCallId,
        group: u32,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            credential_invocations::register_process(&self.pool, call, group)
                .await
                .is_ok()
        })
    }
    fn finished(
        &self,
        call: ModelCallId,
        process_group: Option<u32>,
        proven_unsent: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let result = async {
                let group = match process_group {
                    Some(group) => Some(group),
                    None => credential_invocations::process_group(&self.pool, call).await?,
                };
                if group.is_some_and(group_absent) || (group.is_none() && proven_unsent) {
                    credential_invocations::release(&self.pool, call).await?;
                } else if let Some(group) = group {
                    credential_invocations::register_process(&self.pool, call, group).await?;
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
