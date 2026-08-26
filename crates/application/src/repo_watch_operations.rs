//! Bounded operator read models over durable repository-watch facts.

use std::{collections::BTreeMap, future::Future, time::SystemTime};

use signalbox_domain::{
    BranchName, CommissionedDispatchId, CommitSha, MergeableState, PullRequestNumber,
    PullRequestTitle, RepoWatchDispatchId, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepoWatchRuleId, RepositorySlug, ReviewState, SessionId,
};

use crate::{
    AttentionSummary, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchSingletonKey, RepoWatchThreadState,
};

/// Maximum rows returned by one current repository-watch operations page.
// numeric-bound: guard - caps one operations read's rows and projected bytes
const REPO_WATCH_OPERATIONS_PAGE_CEILING: u16 = 64;
/// Maximum rows returned by one historical repository-watch activity page.
// numeric-bound: guard - caps one historical read's rows and projected bytes
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
    pub cursor_generation: Option<u64>,
    pub observed_at: Option<SystemTime>,
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
pub enum RepoWatchReviewStatus {
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
    pub review_decision: RepoWatchReviewStatus,
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
        let mut current_reviews = BTreeMap::new();
        let mut stale_review_count = 0_u64;
        for review in state.reviews() {
            if review.commit() != context.head_sha() {
                stale_review_count = stale_review_count.saturating_add(1);
                continue;
            }
            // A reviewer's effective state is their latest *opinionated*
            // review, which is the aggregate the blocking-review contract in
            // `docs/spec/repo-watch.md` reads. A later comment-only or
            // dismissed review therefore reports only where that reviewer
            // holds no opinionated state yet, instead of replacing an approval
            // or a blocking review the head still carries. A dismissed review
            // is retained with no state, so it occupies no opinion of its own:
            // a comment the same reviewer left on this head reports over it
            // whichever order the two arrive in.
            match review.state() {
                Some(ReviewState::Approved | ReviewState::ChangesRequested) => {
                    current_reviews.insert(review.reviewer(), review.state());
                }
                Some(ReviewState::Commented) => {
                    let effective = current_reviews.entry(review.reviewer()).or_insert(None);
                    if effective.is_none() {
                        *effective = review.state();
                    }
                }
                None => {
                    current_reviews.entry(review.reviewer()).or_insert(None);
                }
            }
        }
        let review_decision = if current_reviews
            .values()
            .any(|state| *state == Some(ReviewState::ChangesRequested))
        {
            RepoWatchReviewStatus::ChangesRequested
        } else if current_reviews
            .values()
            .any(|state| *state == Some(ReviewState::Approved))
        {
            RepoWatchReviewStatus::Approved
        } else if current_reviews
            .values()
            .any(|state| *state == Some(ReviewState::Commented))
        {
            RepoWatchReviewStatus::Commented
        } else {
            RepoWatchReviewStatus::None
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
    pub singleton: RepoWatchSingletonKey,
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
    /// Held by a live independently commissioned session rather than a
    /// repository-watch dispatch, which owns no dispatch identity here.
    ExternallyBlocked {
        sessions: Vec<SessionId>,
    },
    Cooldown {
        eligible_at: Option<SystemTime>,
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
    pub singleton: RepoWatchSingletonKey,
    pub rule: RepoWatchRuleId,
    pub first_repository: RepositorySlug,
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

/// Position of one independently paged repository-watch stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchPagePosition<T> {
    Start,
    After(T),
    Exhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWorkPage {
    pub held_slots: Vec<RepoWatchHeldSlot>,
    pub held_continuation_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
    pub queued_obligations: Vec<RepoWatchQueuedObligation>,
    pub obligation_continuation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
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
        dispatch: CommissionedDispatchId,
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
    /// A primary-mode delivery that took ownership of the cursor commit,
    /// recorded in place of `Projected` by the delivery whose commit records
    /// the `webhook` producer.
    Committed,
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
    pub event_continuation_before: RepoWatchPagePosition<RepoWatchEventCursor>,
    pub webhooks: Vec<RepoWatchWebhookActivity>,
    pub webhook_continuation_before: RepoWatchPagePosition<u64>,
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
        held_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
        obligation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
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
        events_before: RepoWatchPagePosition<RepoWatchEventCursor>,
        webhooks_before: RepoWatchPagePosition<u64>,
    ) -> impl Future<Output = Result<RepoWatchActivityPage, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use signalbox_domain::{
        GitHubObjectId, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
        RepoWatchAuthorLogin, ReviewState,
    };

    use super::*;
    use crate::{RepoWatchPullRequestStateInput, RepoWatchReviewObservation};

    #[test]
    fn operations_page_bounds_are_pinned() {
        assert_eq!(max_repo_watch_operations_page_items(), 64);
        assert_eq!(max_repo_watch_activity_page_items(), 100);
    }

    fn fixture_head() -> CommitSha {
        CommitSha::try_new(String::from("1111111111111111111111111111111111111111"))
            .expect("fixture head is valid")
    }

    fn fixture_reviewer() -> RepoWatchAuthorLogin {
        RepoWatchAuthorLogin::try_new(String::from("reviewer")).expect("fixture reviewer is valid")
    }

    /// Builds the one reviewer's current-head review sequence in submission
    /// order so each test body stays straight-line, as
    /// `docs/agents/testing-style.md` rule 2 requires.
    fn reviews_by_one_reviewer(
        states: [Option<ReviewState>; 2],
    ) -> Vec<RepoWatchReviewObservation> {
        states
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                RepoWatchReviewObservation::new(
                    GitHubObjectId::new(
                        NonZeroU64::new(u64::try_from(index).expect("fixture index fits u64") + 1)
                            .expect("positive review id"),
                    ),
                    fixture_reviewer(),
                    state,
                    fixture_head(),
                )
            })
            .collect()
    }

