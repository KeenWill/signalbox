//! Periodic export of the session-lifecycle metrics as Prometheus gauges.
//!
//! The numbers themselves are the views the persistence layer owns; this pass
//! only moves them onto the metric registry on a timer, because a gauge is a
//! pull-time value and the read behind it is an aggregate over the whole
//! session population rather than something a scrape may run inline.
//!
//! Nothing here recomputes a metric. Reading the same statements the operator
//! status snapshot reads is what keeps the exported series and the operator's
//! own numbers the same numbers.

use std::sync::Arc;

use signalbox_persistence::lifecycle_metrics::{LifecycleMetricsError, LifecycleMetricsRepository};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    time::{Duration, Interval, MissedTickBehavior, interval},
};

use crate::telemetry::TelemetryMetrics;

/// Why one lifecycle-metric pass exported nothing.
const PASS_FAILURE_CAUSE: &str = "lifecycle_metric_pass_failed";

/// Periodic exporter for the §12 metrics.
pub struct LifecycleMetricsRuntime {
    repository: LifecycleMetricsRepository,
    metrics: Arc<TelemetryMetrics>,
    scan_interval: Option<Duration>,
}

impl LifecycleMetricsRuntime {
    /// Builds the pass over one pool and the daemon's metric registry.
    ///
    /// A `None` scan interval is the configured `"none"` policy: the metrics
    /// stay readable through the operator status command and no series is
    /// exported. That is a deployment choice rather than a defect, so the pass
    /// idles instead of failing startup.
    pub fn new(
        pool: PgPool,
        metrics: Arc<TelemetryMetrics>,
        scan_interval: Option<Duration>,
    ) -> Self {
        Self {
            repository: LifecycleMetricsRepository::new(pool),
            metrics,
            scan_interval,
        }
    }

    /// Refreshes the exported gauges until shutdown is requested.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let Some(scan_interval) = self.scan_interval else {
            let _ = shutdown.wait_for(|requested| *requested).await;
            return;
        };
        let mut timer = self.timer(scan_interval);
        loop {
            select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = timer.tick() => self.export().await,
            }
        }
    }

    fn timer(&self, scan_interval: Duration) -> Interval {
        let mut timer = interval(scan_interval);
        // A pass that ran long must not then run back to back catching up: the
        // report is a snapshot of the present, so a skipped tick has nothing
        // to deliver late.
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        timer
    }

    async fn export(&self) {
        match self.repository.read().await {
            Ok(report) => self.metrics.observe_lifecycle_metrics(&report),
            Err(error) => report_pass_failure(&error),
        }
    }
}

fn report_pass_failure(error: &LifecycleMetricsError) {
    tracing::warn!(
        cause = PASS_FAILURE_CAUSE,
        detail = %error,
        "lifecycle metric export pass decided nothing"
    );
}
