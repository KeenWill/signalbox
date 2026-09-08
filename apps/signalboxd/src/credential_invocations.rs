//! Connects invocation capacity to supervised process lifetimes.
use signalbox_domain::ModelCallId;
use signalbox_model_provider_runtime::InvocationProcessObserver;
use signalbox_persistence::{credential_invocations, model_execution::ModelCallRepositoryError};
use std::{future::Future, pin::Pin};

#[derive(Clone)]
pub struct CredentialInvocationProcesses {
    pool: sqlx::PgPool,
}

impl CredentialInvocationProcesses {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
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
        proven_unsent: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let result = async {
                let group = credential_invocations::process_group(&self.pool, call).await?;
                if group.is_some_and(group_absent) || (group.is_none() && proven_unsent) {
                    credential_invocations::release(&self.pool, call).await?;
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
