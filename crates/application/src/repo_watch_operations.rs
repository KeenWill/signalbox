//! Bounded operator read models over durable repository-watch facts.

use std::{future::Future, time::SystemTime};

use signalbox_domain::{
    BranchName, CommitSha, MergeableState, PullRequestNumber, PullRequestTitle,
    RepoWatchDispatchId, RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchRuleId,
    RepositorySlug, ReviewState, SessionId,
};

use crate::{
    AttentionSummary, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchThreadState,
};

/// Maximum rows returned by one current repository-watch operations page.
// numeric-bound: ceiling - caps one operations read's rows and projected bytes
const REPO_WATCH_OPERATIONS_PAGE_CEILING: u16 = 64;
/// Maximum rows returned by one historical repository-watch activity page.
// numeric-bound: ceiling - caps one historical read's rows and projected bytes
const REPO_WATCH_ACTIVITY_PAGE_CEILING: u16 = 100;

#[must_use]
pub const fn max_repo_watch_operations_page_items() -> u16 {
    REPO_WATCH_OPERATIONS_PAGE_CEILING
}

#[must_use]
pub const fn max_repo_watch_activity_page_items() -> u16 {
    REPO_WATCH_ACTIVITY_PAGE_CEILING
}

/// One durable event fact with its authoritative observation time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchOperatorEvent {
    pub id: RepoWatchEventId,
    pub cursor_generation: u64,
    pub event_ordinal: u32,
    pub kind: RepoWatchEventKindNameV1,
    pub pull_request: Option<PullRequestNumber>,
    pub observed_at: SystemTime,
}

/// One durable dispatch attempt, distinct from its triggering event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchOperatorDispatch {
    pub id: RepoWatchDispatchId,
    pub event: RepoWatchEventId,
    pub rule: RepoWatchRuleId,
    pub attempted_at: SystemTime,
}

/// One event successfully consumed by an achieved, released dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchOperatorSettlement {
    pub dispatch: RepoWatchDispatchId,
    pub event: RepoWatchEventId,
    pub settled_at: SystemTime,
}

/// Latest durable webhook receipt identity and classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchLatestWebhook {
    pub receipt_sequence: u64,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at: SystemTime,
}

/// Delivery/mapping counts for one explicitly named time window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookWindow {
    pub seconds: u32,
    pub received: u64,
    pub projected: u64,
    pub terminal: u64,
    pub quarantined: u64,
}

/// Count of durable repository-watch events of one closed kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEventKindCount {
    pub kind: RepoWatchEventKindNameV1,
    pub count: u64,
}

/// Ingestion health and deliberately separate last-event/action facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchRepositoryStatus {
    pub repository: RepositorySlug,
    pub cursor_generation: u64,
    pub observed_at: SystemTime,
    pub latest_webhook: Option<RepoWatchLatestWebhook>,
    pub previous_five_minutes: RepoWatchWebhookWindow,
    pub previous_hour: RepoWatchWebhookWindow,
    pub latest_projection_latency_milliseconds: Option<u64>,
    pub maximum_projection_latency_milliseconds_previous_hour: Option<u64>,
    pub event_kind_counts_previous_hour: Vec<RepoWatchEventKindCount>,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchRepositoryStatusPage {
    pub repositories: Vec<RepoWatchRepositoryStatus>,
    pub continuation_after: Option<RepositorySlug>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchDraftStatus {
    Draft,
    ReadyForReview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchChecksStatus {
    NoCompletedSuites,
    Passing,
    Failing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchReviewDecision {
    None,
    Commented,
    Approved,
    ChangesRequested,
}

/// Automation convergence and seal state for the current pull-request head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchAutomationStatus {
    Unattempted,
    Held {
        dispatch: RepoWatchDispatchId,
    },
    Queued {
        latest_event: RepoWatchEventId,
    },
    NonConverged {
        dispatch: RepoWatchDispatchId,
    },
    StaleSeal {
        dispatch: RepoWatchDispatchId,
        sealed_event: RepoWatchEventId,
    },
    CurrentHeadSealed {
        dispatch: RepoWatchDispatchId,
        sealed_event: RepoWatchEventId,
        settled_at: SystemTime,
    },
}

/// Persistence-owned facts joined to one normalized pull-request state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestOperationsFacts {
    pub open_parent: Option<PullRequestNumber>,
    pub open_child_count: u64,
    pub automation: RepoWatchAutomationStatus,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
    pub commissioned_session_count: u64,
}

/// Current operator projection for one watched pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestOperations {
    pub number: PullRequestNumber,
    pub title: PullRequestTitle,
    pub head: CommitSha,
    pub head_repository: RepositorySlug,
    pub head_branch: BranchName,
    pub base_branch: BranchName,
    pub lifecycle: RepoWatchPullRequestLifecycle,
    pub mergeable: MergeableState,
    pub draft: RepoWatchDraftStatus,
    pub checks: RepoWatchChecksStatus,
    pub review_decision: RepoWatchReviewDecision,
    pub stale_review_count: u64,
    pub unresolved_thread_count: u64,
    pub open_parent: Option<PullRequestNumber>,
    pub open_child_count: u64,
    pub automation: RepoWatchAutomationStatus,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
    pub commissioned_session_count: u64,
}

