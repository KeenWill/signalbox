//! Re-observes capacity for parked subscription-home members.
use super::PROCESS_GROUP_RECHECK_INTERVAL;
use crate::configuration::HubModelConfiguration;
use signalbox_application::ReconciliationSweepInterval;
use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_codex_cli::{CodexCliConstructionError, CodexCliRuntime};
use signalbox_persistence::{credential_capacity, model_execution::ModelCallRepositoryError};
use std::time::Duration;
use tokio::sync::watch;

/// Read-only Codex capacity reconciliation, independent of model admission.
pub struct CodexCapacityRefresh {
    pool: sqlx::PgPool,
    runtime: CodexCliRuntime,
    profiles: Vec<String>,
    interval: Duration,
    probe_bound: Duration,
}

impl CodexCapacityRefresh {
    /// Composes only explicitly admitted Codex homes. The existing reconciliation
    /// interval and CLI probe bound govern polling and each read respectively.
    pub fn new(
        pool: sqlx::PgPool,
        models: &HubModelConfiguration,
        reconciliation: Option<ReconciliationSweepInterval>,
        probe_bound: Duration,
    ) -> Result<Option<Self>, CodexCliConstructionError> {
        let Some(runtime) = models.codex_cli_runtime(None, None)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            pool,
            runtime,
            profiles: models
                .credential_invocation_registrations()
                .into_iter()
                .map(|(profile, _)| profile)
                .collect(),
            interval: reconciliation.map_or(
                PROCESS_GROUP_RECHECK_INTERVAL,
                ReconciliationSweepInterval::get,
            ),
            probe_bound,
        }))
    }

    /// Observes live headroom waits until shutdown; failed reads preserve prior
    /// evidence and are retried on the next reconciliation pass.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                () = async {
                    if let Err(error) = self.refresh().await {
                        tracing::error!(%error, "credential capacity reconciliation failed");
                    }
                    tokio::time::sleep(self.interval).await;
                } => {}
            }
        }
    }

    async fn refresh(&self) -> Result<(), ModelCallRepositoryError> {
        for profile in
            credential_capacity::waiting_capacity_profiles(&self.pool, &self.profiles).await?
        {
            let snapshot = match self
                .runtime
                .read_credential_capacity(&CredentialReference::new(&profile), self.probe_bound)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%profile, %error, "credential capacity probe failed");
                    continue;
                }
            };
            let snapshot = ProviderRateLimitSnapshot::new(
                snapshot.observed_at,
                snapshot
                    .windows
                    .into_iter()
                    .map(|window| {
                        ProviderRateLimitWindow::new(
                            window.remaining_percent,
                            window.window_duration,
                            window.resets_at,
                        )
                    })
                    .collect(),
            );
            let retained = credential_capacity::retain_credential_capacity_probe(
                &self.pool, &profile, &snapshot,
            )
            .await?;
            tracing::info!(%profile, retained, "credential capacity probe completed");
        }
        Ok(())
    }
}
