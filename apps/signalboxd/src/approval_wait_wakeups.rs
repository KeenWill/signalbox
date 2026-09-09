//! Scheduler nudges at durable human-approval deadlines.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use signalbox_application::InProcessEligibilityNudge;
use signalbox_domain::{SessionId, ToolRequestId};
use signalbox_persistence::tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError};

/// Restores and schedules one wake-up per pending finite approval wait.
#[derive(Clone, Debug)]
pub struct ApprovalWaitWakeups {
    repository: PostgresToolLoopRepository,
    nudge: InProcessEligibilityNudge,
    scheduled: Arc<Mutex<HashSet<ToolRequestId>>>,
}

impl ApprovalWaitWakeups {
    /// Uses the normal scheduler nudge path without a periodic sweep.
    pub fn new(repository: PostgresToolLoopRepository, nudge: InProcessEligibilityNudge) -> Self {
        Self {
            repository,
            nudge,
            scheduled: Arc::default(),
        }
    }

    /// Schedules stored deadlines for one session, or all sessions at startup.
    pub async fn refresh(&self, session: Option<SessionId>) -> Result<(), ToolLoopRepositoryError> {
        for (request, session, delay) in self
            .repository
            .pending_human_approval_waits(session)
            .await?
        {
            if !self
                .scheduled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(request)
            {
                continue;
            }
            let scheduled = Arc::clone(&self.scheduled);
            let nudge = self.nudge.clone();
            drop(tokio::spawn(wake_when_due(
                request, session, delay, scheduled, nudge,
            )));
        }
        Ok(())
    }
}

async fn wake_when_due(
    request: ToolRequestId,
    session: SessionId,
    delay: Duration,
    scheduled: Arc<Mutex<HashSet<ToolRequestId>>>,
    nudge: InProcessEligibilityNudge,
) {
    tokio::time::sleep(delay).await;
    let _ = nudge.nudge_waiting_for_capacity(session).await;
    scheduled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request);
}

#[cfg(test)]
mod tests {
    use std::{future::Future, num::NonZeroUsize, task::Poll};

    use signalbox_application::{
        EligibilityNudge, EligibilityNudgeOutcome, EligibilitySweep, EligibilitySweepBatch,
        EligibilityWorkSource, InProcessEligibilityWorkSource,
    };
    use sqlx::types::Uuid;

    use super::*;

    struct NoHints;

    impl EligibilitySweep for NoHints {
        type Error = std::convert::Infallible;

        async fn find_sessions(&mut self) -> Result<EligibilitySweepBatch, Self::Error> {
            Ok(EligibilitySweepBatch::new(Vec::new(), false))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn approval_deadline_retains_its_wake_until_buffer_capacity_returns() {
        let occupied = SessionId::from_uuid(Uuid::from_u128(1));
        let waiting = SessionId::from_uuid(Uuid::from_u128(2));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(3));
        let (nudge, mut source) =
            InProcessEligibilityWorkSource::with_options(NoHints, None, NonZeroUsize::new(1));
        assert_eq!(nudge.nudge(occupied), EligibilityNudgeOutcome::Enqueued);
        let scheduled = Arc::new(Mutex::new(HashSet::from([request])));
        // The exact duration is arbitrary; advancing virtual time expires the wake.
        let delay = Duration::from_secs(1);
        let wake = wake_when_due(request, waiting, delay, Arc::clone(&scheduled), nudge);
        tokio::pin!(wake);
        std::future::poll_fn(|context| {
            assert!(wake.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(delay).await;
        std::future::poll_fn(|context| {
            assert!(wake.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(
            scheduled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&request)
        );

        assert_eq!(source.next().await, Ok(occupied));
        wake.await;

        assert!(
            !scheduled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&request)
        );
        assert_eq!(source.next().await, Ok(waiting));
    }
}