impl RepoWatchPullRequestOperations {
    /// Projects provider state without guessing automation convergence.
    #[must_use]
    pub fn from_state(
        state: &RepoWatchPullRequestState,
        facts: RepoWatchPullRequestOperationsFacts,
    ) -> Self {
        let context = state.context();
        let checks = if state.completed_check_suites().is_empty() {
            RepoWatchChecksStatus::NoCompletedSuites
        } else if state
            .completed_check_suites()
            .iter()
            .any(|suite| suite.outcome() == signalbox_domain::ChecksOutcome::Failure)
        {
            RepoWatchChecksStatus::Failing
        } else {
            RepoWatchChecksStatus::Passing
        };
        let mut current_approved = false;
        let mut current_changes_requested = false;
        let mut current_commented = false;
        let mut stale_review_count = 0_u64;
        for review in state.reviews() {
            if review.commit() != context.head_sha() {
                stale_review_count = stale_review_count.saturating_add(1);
                continue;
            }
            match review.state() {
                Some(ReviewState::Approved) => current_approved = true,
                Some(ReviewState::ChangesRequested) => current_changes_requested = true,
                Some(ReviewState::Commented) => current_commented = true,
                None => {}
            }
        }
        let review_decision = if current_changes_requested {
            RepoWatchReviewDecision::ChangesRequested
        } else if current_approved {
            RepoWatchReviewDecision::Approved
        } else if current_commented {
            RepoWatchReviewDecision::Commented
        } else {
            RepoWatchReviewDecision::None
        };
        let unresolved_thread_count = state
            .threads()
            .iter()
            .filter(|thread| thread.state() == RepoWatchThreadState::Open)
            .count()
            .try_into()
            .unwrap_or(u64::MAX);
        Self {
            number: context.number(),
            title: context.title().clone(),
            head: context.head_sha().clone(),
            head_repository: context.head_repository().clone(),
            head_branch: context.head_branch().clone(),
            base_branch: context.base_branch().clone(),
            lifecycle: state.lifecycle(),
            mergeable: state.mergeable_state(),
            draft: if context.draft() {
                RepoWatchDraftStatus::Draft
            } else {
                RepoWatchDraftStatus::ReadyForReview
            },
            checks,
            review_decision,
            stale_review_count,
            unresolved_thread_count,
            open_parent: facts.open_parent,
            open_child_count: facts.open_child_count,
            automation: facts.automation,
            last_observed_event: facts.last_observed_event,
            last_actionable_event: facts.last_actionable_event,
            last_dispatch_attempt: facts.last_dispatch_attempt,
            last_automation_settlement: facts.last_automation_settlement,
            held_slot_count: facts.held_slot_count,
            queued_obligation_count: facts.queued_obligation_count,
            commissioned_session_count: facts.commissioned_session_count,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestPage {
    pub repository: RepositorySlug,
    pub pull_requests: Vec<RepoWatchPullRequestOperations>,
    pub continuation_after: Option<PullRequestNumber>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchHeldSlot {
    pub dispatch: RepoWatchDispatchId,
    pub pull_request: Option<PullRequestNumber>,
    pub rule: RepoWatchRuleId,
    pub held_since: SystemTime,
    pub sessions: Vec<SessionId>,
    pub blockers: Vec<RepoWatchHeldSlotBlocker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchObligationReadiness {
    Ready,
    Occupied {
        dispatch: RepoWatchDispatchId,
        sessions: Vec<SessionId>,
    },
    Cooldown {
        eligible_at: SystemTime,
    },
    Parked {
        parked_at: SystemTime,
    },
}

/// Durable identity of one queued repository-watch dispatch obligation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RepoWatchObligationId(uuid::Uuid);

impl RepoWatchObligationId {
    #[must_use]
    pub const fn from_uuid(value: uuid::Uuid) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn into_uuid(self) -> uuid::Uuid {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchQueuedObligation {
    pub id: RepoWatchObligationId,
    pub pull_request: Option<PullRequestNumber>,
    pub rule: RepoWatchRuleId,
    pub first_event: RepoWatchEventId,
    pub latest_event: RepoWatchEventId,
    pub matched_event_count: u64,
    pub owed_since: SystemTime,
    pub latest_match_at: SystemTime,
    pub failed_attempts: u64,
    pub readiness: RepoWatchObligationReadiness,
}

/// Keyset position after one held dispatch slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchHeldCursor {
    pub held_since: SystemTime,
    pub dispatch: RepoWatchDispatchId,
}

/// Keyset position after one queued dispatch obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchObligationCursor {
    pub owed_since: SystemTime,
    pub obligation: RepoWatchObligationId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWorkPage {
    pub held_slots: Vec<RepoWatchHeldSlot>,
    pub held_continuation_after: Option<RepoWatchHeldCursor>,
    pub queued_obligations: Vec<RepoWatchQueuedObligation>,
    pub obligation_continuation_after: Option<RepoWatchObligationCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchSessionPurpose {
    RuleDispatch {
        dispatch: RepoWatchDispatchId,
        event: RepoWatchEventId,
        rule: RepoWatchRuleId,
        template: String,
    },
    OperatorCommission {
        dispatch: RepoWatchDispatchId,
        template: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestSession {
    pub commissioned_at: SystemTime,
    pub purpose: RepoWatchSessionPurpose,
    pub attention: AttentionSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchSessionCursor {
    pub commissioned_at: SystemTime,
    pub session: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestSessionPage {
    pub sessions: Vec<RepoWatchPullRequestSession>,
    pub continuation_before: Option<RepoWatchSessionCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchEventCursor {
    pub cursor_generation: u64,
    pub event_ordinal: u32,
}

/// Closed terminal processing result for an accepted webhook delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookDisposition {
    Projected,
    DuplicateState,
    Superseded,
    Ignored,
    Quarantined,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookActivity {
    pub receipt_sequence: u64,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at: SystemTime,
    pub projection_count: u64,
    pub latest_projected_at: Option<SystemTime>,
    pub disposition: Option<RepoWatchWebhookDisposition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchActivityPage {
    pub events: Vec<RepoWatchOperatorEvent>,
    pub event_continuation_before: Option<RepoWatchEventCursor>,
    pub webhooks: Vec<RepoWatchWebhookActivity>,
    pub webhook_continuation_before: Option<u64>,
}

/// Application port for bounded repository-watch operator reads.
pub trait RepoWatchOperationsReader {
    type Error;

    fn repository_statuses(
        &self,
        after: Option<RepositorySlug>,
    ) -> impl Future<Output = Result<RepoWatchRepositoryStatusPage, Self::Error>> + Send;

    fn pull_requests(
        &self,
        repository: RepositorySlug,
        after: Option<PullRequestNumber>,
    ) -> impl Future<Output = Result<RepoWatchPullRequestPage, Self::Error>> + Send;

    fn work(
        &self,
        repository: RepositorySlug,
        held_after: Option<RepoWatchHeldCursor>,
        obligation_after: Option<RepoWatchObligationCursor>,
    ) -> impl Future<Output = Result<RepoWatchWorkPage, Self::Error>> + Send;

    fn pull_request_sessions(
        &self,
        repository: RepositorySlug,
        pull_request: PullRequestNumber,
        before: Option<RepoWatchSessionCursor>,
    ) -> impl Future<Output = Result<RepoWatchPullRequestSessionPage, Self::Error>> + Send;

    fn activity(
        &self,
        repository: RepositorySlug,
        events_before: Option<RepoWatchEventCursor>,
        webhooks_before: Option<u64>,
    ) -> impl Future<Output = Result<RepoWatchActivityPage, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_page_bounds_are_pinned() {
        assert_eq!(max_repo_watch_operations_page_items(), 64);
        assert_eq!(max_repo_watch_activity_page_items(), 100);
    }
}
