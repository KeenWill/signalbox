//! Periodic application of durable session admission and waiting deadlines.

use signalbox_application::TurnLivenessScanInterval;
use signalbox_persistence::session_deadline::{
    PostgresSessionDeadlineRepository, SessionDeadlineBounds, SessionDeadlinePassOutcome,
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    time::{MissedTickBehavior, interval},
};

/// Core deadline expiry on the existing liveness cadence.
#[derive(Clone, Debug)]
pub struct LifecycleDeadlineRuntime {
    repository: PostgresSessionDeadlineRepository,
    scan_interval: Option<TurnLivenessScanInterval>,
}

impl LifecycleDeadlineRuntime {
    /// Uses the durable session deadline store and existing configured cadence.
    pub const fn new(
        pool: PgPool,
        scan_interval: Option<TurnLivenessScanInterval>,
        bounds: SessionDeadlineBounds,
    ) -> Self {
        Self {
            repository: PostgresSessionDeadlineRepository::new(pool, bounds),
            scan_interval,
        }
    }

    /// Applies due transitions until shutdown.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let Some(scan_interval) = self.scan_interval else {
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            return;
        };
        let mut ticker = interval(scan_interval.get());
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return;
                    }
                }
                _ = ticker.tick() => {
                    loop {
                        match self.repository.expire_next().await {
                            Ok(SessionDeadlinePassOutcome::Idle) => break,
                            Ok(SessionDeadlinePassOutcome::Retired { session }) => {
                                tracing::info!(session_id = %session.into_uuid(),
                                    "session admission deadline retired the session");
                            }
                            Ok(SessionDeadlinePassOutcome::Parked { session }) => {
                                tracing::info!(session_id = %session.into_uuid(),
                                    "session waiting deadline parked the session");
                            }
                            Err(error) => {
                                tracing::warn!(cause = %error,
                                    "session deadline pass produced no decision");
                                break;
                            }
                        }
                        if *shutdown.borrow() {
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }
}
