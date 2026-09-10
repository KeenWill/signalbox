//! Bounded recovery of successive single-daemon database incarnations.

use std::{future::Future, time::Duration};
use tokio::{
    sync::watch,
    time::{Instant, sleep},
};

/// Deployment-owned guard reacquisition timing.
#[derive(Clone, Copy, Debug)]
pub struct GuardRecoveryPolicy {
    initial_delay: Duration,
    maximum_delay: Duration,
    elapsed_bound: Option<Duration>,
}

impl GuardRecoveryPolicy {
    /// Admits positive backoff delays in ascending order and an optional elapsed bound.
    pub fn new(
        initial_delay: Duration,
        maximum_delay: Duration,
        elapsed_bound: Option<Duration>,
    ) -> Option<Self> {
        if initial_delay.is_zero() || maximum_delay < initial_delay {
            return None;
        }
        Some(Self {
            initial_delay,
            maximum_delay,
            elapsed_bound,
        })
    }
}

/// Why paused database recovery stopped before admission resumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardRecoveryStop {
    /// The configured elapsed recovery bound expired.
    ElapsedBoundExhausted,
    /// Shutdown was requested while runtime admission was paused.
    ShutdownRequested,
}

/// Recovery outcome of one runtime incarnation.
pub enum GuardedIncarnationOutcome<T> {
    /// The runtime reached an ordinary shutdown or a non-recoverable construction result.
    Finished(T),
    /// Database authority was lost and the old runtime must be replaced.
    Reacquire,
}

/// Shares guard loss and successful runtime reconstruction with the elapsed-bound owner.
#[derive(Clone, Debug)]
pub struct GuardRecoveryObserver {
    started: watch::Sender<Option<Instant>>,
}

impl GuardRecoveryObserver {
    /// Starts the recovery clock once per outage, before pool shutdown.
    pub fn guard_lost(&self) {
        self.started.send_if_modified(|started| {
            if started.is_some() {
                return false;
            }
            *started = Some(Instant::now());
            true
        });
    }

    /// Marks a fresh, fenced runtime ready for admission.
    pub fn runtime_ready(&self) {
        self.started.send_replace(None);
    }

    /// Reports whether the current outage is still being recovered.
    pub fn is_recovering(&self) -> bool {
        self.started.borrow().is_some()
    }
}

/// Rebuilds runtime incarnations with capped backoff under one elapsed bound per outage.
pub async fn run_guarded_incarnations<T, Run, Incarnation, Shutdown>(
    policy: GuardRecoveryPolicy,
    mut run: Run,
    shutdown: Shutdown,
) -> Result<T, GuardRecoveryStop>
where
    Run: FnMut(GuardRecoveryObserver) -> Incarnation,
    Incarnation: Future<Output = GuardedIncarnationOutcome<T>>,
    Shutdown: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let (started, mut changes) = watch::channel(None);
    let observer = GuardRecoveryObserver { started };
    let mut delay = policy.initial_delay;
    let mut previous_outage = None;
    loop {
        let outcome = within_recovery_bound(
            &mut changes,
            policy.elapsed_bound,
            run(observer.clone()),
            shutdown.as_mut(),
        )
        .await?;
        match outcome {
            GuardedIncarnationOutcome::Finished(result) => return Ok(result),
            GuardedIncarnationOutcome::Reacquire => observer.guard_lost(),
        }
        let outage = *observer.started.borrow();
        if outage != previous_outage {
            delay = policy.initial_delay;
            previous_outage = outage;
        }
        tracing::warn!(
            backoff_ms = delay.as_millis(),
            "database guard recovery paused runtime admission"
        );
        within_recovery_bound(
            &mut changes,
            policy.elapsed_bound,
            sleep(delay),
            shutdown.as_mut(),
        )
        .await?;
        delay = delay.saturating_mul(2).min(policy.maximum_delay);
    }
}

async fn within_recovery_bound<T>(
    changes: &mut watch::Receiver<Option<Instant>>,
    bound: Option<Duration>,
    operation: impl Future<Output = T>,
    mut shutdown: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<T, GuardRecoveryStop> {
    tokio::pin!(operation);
    loop {
        let started = *changes.borrow_and_update();
        let remaining = started
            .zip(bound)
            .map(|(started, bound)| bound.saturating_sub(started.elapsed()));
        let expiry = async {
            match remaining {
                Some(remaining) => sleep(remaining).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            () = expiry => return Err(GuardRecoveryStop::ElapsedBoundExhausted),
            () = async {
                if started.is_some() { shutdown.as_mut().await; }
                else { std::future::pending::<()>().await; }
            } => return Err(GuardRecoveryStop::ShutdownRequested),
            _ = changes.changed() => {},
            result = &mut operation => return Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn stalled_then_ready(
        observer: GuardRecoveryObserver,
        runs: Arc<AtomicUsize>,
        admitted: Arc<AtomicUsize>,
        stall: Duration,
    ) -> GuardedIncarnationOutcome<()> {
        if runs.fetch_add(1, Ordering::SeqCst) == 0 {
            observer.guard_lost();
            sleep(stall).await;
            GuardedIncarnationOutcome::Reacquire
        } else {
            observer.runtime_ready();
            admitted.fetch_add(1, Ordering::SeqCst);
            GuardedIncarnationOutcome::Finished(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guard_stall_under_bound_resumes_scheduling() {
        let admitted = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let policy = GuardRecoveryPolicy::new(
            Duration::from_secs(1),
            Duration::from_secs(4),
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        let result = run_guarded_incarnations(
            policy,
            |observer| {
                stalled_then_ready(
                    observer,
                    runs.clone(),
                    admitted.clone(),
                    Duration::from_secs(2),
                )
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(result, Ok(()));
        assert_eq!(admitted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn guard_stall_over_bound_stops_with_typed_reason() {
        let policy = GuardRecoveryPolicy::new(
            Duration::from_secs(1),
            Duration::from_secs(4),
            Some(Duration::from_secs(3)),
        )
        .unwrap();
        let result = run_guarded_incarnations(
            policy,
            |observer| async move {
                observer.guard_lost();
                sleep(Duration::from_secs(4)).await;
                GuardedIncarnationOutcome::Finished(())
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(result, Err(GuardRecoveryStop::ElapsedBoundExhausted));
    }

    #[tokio::test(start_paused = true)]
    async fn guard_recovery_none_keeps_waiting_for_authority() {
        let policy =
            GuardRecoveryPolicy::new(Duration::from_secs(1), Duration::from_secs(4), None).unwrap();
        let result = run_guarded_incarnations(
            policy,
            |observer| async move {
                observer.guard_lost();
                sleep(Duration::from_secs(60)).await;
                observer.runtime_ready();
                GuardedIncarnationOutcome::Finished(())
            },
            std::future::pending(),
        )
        .await;
        assert_eq!(result, Ok(()));
    }
    #[tokio::test(start_paused = true)]
    async fn guard_recovery_observes_shutdown_with_no_elapsed_bound() {
        let policy =
            GuardRecoveryPolicy::new(Duration::from_secs(1), Duration::from_secs(4), None).unwrap();
        let result = run_guarded_incarnations(
            policy,
            |observer| async move {
                observer.guard_lost();
                std::future::pending::<GuardedIncarnationOutcome<()>>().await
            },
            sleep(Duration::from_secs(2)),
        )
        .await;
        assert_eq!(result, Err(GuardRecoveryStop::ShutdownRequested));
    }
}