    fn review_decision_for(reviews: Vec<RepoWatchReviewObservation>) -> RepoWatchReviewStatus {
        let state = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: PullRequestNumber::new(NonZeroU64::new(1).expect("positive number")),
                head_sha: fixture_head(),
                head_repository: RepositorySlug::try_new(String::from("namespace/repository"))
                    .expect("fixture repository is valid"),
                base_branch: BranchName::try_new(String::from("main"))
                    .expect("fixture base branch is valid"),
                head_branch: BranchName::try_new(String::from("feature"))
                    .expect("fixture head branch is valid"),
                title: PullRequestTitle::try_new(String::from("Feature"))
                    .expect("fixture title is valid"),
                body: PullRequestBody::try_new(String::from("Body"))
                    .expect("fixture body is valid"),
                labels: Vec::new(),
                draft: false,
                author: None,
            }),
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: Vec::new(),
            completed_check_runs: Vec::new(),
            reviews,
            threads: Vec::new(),
            reactions: Vec::new(),
        })
        .expect("fixture state is valid");
        RepoWatchPullRequestOperations::from_state(
            &state,
            RepoWatchPullRequestOperationsFacts {
                open_parent: None,
                open_child_count: 0,
                automation: RepoWatchAutomationStatus::Unattempted,
                last_observed_event: None,
                last_actionable_event: None,
                last_dispatch_attempt: None,
                last_automation_settlement: None,
                held_slot_count: 0,
                queued_obligation_count: 0,
                commissioned_session_count: 0,
            },
        )
        .review_decision
    }

    #[test]
    fn latest_current_head_opinionated_review_is_effective_per_reviewer() {
        let decision = review_decision_for(reviews_by_one_reviewer([
            Some(ReviewState::ChangesRequested),
            Some(ReviewState::Approved),
        ]));

        assert_eq!(decision, RepoWatchReviewStatus::Approved);
    }

    #[test]
    fn comment_only_review_does_not_replace_a_reviewer_approval() {
        let decision = review_decision_for(reviews_by_one_reviewer([
            Some(ReviewState::Approved),
            Some(ReviewState::Commented),
        ]));

        assert_eq!(decision, RepoWatchReviewStatus::Approved);
    }

    #[test]
    fn comment_only_review_does_not_replace_a_blocking_review() {
        let decision = review_decision_for(reviews_by_one_reviewer([
            Some(ReviewState::ChangesRequested),
            Some(ReviewState::Commented),
        ]));

        assert_eq!(decision, RepoWatchReviewStatus::ChangesRequested);
    }

    #[test]
    fn comment_only_review_reports_where_no_opinionated_state_exists() {
        let decision = review_decision_for(reviews_by_one_reviewer([
            Some(ReviewState::Commented),
            None,
        ]));

        assert_eq!(decision, RepoWatchReviewStatus::Commented);
    }

    #[test]
    fn comment_only_review_reports_after_a_dismissal_on_the_same_head() {
        let decision = review_decision_for(reviews_by_one_reviewer([
            None,
            Some(ReviewState::Commented),
        ]));

        assert_eq!(decision, RepoWatchReviewStatus::Commented);
    }
}
