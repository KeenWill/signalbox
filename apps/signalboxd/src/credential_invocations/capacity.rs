//! Re-observes capacity for parked subscription-home members.
use super::PROCESS_GROUP_RECHECK_INTERVAL;
use crate::configuration_reload::ConfigurationReload;
use signalbox_application::ReconciliationSweepInterval;
use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use signalbox_model_runtime::CredentialReference;
use signalbox_persistence::{credential_capacity, model_execution::ModelCallRepositoryError};
use std::time::Duration;
use tokio::sync::watch;

/// Read-only Codex capacity reconciliation, independent of model admission.
pub struct CodexCapacityRefresh {
    pool: sqlx::PgPool,
    configuration: ConfigurationReload,
    interval: Duration,
    probe_bound: Duration,
}

impl CodexCapacityRefresh {
    /// Composes only explicitly admitted Codex homes. The existing reconciliation
    /// interval and CLI probe bound govern polling and each read respectively.
    pub fn new(
        pool: sqlx::PgPool,
        configuration: ConfigurationReload,
        reconciliation: Option<ReconciliationSweepInterval>,
        probe_bound: Duration,
    ) -> Self {
        Self {
            pool,
            configuration,
            interval: reconciliation.map_or(
                PROCESS_GROUP_RECHECK_INTERVAL,
                ReconciliationSweepInterval::get,
            ),
            probe_bound,
        }
    }

    /// Observes live headroom and network waits until shutdown; failed reads preserve prior
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
        let catalogs = self.configuration.catalogs();
        let runtime = match catalogs.models.codex_cli_runtime(None, None) {
            Ok(Some(runtime)) => runtime,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, "credential capacity runtime construction failed");
                return Ok(());
            }
        };
        let profiles = catalogs
            .models
            .credential_invocation_registrations()
            .into_iter()
            .map(|(profile, _)| profile)
            .collect::<Vec<_>>();
        for profile in credential_capacity::waiting_capacity_profiles(&self.pool, &profiles).await?
        {
            let snapshot = match runtime
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
