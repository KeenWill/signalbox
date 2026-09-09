//! Scheduler nudges at durable human-approval deadlines.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use signalbox_application::{EligibilityNudge, InProcessEligibilityNudge};
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
            drop(tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                scheduled
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&request);
                let _ = nudge.nudge(session);
            }));
        }
        Ok(())
    }
}
