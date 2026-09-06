//! Conditional GitHub polling and durable repository-watch handoff.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
    future::Future,
    num::{NonZeroU16, NonZeroU64},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH, LINK,
        RETRY_AFTER, USER_AGENT,
    },
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use signalbox_application::{
    EligibilityNudge, EligibilityNudgeOutcome, InProcessEligibilityNudge, RepoWatchBranchHead,
    RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceAssessmentInput, RepoWatchDifferFailureKind, RepoWatchDispatchService,
    RepoWatchDispatchTransaction, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierV1, RepoWatchEventOccurrenceV1,
    RepoWatchMergedPullRequestBaselineV1, RepoWatchObservation, RepoWatchObservationApplyV1,
    RepoWatchObservationPatchV1, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewDecision, RepoWatchReviewObservation,
    RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchStaleReviewClearanceCandidate, RepoWatchTargetedRefreshCoalescerV1,
    RepoWatchTargetedRefreshV1, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWebhookDeliveryV1, RepoWatchWebhookDeliveryV1Input, RepoWatchWebhookIgnoredReasonV1,
    RepoWatchWebhookMappedNoChangeV1, RepoWatchWebhookMappingError, RepoWatchWebhookMappingV1,
    RepoWatchWorkflowRunObservation, SubmitInputIdGenerator, UuidV7RepoWatchDispatchIdGenerator,
    UuidV7RepoWatchEventIdGenerator, apply_repo_watch_observation_patch_v1,
    derive_repo_watch_events_with_merged_baselines, map_repo_watch_webhook_delivery_v1,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, DurableCommandId,
    GitHubObjectId, LabelName, MergeableState, ModelAlias, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    ReactionChange, ReactionContent, ReactionSubject, RepoWatchAuthorLogin, RepoWatchEvent,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchRule,
    RepoWatchWorkflowRunAttempt, RepositorySlug, ReviewState, ReviewThreadId, UserContent,
    WorkflowName,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_persistence::repo_watch::{
    PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest, RepoWatchCursor,
    RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchEventProducer,
    RepoWatchObservedReviewState, RepoWatchPlannedStaleReviewClearance,
    RepoWatchStaleReviewClearanceOutcome, RepoWatchStaleReviewClearanceRenewal,
    RepoWatchStoreError,
};
use signalbox_persistence::repo_watch_dispatch::{
    PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError,
};
use signalbox_persistence::repo_watch_dispatch_obligation::{
    RepoWatchDispatchObligation, RepoWatchDispatchRetryPolicy,
};
use signalbox_persistence::repo_watch_webhook::{
    PendingRepoWatchWebhookDelivery, PostgresRepoWatchWebhookStore, RepoWatchWebhookDeliveryKey,
    RepoWatchWebhookDisposition, RepoWatchWebhookParityCauseV1, RepoWatchWebhookPendingPageSize,
    RepoWatchWebhookProjection, RepoWatchWebhookStoreError, RepoWatchWebhookTargetedQuery,
    RepoWatchWebhookTerminalRequest,
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep, sleep_until, timeout},
};

use crate::SessionTemplateConfiguration;
use crate::configuration::{
    FileCredentialAccess, HubModelConfiguration, RepositoryWatchConfiguration,
    RepositoryWatchWebhookMode, WatchedRepositoryConfiguration,
};
use crate::repo_watch_webhook_runtime::{RepoWatchWebhookRuntime, RepoWatchWebhookRuntimeError};

const REST_BASE_URL: &str = "https://api.github.com/";
const API_VERSION: &str = "2026-03-10";
const USER_AGENT_VALUE: &str = "signalbox-repository-watch";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: usize = 100;
const MAX_RESULT_PAGES: u16 = 100;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const WEBHOOK_PAYLOAD_PURGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTITY_TAG_BYTES: usize = 1_024;
const MAX_REQUESTS_PER_POLL: usize = 20_000;
const MAX_CACHED_RESOURCES: usize = 20_000;
const MAX_CONCURRENT_PULL_REQUEST_FETCHES: usize = 8;
const MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS: usize = 4;
// GitHub's commit check-run search covers the latest 1,000 suites. The suite
// inventory is already complete, so a count at or below this ceiling proves
// the cheaper commit query is exhaustive; larger inventories retain the
// suite-by-suite path rather than weakening the baseline.
const MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH: usize = 1_000;
// One polling attempt may transfer this many response bytes. The dogfooded
// repository exceeds 64 MiB in a single attempt, and the bound fails the
// attempt rather than shedding, so it has to clear real event volume.
const MAX_POLL_WIRE_BYTES: usize = 768 * 1024 * 1024;
// What one poller may retain between attempts, which is per watched repository
// and therefore multiplies by the configured repository count. Deliberately not
// raised with the per-attempt bound: transfer is transient, retention is not.
const MAX_CACHED_WIRE_BYTES: usize = 64 * 1024 * 1024;
const NON_GATING_CHECK_NAME_MARKERS: [&str; 4] = [
    "report only",
    "coderabbit",
    "codecov/project",
    "codecov/patch",
];
const WEBHOOK_PENDING_PAGE_SIZE: NonZeroU16 =
    NonZeroU16::new(25).expect("webhook pending page size is positive");
const WEBHOOK_DRAIN_RETRY_DELAY: Duration = Duration::from_secs(5);
// Consecutive drain failures double the retry delay up to this ceiling. A
// delivery whose projection cannot succeed while it stays pending would
// otherwise re-read and re-attempt the same page every five seconds for as long
// as it remained, turning one admitted burst into unbounded repeated database
// and credentialed provider work.
const WEBHOOK_DRAIN_RETRY_MAX_DELAY: Duration = Duration::from_secs(300);
// Six doublings carry the base delay past the ceiling above, so the ceiling is
// what bounds the delay rather than the doubling count.
const WEBHOOK_DRAIN_RETRY_MAX_DOUBLINGS: u32 = 6;
// A separate task inspects durable pending work at this cadence, so a wedged
// serialized repository task cannot also silence the observer meant to
// expose it.
const WEBHOOK_DRAIN_MONITOR_INTERVAL: Duration = Duration::from_secs(30);
// The minimum drain deadline covers a cursor up to two scaling quanta. Larger
// cursor documents receive proportional time below, while this floor preserves
// the original bound for ordinary repositories.
const WEBHOOK_DRAIN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
// numeric-bound: guard - prevents cursor size from granting an unbounded drain deadline
const WEBHOOK_DRAIN_TIMEOUT_PAYLOAD_QUANTUM_BYTES: u64 = 1024 * 1024;
// numeric-bound: guard - limits deadline growth while admitting real cursor decode and refresh cost
const WEBHOOK_DRAIN_TIMEOUT_PER_PAYLOAD_QUANTUM: Duration = Duration::from_secs(30);
// numeric-bound: guard - returns a payload-scaled drain to its scheduler within fifteen minutes
const WEBHOOK_DRAIN_MAX_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
// Reconciliation before or after the drain is durable and replayable. This
// margin lets the drain deadline and bounded child cleanup report before the
// enclosing attempt is cancelled.
// numeric-bound: guard - bounds reconciliation time outside the payload-scaled drain
const WEBHOOK_ATTEMPT_TIMEOUT_MARGIN: Duration = Duration::from_secs(10);
// Every shared child-set join uses this bound. A later attempt may retry the
// join, but it never spawns alongside survivors or wedges the scheduler while
// waiting for a child that does not finish cancellation.
const WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
// Shutdown gives a retained targeted completion a short grace period, then
// aborts and joins it so a wedged database operation cannot prevent the
// repository supervisor from stopping.
// numeric-bound: guard - prevents a wedged targeted completion from stalling supervisor shutdown forever
const WEBHOOK_TARGETED_COMPLETION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// The monitor reads through the shared daemon pool, whose connections wedged
// repositories can hold all of. An unbounded acquisition would leave the
// observer silent during exactly the degradation it exists to expose, so the
// inspection is bounded and expiry is itself an operator-visible signal. Well
// under the stall threshold, so a bounded failure is reported within the
// cadence rather than displacing the report it exists to produce.
const WEBHOOK_DRAIN_MONITOR_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
// Sizing the stored cursor is what derives the payload-scaled deadlines below,
// so those deadlines cannot span it. Left unbounded it would be the one step of
// a webhook attempt no deadline covers, and startup joins every repository's
// attempt without a bound of its own. Bounded like the monitor's read above:
// the pool is shared with repositories whose own work can hold its connections,
// and expiry is reported as the persistence failure the attempt already
// handles.
// numeric-bound: guard - prevents an unbounded sizing read ahead of every payload-scaled attempt
const WEBHOOK_CURSOR_SIZING_TIMEOUT: Duration = Duration::from_secs(10);
// One webhook drain visits one 25-delivery page before returning to the
// scheduler. A full 100-delivery storage page repeatedly exceeded the outer
// deadline under admitted dogfood bursts even though receipts were progressing,
// turning saturation into failure and exponential backoff. Webhook wakes
// accelerate reconciliation and must never crowd out the full poll that
// performs it, so remaining work re-arms its own wake after this bounded
// quantum instead of holding the worker across poll deadlines.
const WEBHOOK_DRAIN_PAGE_LIMIT: usize = 1;
// How many consecutive webhook preemptions one still-due complete poll yields
// to before it runs uninterruptibly. The termination argument for preemption is
// that a bounded page stops re-arming once it observes no remainder, but while
// authenticated deliveries keep the durable backlog nonempty every successful
// page requests a continuation, the next fresh sweep observes that wake, and the
// poll is preempted again: at ingress meeting or exceeding drain throughput the
// authoritative completeness sweep never commits, and the facts outside the
// webhook mapping — reactions, missed deliveries — stop being reconciled for as
// long as the backlog lasts.
//
// Counted in preemptions, but chosen against the pages they cost, because a
// preempted pass drains two: the poll's own pre-poll drain in
// `run_attempt_prelude` and then the attempt the admission wake runs. The
// suppressed pass that ends the cycle drains one more, so the sweep waits behind
// `2 * MAX_CONSECUTIVE_POLL_PREEMPTIONS + 1` bounded pages — nine here, about
// 225 deliveries at WEBHOOK_PENDING_PAGE_SIZE each, which covers a merge
// batch. Raising this raises that ceiling twice as fast, and every page may
// spend its own deadline. Remaining backlog re-arms the scheduler immediately
// after the sweep, so suppression delays webhook work rather than dropping it.
// numeric-bound: guard - prevents sustained webhook ingress from starving the complete poll
const MAX_CONSECUTIVE_POLL_PREEMPTIONS: u32 = 4;
// One repository scheduling phase may settle this many cutoff or dispatch
// records before returning to the webhook-aware outer loop. The remaining work
// is durable and re-arms that loop; bounding the phase prevents an event backlog
// from owning the serialized repository task indefinitely.
// How many times one terminal record may be re-attempted while PostgreSQL keeps
// losing its commit result. Each attempt is settled by a read, so this bounds a
// flapping connection rather than a genuinely undecided outcome.
const MAX_WEBHOOK_TERMINAL_ATTEMPTS: u32 = 5;
// What separates those attempts, so a dropping connection cannot become a hot
// loop against the pool.
const WEBHOOK_TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(250);

const REVIEW_THREADS_QUERY: &str = r#"
query RepositoryWatchReviewThreads(
  $namespace: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        nodes { id isResolved }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const CONVERGENCE_QUERY: &str = r#"
query RepositoryWatchConvergence(
  $namespace: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      baseRefName
      baseRefOid
      mergeable
      reviewDecision
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              contexts(first: 100, after: $after) {
                nodes {
                  __typename
                  ... on CheckRun { name status conclusion }
                  ... on StatusContext { context state }
                }
                pageInfo { hasNextPage endCursor }
              }
            }
          }
        }
      }
    }
  }
}
"#;

const BLOCKING_REVIEWS_QUERY: &str = r#"
query RepositoryWatchBlockingReviews(
  $namespace: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      baseRefOid
      reviewDecision
      latestOpinionatedReviews(first: 100, after: $after) {
        nodes {
          id
          state
          author { login }
          commit { oid }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const DISMISS_REVIEW_MUTATION: &str = r#"
mutation RepositoryWatchDismissReview($review: ID!, $message: String!) {
  dismissPullRequestReview(
    input: {pullRequestReviewId: $review, message: $message}
  ) {
    pullRequestReview { id state }
  }
}
"#;

const REVIEW_CLEARANCE_STATE_QUERY: &str = r#"
query RepositoryWatchReviewClearanceState($review: ID!, $after: String) {
  node(id: $review) {
    ... on PullRequestReview {
      id
      state
      commit { oid }
      pullRequest {
        number
        state
        headRefOid
        baseRefName
        baseRefOid
        reviewDecision
        latestOpinionatedReviews(first: 100, after: $after) {
          nodes { id }
          pageInfo { hasNextPage endCursor }
        }
      }
    }
  }
}
"#;

/// Why the repository-watch runtime could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryWatchRuntimeConstructionError;

impl fmt::Display for RepositoryWatchRuntimeConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository-watch HTTP transport could not be constructed")
    }
}

impl Error for RepositoryWatchRuntimeConstructionError {}

/// Why the repository-watch supervisor ended before daemon shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchRuntimeError {
    RepositoryTaskExited,
    RepositoryTaskPanicked,
    WebhookMonitorExited,
    WebhookListenerExited,
    WebhookListenerFailed,
    TaskSetEmpty,
}

impl fmt::Display for RepositoryWatchRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RepositoryTaskExited => "repository-watch task exited before shutdown",
            Self::RepositoryTaskPanicked => "repository-watch task panicked",
            Self::WebhookMonitorExited => {
                "repository-watch webhook drain monitor exited before shutdown"
            }
            Self::WebhookListenerExited => {
                "repository-watch webhook listener exited before shutdown"
            }
            Self::WebhookListenerFailed => "repository-watch webhook listener failed",
            Self::TaskSetEmpty => "repository-watch task set became empty",
        })
    }
}

impl Error for RepositoryWatchRuntimeError {}

/// Supervisor for one independent polling task per configured repository.
pub struct RepositoryWatchRuntime {
    tasks: Vec<RepositoryWatchTask>,
    webhook: Option<RepoWatchWebhookRuntime>,
}

/// Deployment-owned work policies for the repository-watch scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryWatchNumericBounds {
    reconciliation_quantum: Option<usize>,
    webhook_drain_work_budget: Option<Duration>,
}

impl RepositoryWatchNumericBounds {
    /// Groups the repository-watch policies loaded from required configuration.
    pub const fn new(
        reconciliation_quantum: Option<usize>,
        webhook_drain_work_budget: Option<Duration>,
    ) -> Self {
        Self {
            reconciliation_quantum,
            webhook_drain_work_budget,
        }
    }
}

impl RepositoryWatchRuntime {
    /// Constructs all repository-specific clients without reading credentials.
    pub fn try_new(
        pool: PgPool,
        configuration: &RepositoryWatchConfiguration,
        templates: SessionTemplateConfiguration,
        models: HubModelConfiguration,
        credential_pin: signalbox_persistence::SessionCredentialPin,
        eligibility_nudge: InProcessEligibilityNudge,
        numeric_bounds: RepositoryWatchNumericBounds,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let RepositoryWatchNumericBounds {
            reconciliation_quantum,
            webhook_drain_work_budget,
        } = numeric_bounds;
        let mut tasks = Vec::with_capacity(configuration.repositories().len());
        let mut webhook_workers = HashMap::new();
        let payload_purge = WebhookPayloadPurgeSchedule::starting_now();
        for repository in configuration.repositories() {
            let mut webhook_nudge = None;
            let webhook_work = repository.webhook().map(|_| {
                let (sender, receiver) = watch::channel(());
                // Shared rather than cloned: a `watch` sender cannot be
                // cloned, and both the listener and the worker publish here.
                let sender = Arc::new(sender);
                webhook_workers.insert(repository.repository().clone(), Arc::clone(&sender));
                // The worker keeps a wake of its own so a bounded drain can hand
                // the scheduler its turn and still resume.
                webhook_nudge = Some(sender);
                receiver
            });
            tasks.push(RepositoryWatchTask::try_new(
                repository,
                RepositoryWatchTaskContext {
                    pool: pool.clone(),
                    signal_reviewers: configuration.signal_reviewers().to_vec(),
                    rules: configuration.rules().to_vec(),
                    templates: templates.clone(),
                    models: models.clone(),
                    credential_pin: credential_pin.clone(),
                    eligibility_nudge: eligibility_nudge.clone(),
                    webhook_work,
                    webhook_nudge,
                    payload_purge: payload_purge.clone(),
                    reconciliation_quantum,
                    webhook_drain_work_budget,
                },
            )?);
        }
        let webhook = RepoWatchWebhookRuntime::try_new(pool, configuration, webhook_workers)
            .map_err(|_| RepositoryWatchRuntimeConstructionError)?;
        Ok(Self { tasks, webhook })
    }

    /// Completes each repository's bounded startup webhook attempt before the
    /// daemon admits scheduler work.
    pub async fn prepare_startup(&mut self) -> Result<(), RepositoryWatchRuntimeError> {
        let outcomes = futures_util::future::join_all(
            self.tasks
                .iter_mut()
                .map(RepositoryWatchTask::prepare_startup),
        )
        .await;
        if outcomes.into_iter().any(std::convert::identity) {
            return Err(RepositoryWatchRuntimeError::RepositoryTaskExited);
        }
        Ok(())
    }

    /// Runs every repository task until the daemon broadcasts shutdown.
    pub async fn run(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut tasks = JoinSet::new();
        let (task_shutdown_sender, task_shutdown) = watch::channel(*shutdown.borrow());
        let mut pollers = Vec::with_capacity(self.tasks.len());
        for task in self.tasks {
            if task.webhook_work.is_some() {
                let repository = task.repository.clone();
                let webhook_store = task.webhook_store.clone();
                let cursor_store = task.store.clone();
                let monitor_shutdown = task_shutdown.clone();
                tasks.spawn(async move {
                    monitor_webhook_drain(
                        repository,
                        webhook_store,
                        cursor_store,
                        monitor_shutdown,
                    )
                    .await;
                    RepositoryWatchChildExit::WebhookMonitor
                });
            }
            pollers.push(Arc::clone(&task.poller));
            let repository_shutdown = task_shutdown.clone();
            tasks.spawn(async move {
                task.run(repository_shutdown).await;
                RepositoryWatchChildExit::Repository
            });
        }
        if let Some(webhook) = self.webhook {
            let webhook_shutdown = task_shutdown.clone();
            tasks.spawn(async move {
                RepositoryWatchChildExit::Webhook(webhook.run(webhook_shutdown).await)
            });
        }
        supervise_repository_tasks(tasks, pollers, shutdown, task_shutdown_sender).await
    }
}

enum RepositoryWatchChildExit {
    Repository,
    WebhookMonitor,
    Webhook(Result<(), RepoWatchWebhookRuntimeError>),
}

async fn supervise_repository_tasks(
    mut tasks: JoinSet<RepositoryWatchChildExit>,
    pollers: Vec<Arc<GitHubRepositoryPoller>>,
    mut shutdown: watch::Receiver<bool>,
    task_shutdown: watch::Sender<bool>,
) -> Result<(), RepositoryWatchRuntimeError> {
    let result = async {
        if *shutdown.borrow() {
            let _ = task_shutdown.send(true);
            while let Some(result) = tasks.join_next().await {
                result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
            }
            return Ok(());
        }
        loop {
            select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        let _ = task_shutdown.send(true);
                        while let Some(result) = tasks.join_next().await {
                            result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
                        }
                        return Ok(());
                    }
                }
                completed = tasks.join_next() => {
                    return match completed {
                        Some(Ok(_)) if *shutdown.borrow() => {
                            let _ = task_shutdown.send(true);
                            while let Some(result) = tasks.join_next().await {
                                result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
                            }
                            Ok(())
                        }
                        Some(Ok(RepositoryWatchChildExit::Repository)) => {
                            Err(RepositoryWatchRuntimeError::RepositoryTaskExited)
                        }
                        Some(Ok(RepositoryWatchChildExit::WebhookMonitor)) => {
                            Err(RepositoryWatchRuntimeError::WebhookMonitorExited)
                        }
                        Some(Ok(RepositoryWatchChildExit::Webhook(Ok(())))) => {
                            Err(RepositoryWatchRuntimeError::WebhookListenerExited)
                        }
                        Some(Ok(RepositoryWatchChildExit::Webhook(Err(_)))) => {
                            Err(RepositoryWatchRuntimeError::WebhookListenerFailed)
                        }
                        Some(Err(_)) => Err(RepositoryWatchRuntimeError::RepositoryTaskPanicked),
                        None => Err(RepositoryWatchRuntimeError::TaskSetEmpty),
                    };
                }
            }
        }
    }
    .await;

    // Unexpected sibling exit uses the same cleanup path as operator shutdown,
    // allowing repository tasks to settle retained targeted completions before
    // the supervisor returns the lifecycle error.
    let _ = task_shutdown.send(true);
    while tasks.join_next().await.is_some() {}
    for poller in &pollers {
        if !poller
            .drain_fetches_within(WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT)
            .await
        {
            tracing::error!(
                repository = %poller.repository.as_str(),
                timeout_seconds = WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT.as_secs(),
                cause_code = "webhook_cancelled_fetch_drain_timed_out",
                "repository-watch supervisor fetch cleanup exceeded its deadline"
            );
        }
    }
    result
}

async fn receive_webhook_work(receiver: &mut Option<watch::Receiver<()>>) -> bool {
    let received = match receiver {
        Some(receiver) => receiver.changed().await.is_ok(),
        None => std::future::pending().await,
    };
    if !received {
        *receiver = None;
    }
    received
}

/// Marks a wake already covered by the immediately following durable drain.
///
/// The scheduler may select an equally ready poll deadline ahead of this wake.
/// Observing it before the drain means a delivery admitted while the drain is
/// running publishes a later change and remains visible to the provider-sweep
/// interrupt arm.
fn observe_webhook_work_before_drain(receiver: &mut Option<watch::Receiver<()>>) {
    let Some(active_receiver) = receiver.as_mut() else {
        return;
    };
    let disconnected = match active_receiver.has_changed() {
        Ok(true) => {
            active_receiver.borrow_and_update();
            false
        }
        Ok(false) => false,
        Err(_) => true,
    };
    if disconnected {
        *receiver = None;
    }
}

enum PollAttemptWait<T> {
    Completed(T),
    Continue,
    Shutdown,
    Webhook,
    WebhookRetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookPollInterrupt {
    Enabled,
    Suppressed,
}

impl WebhookPollInterrupt {
    const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Whether projection backoff is in force for the drain retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainRetryBackoff {
    InForce,
    Clear,
}

impl DrainRetryBackoff {
    fn of(retry: &WebhookDrainRetry) -> Self {
        if retry.is_backing_off() {
            Self::InForce
        } else {
            Self::Clear
        }
    }
}

/// Whether a still-due complete poll may yield to webhook admission again.
///
/// A drain retry in backoff already suppresses admission preemption so the
/// retry deadline stays authoritative. Beyond that, consecutive preemptions of
/// the same due poll are counted and bounded: each one drains a bounded page and
/// returns the poll to a fresh interruptible pass, so nothing in that cycle ends
/// it while ingress keeps the durable backlog nonempty. Suppression does not
/// discard the wake — it is latched, and the scheduler admits it as ordinary
/// webhook work as soon as the sweep commits.
const fn poll_webhook_interrupt(
    backoff: DrainRetryBackoff,
    consecutive_preemptions: u32,
) -> WebhookPollInterrupt {
    if matches!(backoff, DrainRetryBackoff::InForce)
        || consecutive_preemptions >= MAX_CONSECUTIVE_POLL_PREEMPTIONS
    {
        WebhookPollInterrupt::Suppressed
    } else {
        WebhookPollInterrupt::Enabled
    }
}

async fn await_poll_or_interrupt<F>(
    poll: F,
    shutdown: &mut watch::Receiver<bool>,
    webhook_retry: &WebhookDrainRetry,
    webhook_work: &mut Option<watch::Receiver<()>>,
    webhook_interrupt: WebhookPollInterrupt,
) -> PollAttemptWait<F::Output>
where
    F: Future,
{
    select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                PollAttemptWait::Shutdown
            } else {
                PollAttemptWait::Continue
            }
        }
        () = webhook_retry.due() => PollAttemptWait::WebhookRetry,
        admitted = receive_webhook_work(webhook_work), if webhook_interrupt.is_enabled() => {
            if admitted {
                PollAttemptWait::Webhook
            } else {
                PollAttemptWait::Continue
            }
        }
        result = poll => PollAttemptWait::Completed(result),
    }
}

#[derive(Default)]
struct WebhookDrainRetry {
    deadline: Option<Instant>,
    deadline_kind: Option<WebhookRetryDeadlineKind>,
    consecutive_failures: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookRetryDeadlineKind {
    DrainBackoff,
    DispatchFollowUp,
}

impl WebhookDrainRetry {
    /// The delay the most recent failure earned.
    ///
    /// A first failure waits `WEBHOOK_DRAIN_RETRY_DELAY`, which keeps an
    /// ordinary transient failure — a dropped connection, a lock contended past
    /// its timeout — draining promptly. Each further consecutive failure
    /// doubles that up to `WEBHOOK_DRAIN_RETRY_MAX_DELAY`, so a delivery that
    /// cannot be projected at all costs bounded repeated work instead of a
    /// fixed five-second loop for as long as it stays pending. The drain
    /// monitor reports such a delivery independently of this delay.
    fn delay(&self) -> Duration {
        let doublings = self
            .consecutive_failures
            .saturating_sub(1)
            .min(WEBHOOK_DRAIN_RETRY_MAX_DOUBLINGS);
        WEBHOOK_DRAIN_RETRY_DELAY
            .saturating_mul(1_u32 << doublings)
            .min(WEBHOOK_DRAIN_RETRY_MAX_DELAY)
    }

    fn schedule(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.deadline = Some(Instant::now() + self.delay());
        self.deadline_kind = Some(WebhookRetryDeadlineKind::DrainBackoff);
    }

    /// Whether projection backoff is currently in force.
    ///
    /// A dispatch follow-up also owns a deadline, but it must not suppress
    /// admission wakes or make a full poll omit healthy drain work. The
    /// deadline is what makes a kind in force: a spent one is retained only so
    /// that rearming can restore it.
    const fn is_backing_off(&self) -> bool {
        self.deadline.is_some()
            && matches!(
                self.deadline_kind,
                Some(WebhookRetryDeadlineKind::DrainBackoff)
            )
    }

    /// What a full polling attempt should do about its own drain step.
    const fn poll_drain(&self) -> WebhookDrain {
        if self.is_backing_off() {
            WebhookDrain::Deferred
        } else {
            WebhookDrain::Run
        }
    }

    /// Spends the earned backoff and arms a retry at the base delay.
    ///
    /// The drain itself succeeded, so the consecutive-failure count it earned
    /// is spent. The work that failed after it runs only from a later
    /// repository attempt, and the delivery that would have woken one is
    /// already terminal — its admission wake is spent too — so without an armed
    /// retry that work waits for another admission or the next full poll rather
    /// than the delay this schedule promises. The drain's own escalation does
    /// not apply, because the drain is not what failed.
    fn arm_follow_up(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.deadline = Some(now + self.delay());
        self.deadline_kind = Some(WebhookRetryDeadlineKind::DispatchFollowUp);
    }

    /// Marks the owed retry as taken, keeping the delay it has earned.
    ///
    /// The retry arm firing is that retry being spent. Leaving the deadline
    /// owed would select the same attempt again on the next pass, because the
    /// retry arm precedes the poll arm, and the delay would never be waited
    /// out. What follows decides the next deadline: a drain failure advances
    /// it, a success clears the backoff, and a failure before the drain arms it
    /// again at the same delay and the same kind. The kind is therefore
    /// retained rather than dropped; the absent deadline is what marks the
    /// retry as spent.
    fn consume(&mut self) {
        self.deadline = None;
    }

    /// Arms a retry when none is owed, without counting a drain failure.
    ///
    /// Two failures reach here and neither is the drain's: a full poll, and an
    /// attempt that failed before reaching its drain. The delay therefore does
    /// not advance — only consecutive drain failures may move it.
    ///
    /// An owed deadline is left exactly as it stands, including one that
    /// expired while the failing attempt ran. That attempt did not take the
    /// retry, so the next pass must find it ready: moving it forward here would
    /// let a poll interval shorter than the delay push the retry out on every
    /// slow failure and starve the drain indefinitely, which is the starvation
    /// the retry arm's priority exists to prevent.
    ///
    /// A deadline the retry arm just spent keeps its kind. The attempt that
    /// followed it failed before its drain, so it says nothing about
    /// projection: a follow-up whose trailing work failed again would otherwise
    /// rearm as backoff and begin suppressing admission wakes and poll drains
    /// while projection was healthy, which is exactly what the two kinds exist
    /// to keep apart.
    fn arm_if_unowed(&mut self, now: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(now + self.delay());
            self.deadline_kind = self
                .deadline_kind
                .or(Some(WebhookRetryDeadlineKind::DrainBackoff));
        }
    }

    fn clear(&mut self) {
        self.deadline = None;
        self.deadline_kind = None;
        self.consecutive_failures = 0;
    }

    /// `trailing_failure` is the attempt's cutoff or dispatch work after the
    /// drain, which the drain's own outcome cannot report: both run once the
    /// drain has committed, and a delivery already terminal will not wake
    /// another attempt for them.
    fn update_after_poll_drain(
        &mut self,
        outcome: WebhookDrainOutcome,
        trailing_failure: Option<RepositoryWatchAttemptError>,
        now: Instant,
    ) {
        match outcome {
            WebhookDrainOutcome::Drained if trailing_failure.is_some() => {
                self.arm_follow_up(now);
            }
            WebhookDrainOutcome::Drained => self.update_after(&Ok(())),
            WebhookDrainOutcome::DispatchFailedAfterTerminal(_) => {
                self.arm_follow_up(now);
            }
            WebhookDrainOutcome::ProjectionFailed(error) => {
                self.update_after(&Err(error));
            }
        }
    }

    fn update_after(&mut self, result: &Result<(), RepositoryWatchAttemptError>) {
        match result {
            Ok(()) => self.clear(),
            Err(_) => self.schedule(),
        }
    }

    async fn due(&self) {
        match self.deadline {
            Some(deadline) => sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }
}

/// Which schedule ran a webhook attempt, reported as a field so its three
/// callers share one set of records rather than three near-identical ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookAttemptTrigger {
    Startup,
    Wake,
    Retry,
}

impl WebhookAttemptTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Wake => "wake",
            Self::Retry => "retry",
        }
    }
}

/// How one drain ended, so work after a delivery reached terminal state is not
/// charged to projection.
///
/// A dispatch failure cannot be retried by draining again — the delivery it
/// followed is already terminal and will not be reloaded — so counting it as a
/// projection failure would grow the backoff, suppress admission wakes, and
/// make polls omit their drain steps while every projection was succeeding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookDrainOutcome {
    /// Every delivery this drain visited reached terminal state.
    Drained,
    /// A delivery could not be projected, and the drain retains it.
    ProjectionFailed(RepositoryWatchAttemptError),
    /// Every delivery reached terminal state and dispatch work for one of them
    /// then failed.
    DispatchFailedAfterTerminal(RepositoryWatchAttemptError),
}

impl WebhookDrainOutcome {
    /// The drain's failure, whichever step produced it.
    const fn failure(self) -> Option<RepositoryWatchAttemptError> {
        match self {
            Self::Drained => None,
            Self::ProjectionFailed(error) | Self::DispatchFailedAfterTerminal(error) => Some(error),
        }
    }

    /// Whether deadline cancellation can leave projection work pending and
    /// therefore makes a cursor-advancing complete poll unsafe.
    const fn blocks_complete_poll_after_timeout(self) -> bool {
        !matches!(self, Self::DispatchFailedAfterTerminal(_))
    }
}

/// How one webhook-triggered attempt ended, with the drain's own outcome held
/// apart from the cutoff and dispatch work around it.
///
/// The backoff measures the drain, so only a drain failure may advance it.
/// Dispatch work that kept failing would otherwise grow the delay to its
/// ceiling, suppress admission wakes, and make full polls omit their drain
/// steps while projection itself was healthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookAttemptOutcome {
    /// Every step succeeded.
    Completed,
    /// The drain itself failed, so the backoff advances.
    DrainFailed(RepositoryWatchAttemptError),
    /// The drain succeeded and the dispatch work after it failed, so the
    /// backoff is cleared: the drain is what it measures.
    DrainedThenFailed(RepositoryWatchAttemptError),
    /// A step before the drain failed, so the drain never ran. It has earned
    /// neither a longer delay nor a clear, and a retry is armed only if none is
    /// already owed.
    FailedBeforeDrain(RepositoryWatchAttemptError),
}

impl WebhookAttemptOutcome {
    /// The attempt's failure, whichever step produced it.
    const fn failure(self) -> Option<RepositoryWatchAttemptError> {
        match self {
            Self::Completed => None,
            Self::DrainFailed(error)
            | Self::DrainedThenFailed(error)
            | Self::FailedBeforeDrain(error) => Some(error),
        }
    }
}

/// Which step of a webhook attempt is running.
///
/// The enclosing attempt deadline can expire in any of them, and cancellation
/// carries no failure of its own to classify. Recording the step lets a
/// cancelled attempt report the same outcome that step's own failure would
/// have, so a wedge outside the drain does not advance the projection backoff
/// that only drain failures are meant to grow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookAttemptPhase {
    /// Activation and the leading reconciliation that precede the drain.
    BeforeDrain,
    /// The drain itself, which owns the durable pending deliveries.
    Drain,
    /// The cutoff and dispatch reconciliation that follow a committed drain.
    AfterDrain,
}

impl WebhookAttemptPhase {
    /// The outcome a cancellation during this step reports.
    ///
    /// The drain performs its own post-terminal dispatch work, which the phase
    /// alone cannot separate from projection: that window sits inside the drain
    /// and is marked by `dispatch_in_flight` instead. The delivery it follows is
    /// already terminal and will not be reloaded, so a cancellation there is the
    /// surrounding dispatch work's failure exactly as one after the drain is —
    /// reporting it as a drain failure would grow the projection backoff, and
    /// suppress admission wakes and poll drains for up to its cap, while no
    /// projection remained.
    const fn cancelled_outcome(
        self,
        error: RepositoryWatchAttemptError,
        dispatch_in_flight: bool,
    ) -> WebhookAttemptOutcome {
        match self {
            Self::BeforeDrain => WebhookAttemptOutcome::FailedBeforeDrain(error),
            Self::Drain if dispatch_in_flight => WebhookAttemptOutcome::DrainedThenFailed(error),
            Self::Drain => WebhookAttemptOutcome::DrainFailed(error),
            Self::AfterDrain => WebhookAttemptOutcome::DrainedThenFailed(error),
        }
    }

    /// Whether a cancellation during this step may have left a durable delivery
    /// pending, so the next cursor-advancing poll must be fenced.
    ///
    /// Only the drain owns pending deliveries. A cancellation there is
    /// indistinguishable from the drain's own deadline as far as the durable
    /// queue is concerned: work the drain had loaded may never have reached a
    /// terminal record, and a complete poll that commits past it would advance
    /// the cursor over a delivery still waiting to be projected.
    const fn cancellation_fences_complete_poll(self) -> bool {
        matches!(self, Self::Drain)
    }

    /// The operator-facing label for the cancelled step.
    const fn label(self) -> &'static str {
        match self {
            Self::BeforeDrain => "before_drain",
            Self::Drain => "drain",
            Self::AfterDrain => "after_drain",
        }
    }
}

/// Why one attempt could not derive its payload-scaled deadlines.
///
/// Both causes leave the attempt with no bound to run under, so both report the
/// same persistence failure; they are distinguished only so the operator can
/// tell a rejected read from one that never answered.
#[derive(Debug)]
enum WebhookCursorSizingError {
    Store(RepoWatchStoreError),
    Settlement(RepositoryWatchAttemptError),
    TimedOut,
}

impl fmt::Display for WebhookCursorSizingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::Settlement(error) => write!(
                formatter,
                "retained webhook completion failed ({})",
                error.cause_code()
            ),
            Self::TimedOut => write!(
                formatter,
                "durable cursor sizing exceeded its {}-second bound",
                WEBHOOK_CURSOR_SIZING_TIMEOUT.as_secs()
            ),
        }
    }
}

async fn load_webhook_attempt_deadlines(
    store: &PostgresRepoWatchStore,
    repository: &RepositorySlug,
) -> Result<WebhookAttemptDeadlines, WebhookCursorSizingError> {
    timeout(
        WEBHOOK_CURSOR_SIZING_TIMEOUT,
        load_webhook_attempt_deadlines_unbounded(store, repository),
    )
    .await
    .map_err(|_| WebhookCursorSizingError::TimedOut)?
}

async fn load_webhook_attempt_deadlines_unbounded(
    store: &PostgresRepoWatchStore,
    repository: &RepositorySlug,
) -> Result<WebhookAttemptDeadlines, WebhookCursorSizingError> {
    let cursor_payload_bytes = store
        .load_cursor_payload_bytes(repository)
        .await
        .map_err(WebhookCursorSizingError::Store)?
        .unwrap_or(0);
    Ok(WebhookAttemptDeadlines::for_cursor_payload(
        cursor_payload_bytes,
    ))
}

/// Payload-derived bounds for one serialized webhook attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WebhookAttemptDeadlines {
    drain: Duration,
    attempt: Duration,
    cursor_payload_bytes: u64,
}

impl WebhookAttemptDeadlines {
    fn for_cursor_payload(cursor_payload_bytes: u64) -> Self {
        let payload_quanta = cursor_payload_bytes
            .div_ceil(WEBHOOK_DRAIN_TIMEOUT_PAYLOAD_QUANTUM_BYTES)
            .max(1);
        let max_quanta = WEBHOOK_DRAIN_MAX_ATTEMPT_TIMEOUT.as_secs()
            / WEBHOOK_DRAIN_TIMEOUT_PER_PAYLOAD_QUANTUM.as_secs();
        let scaled_seconds =
            payload_quanta.min(max_quanta) * WEBHOOK_DRAIN_TIMEOUT_PER_PAYLOAD_QUANTUM.as_secs();
        let drain = Duration::from_secs(scaled_seconds)
            .max(WEBHOOK_DRAIN_ATTEMPT_TIMEOUT)
            .min(WEBHOOK_DRAIN_MAX_ATTEMPT_TIMEOUT);
        Self {
            drain,
            attempt: drain.saturating_add(WEBHOOK_ATTEMPT_TIMEOUT_MARGIN),
            cursor_payload_bytes,
        }
    }

    const fn stall_threshold(self) -> Duration {
        self.drain
    }
}

/// Whether a full polling attempt performs its own webhook drain step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookDrain {
    /// No retry is owed, so this attempt drains durable pending deliveries.
    Run,
    /// A retry already owes that drain. Running it here would repeat at the
    /// poll cadence exactly the work the backoff exists to space out.
    Deferred,
}

/// What a repository task's next wake asks it to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryWatchWake {
    /// Shutdown was observed, or its sender was dropped.
    Stop,
    /// A durable webhook drain retry came due.
    WebhookRetry,
    /// The next full poll came due.
    Poll,
    /// A wake published by webhook admission arrived.
    WebhookWork,
    /// Nothing actionable happened; the deadlines are re-evaluated.
    Continue,
}

/// Chooses a repository task's next action, cancelled by shutdown.
///
/// The order is load-bearing. An overdue drain retry precedes an overdue poll:
/// when a full poll consistently outlasts its own interval — a provider request
/// timing out repeatedly, say — the poll deadline is already elapsed on entry
/// to every pass, and a biased poll arm ahead of the retry would win every one
/// of them and leave durable webhook deliveries pending for as long as polling
/// kept failing. Deferring a poll costs at most one drain attempt, because
/// taking the retry reschedules its deadline before the next pass.
///
/// An admission wake is disabled while projection backoff is owed, so it cannot
/// start a drain the backoff has deferred. A dispatch follow-up remains in this
/// schedule without activating that suppression. Admission is authenticated but
/// not trusted to pace this worker: replays are acknowledged at the intake rate,
/// and each one publishing an immediate drain would drive provider and database
/// work at that rate for as long as the drain kept failing. Nothing is lost by
/// waiting, because the wake coalesces and an unobserved one stays observable for
/// the attempt that follows the retry.
async fn next_repository_wake(
    shutdown: &mut watch::Receiver<bool>,
    next_poll: Instant,
    webhook_retry: &WebhookDrainRetry,
    webhook_work: &mut Option<watch::Receiver<()>>,
) -> RepositoryWatchWake {
    select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                RepositoryWatchWake::Stop
            } else {
                RepositoryWatchWake::Continue
            }
        }
        () = webhook_retry.due() => RepositoryWatchWake::WebhookRetry,
        () = sleep_until(next_poll) => RepositoryWatchWake::Poll,
        admitted = receive_webhook_work(webhook_work), if !webhook_retry.is_backing_off() => {
            if admitted {
                RepositoryWatchWake::WebhookWork
            } else {
                RepositoryWatchWake::Continue
            }
        }
    }
}

/// The next deadline on a fixed cadence anchored at `previous`.
///
/// The cadence holds while the work between deadlines fits inside the interval,
/// so neither a poll attempt nor a monitor query accrues its own duration into
/// the rate. Work that outlasts the interval cannot hold that cadence, and
/// leaving the missed tick in the past would keep the timer ready on entry to
/// every pass, so an overrun starts a fresh interval from now. Every pass then
/// reaches a real sleep, which is what lets a repository task's other arms be
/// polled at all.
fn next_cadence_deadline(previous: Instant, interval: Duration, now: Instant) -> Instant {
    let anchored = previous + interval;
    if anchored > now {
        anchored
    } else {
        now + interval
    }
}

/// Schedules a first-ever repository baseline immediately and a warm restart at
/// whatever remains of the ordinary cadence.
///
/// Startup has already drained durable webhook work before reaching this
/// decision. A completed sweep therefore remains an authoritative baseline until
/// the next scheduled one; paying that same sweep on every daemon restart would
/// let operational restarts multiply provider quota independently of the
/// configured poll interval.
///
/// What remains is measured from the durable record of the last completed sweep,
/// never from this process's own start. Anchoring on startup made the deadline a
/// full interval away every time, so a daemon restarting more often than a
/// repository's interval — the tight deployment cycle this scheduling was
/// written for — never reached it at all: a polling-only repository's signal
/// went unobserved indefinitely, and a webhook-enabled one lost the completeness
/// sweep for every fact its targeted projections do not cover. A repository
/// whose last sweep is already older than the interval, or that has none on
/// record, polls immediately.
fn initial_poll_deadline(
    now: Instant,
    interval: Duration,
    complete_poll_age: Option<Duration>,
) -> Instant {
    match complete_poll_age {
        Some(age) => now + interval.saturating_sub(age),
        None => now,
    }
}

fn repository_reconciliation_quantum_exhausted(
    processed: usize,
    reconciliation_quantum: Option<usize>,
) -> bool {
    reconciliation_quantum.is_some_and(|quantum| processed >= quantum)
}

fn repository_reconciliation_should_yield(
    processed: usize,
    reconciliation_quantum: Option<usize>,
    continuation_available: bool,
) -> bool {
    continuation_available
        && repository_reconciliation_quantum_exhausted(processed, reconciliation_quantum)
}

/// Awaits `work`, leaving it cancellable by shutdown.
///
/// Returns `None` when shutdown — or a dropped sender — cancelled the work
/// before it produced an output; a channel notification that is not shutdown
/// resumes the same work rather than abandoning it. Every await a supervised
/// child performs outside its own `select!` has to pass through here:
/// `supervise_repository_tasks` joins each child before aborting the set, so a
/// single uncancellable database await would hang daemon termination for as
/// long as PostgreSQL stayed unresponsive.
async fn run_until_shutdown<F>(shutdown: &mut watch::Receiver<bool>, work: F) -> Option<F::Output>
where
    F: Future,
{
    let mut work = std::pin::pin!(work);
    loop {
        select! {
            output = &mut work => return Some(output),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return None;
                }
            }
        }
    }
}

async fn monitor_webhook_drain(
    repository: RepositorySlug,
    webhook_store: PostgresRepoWatchWebhookStore,
    cursor_store: PostgresRepoWatchStore,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut next_inspection = Instant::now() + WEBHOOK_DRAIN_MONITOR_INTERVAL;
    let mut progress = WebhookDrainProgress::default();
    loop {
        select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            () = sleep_until(next_inspection) => {}
        }
        // Anchored before the inspection rather than after it. A slow query
        // would otherwise push the cadence out by its own duration and delay
        // the stall report precisely when the database is degraded, which is
        // when that report matters.
        next_inspection = next_cadence_deadline(
            next_inspection,
            WEBHOOK_DRAIN_MONITOR_INTERVAL,
            Instant::now(),
        );
        // The sizing read acquires a pooled connection and queries, for the
        // same reason the inspection below is raced with shutdown: its own
        // ten-second bound is still ten seconds this child would hold the
        // supervisor while PostgreSQL was unresponsive.
        let Some(sized) = run_until_shutdown(
            &mut shutdown,
            load_webhook_attempt_deadlines(&cursor_store, &repository),
        )
        .await
        else {
            return;
        };
        let stall_threshold = match sized {
            Ok(deadlines) => deadlines.stall_threshold(),
            Err(error) => {
                tracing::error!(
                    repository = %repository.as_str(),
                    cause_code = "webhook_drain_monitor_cursor_sizing_failed",
                    cause = %error,
                    "repository-watch webhook drain monitor could not size the durable cursor"
                );
                continue;
            }
        };
        // The pending-receipt inspection acquires a pooled connection and
        // queries. Awaiting it outside shutdown would hold this child while
        // PostgreSQL was unresponsive, and the supervisor joins every child
        // before aborting the set, so the daemon could not stop.
        if run_until_shutdown(
            &mut shutdown,
            inspect_webhook_drain(&repository, &webhook_store, stall_threshold, &mut progress),
        )
        .await
        .is_none()
        {
            return;
        }
    }
}

#[derive(Default)]
struct WebhookDrainProgress {
    // This is observation state, not recovery authority: pending receipts remain
    // durable. A fresh daemon deliberately observes one full threshold before
    // declaring an old queue head stalled.
    unchanged_head: Option<(NonZeroU64, Instant, Duration)>,
}

impl WebhookDrainProgress {
    fn observe(
        &mut self,
        receipt_sequence: NonZeroU64,
        observed_at: Instant,
        stall_threshold: Duration,
    ) -> Option<(Duration, Duration)> {
        match self.unchanged_head {
            Some((previous_sequence, unchanged_since, retained_threshold))
                if previous_sequence == receipt_sequence =>
            {
                let pinned_threshold = retained_threshold.max(stall_threshold);
                self.unchanged_head = Some((previous_sequence, unchanged_since, pinned_threshold));
                Some((
                    observed_at.saturating_duration_since(unchanged_since),
                    pinned_threshold,
                ))
            }
            _ => {
                self.unchanged_head = Some((receipt_sequence, observed_at, stall_threshold));
                None
            }
        }
    }

    fn clear(&mut self) {
        self.unchanged_head = None;
    }
}

async fn inspect_webhook_drain(
    repository: &RepositorySlug,
    store: &PostgresRepoWatchWebhookStore,
    stall_threshold: Duration,
    progress: &mut WebhookDrainProgress,
) {
    // Identity and receipt only, never the admitted body. This runs on a fixed
    // cadence for every webhook repository, so loading a pending page here
    // would transfer the payload — up to the 25 MiB admission ceiling — on
    // every pass, for every stalled repository at once, precisely while the
    // system is already degraded.
    let inspection = tokio::time::timeout(
        WEBHOOK_DRAIN_MONITOR_QUERY_TIMEOUT,
        store.load_oldest_pending_receipt(repository),
    );
    let oldest = match inspection.await {
        Ok(Ok(Some(oldest))) => oldest,
        Ok(Ok(None)) => {
            progress.clear();
            return;
        }
        Err(_) => {
            tracing::error!(
                repository = %repository.as_str(),
                timeout_seconds = WEBHOOK_DRAIN_MONITOR_QUERY_TIMEOUT.as_secs(),
                cause_code = "webhook_drain_monitor_query_timed_out",
                "repository-watch webhook drain monitor cannot reach durable work"
            );
            return;
        }
        Ok(Err(error)) => {
            tracing::error!(
                repository = %repository.as_str(),
                cause_code = "webhook_drain_monitor_query_failed",
                cause = %error,
                "repository-watch webhook drain monitor cannot inspect durable work"
            );
            return;
        }
    };
    let pending_for = oldest.pending_for();
    let Some((stalled_for, pinned_threshold)) =
        progress.observe(oldest.receipt().sequence(), Instant::now(), stall_threshold)
    else {
        return;
    };
    if pending_for < pinned_threshold || stalled_for < pinned_threshold {
        return;
    }
    tracing::error!(
        repository = %repository.as_str(),
        hook_id = oldest.key().hook_id().get(),
        delivery_id = %oldest.key().delivery_id(),
        receipt_sequence = oldest.receipt().sequence().get(),
        pending_seconds = pending_for.as_secs(),
        stalled_seconds = stalled_for.as_secs(),
        cause_code = "webhook_projection_drain_stalled",
        "durable repository-watch webhook delivery remains undispositioned"
    );
}

struct RepositoryWatchTask {
    repository: RepositorySlug,
    interval: Duration,
    poller: Arc<GitHubRepositoryPoller>,
    store: PostgresRepoWatchStore,
    dispatch_store: PostgresRepoWatchDispatchStore,
    rules: Vec<RepoWatchRule>,
    templates: SessionTemplateConfiguration,
    models: HubModelConfiguration,
    eligibility_nudge: InProcessEligibilityNudge,
    webhook_store: PostgresRepoWatchWebhookStore,
    webhook_work: Option<watch::Receiver<()>>,
    webhook_nudge: Option<Arc<watch::Sender<()>>>,
    /// Whether an authenticated delivery writes the durable cursor itself.
    webhook_primary: bool,
    webhook_shadow: Option<WebhookShadowBaseline>,
    webhook_shadow_superseded: bool,
    webhook_shadow_supersession_epoch: u64,
    webhook_projected_terminal_in_flight: Option<RepoWatchWebhookDeliveryKey>,
    webhook_dispatch_in_flight: bool,
    webhook_targeted_completion: Option<RetainedTargetedWebhookCompletion>,
    webhook_terminal_ambiguous: Option<RepoWatchWebhookDeliveryKey>,
    webhook_drain_first_failure: Option<RepositoryWatchAttemptError>,
    webhook_drain_projection_failure: Option<RepositoryWatchAttemptError>,
    webhook_drain_timed_out: bool,
    webhook_attempt_phase: WebhookAttemptPhase,
    payload_purge: WebhookPayloadPurgeSchedule,
    rules_activated: bool,
    startup_webhook_retry: Option<WebhookDrainRetry>,
    reconciliation_quantum: Option<usize>,
    webhook_drain_work_budget: Option<Duration>,
}

/// One process-wide schedule for the expired-payload purge.
///
/// Every repository task shares it, so a multi-repository daemon purges at
/// most once per interval rather than once per repository, and the lock is
/// held across the deletion so concurrent tasks cannot run it redundantly.
#[derive(Clone)]
struct WebhookPayloadPurgeSchedule {
    next: Arc<tokio::sync::Mutex<Instant>>,
}

impl WebhookPayloadPurgeSchedule {
    fn starting_now() -> Self {
        Self {
            next: Arc::new(tokio::sync::Mutex::new(Instant::now())),
        }
    }
}

struct RepositoryWatchTaskContext {
    pool: PgPool,
    signal_reviewers: Vec<RepoWatchAuthorLogin>,
    rules: Vec<RepoWatchRule>,
    templates: SessionTemplateConfiguration,
    models: HubModelConfiguration,
    credential_pin: signalbox_persistence::SessionCredentialPin,
    eligibility_nudge: InProcessEligibilityNudge,
    webhook_work: Option<watch::Receiver<()>>,
    webhook_nudge: Option<Arc<watch::Sender<()>>>,
    payload_purge: WebhookPayloadPurgeSchedule,
    reconciliation_quantum: Option<usize>,
    webhook_drain_work_budget: Option<Duration>,
}

fn record_dispatch_start_nudge_outcome(
    repository: &RepositorySlug,
    session: signalbox_domain::SessionId,
    outcome: EligibilityNudgeOutcome,
) {
    match outcome {
        EligibilityNudgeOutcome::Enqueued => {}
        EligibilityNudgeOutcome::Coalesced => tracing::info!(
            repository = %repository.as_str(),
            session_id = %session.as_uuid(),
            cause_code = "repository_watch_dispatch_start_nudge_coalesced",
            "repository-watch dispatch-start nudge was coalesced"
        ),
        EligibilityNudgeOutcome::DroppedAtCapacity => tracing::warn!(
            repository = %repository.as_str(),
            session_id = %session.as_uuid(),
            cause_code = "repository_watch_dispatch_start_nudge_capacity",
            "repository-watch dispatch-start nudge was not enqueued"
        ),
        EligibilityNudgeOutcome::WorkSourceClosed => tracing::warn!(
            repository = %repository.as_str(),
            session_id = %session.as_uuid(),
            cause_code = "repository_watch_dispatch_start_nudge_closed",
            "repository-watch dispatch-start nudge was not enqueued"
        ),
    }
}

impl RepositoryWatchTask {
    fn try_new(
        configuration: &WatchedRepositoryConfiguration,
        context: RepositoryWatchTaskContext,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let RepositoryWatchTaskContext {
            pool,
            signal_reviewers,
            rules,
            templates,
            models,
            credential_pin,
            eligibility_nudge,
            webhook_work,
            webhook_nudge,
            payload_purge,
            reconciliation_quantum,
            webhook_drain_work_budget,
        } = context;
        let credential_reference = configuration.credential_reference();
        let credentials = FileCredentialAccess::new_bounded(
            configuration.credential_file().to_path_buf(),
            credential_reference.clone(),
            MAX_CREDENTIAL_BYTES,
        );
        let store = PostgresRepoWatchStore::new(pool.clone());
        Ok(Self {
            repository: configuration.repository().clone(),
            interval: configuration.poll_interval(),
            poller: Arc::new(GitHubRepositoryPoller::try_new(
                configuration.repository().clone(),
                signal_reviewers,
                credentials,
                credential_reference,
            )?),
            store,
            dispatch_store: PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin),
            webhook_store: PostgresRepoWatchWebhookStore::new(pool),
            rules,
            templates,
            models,
            eligibility_nudge,
            webhook_work,
            webhook_nudge,
            webhook_primary: configuration
                .webhook()
                .is_some_and(|webhook| webhook.mode() == RepositoryWatchWebhookMode::Primary),
            webhook_shadow: None,
            webhook_shadow_superseded: false,
            webhook_shadow_supersession_epoch: 0,
            webhook_projected_terminal_in_flight: None,
            webhook_dispatch_in_flight: false,
            webhook_targeted_completion: None,
            webhook_terminal_ambiguous: None,
            webhook_drain_first_failure: None,
            webhook_drain_projection_failure: None,
            webhook_drain_timed_out: false,
            webhook_attempt_phase: WebhookAttemptPhase::BeforeDrain,
            payload_purge,
            rules_activated: false,
            startup_webhook_retry: None,
            reconciliation_quantum,
            webhook_drain_work_budget,
        })
    }

    async fn run(mut self, shutdown: watch::Receiver<bool>) {
        self.run_until_stop(shutdown).await;
        // A targeted terminal/cursor completion is deliberately detached from
        // drain cancellation, but repository shutdown must still join it before
        // the supervisor can report that this task stopped cleanly. The durable
        // terminal handoff precedes cursor advancement, so aborting after this
        // grace period cannot leave an advanced cursor with a pending delivery.
        if timeout(
            WEBHOOK_TARGETED_COMPLETION_SHUTDOWN_TIMEOUT,
            self.settle_webhook_targeted_completion(),
        )
        .await
        .is_err()
        {
            if let Some(handle) = self.webhook_targeted_completion.take() {
                handle.abort_and_join().await;
            }
            tracing::error!(
                repository = %self.repository.as_str(),
                timeout_seconds = WEBHOOK_TARGETED_COMPLETION_SHUTDOWN_TIMEOUT.as_secs(),
                cause_code = "webhook_targeted_completion_shutdown_timed_out",
                "repository-watch aborted a retained targeted completion after its durable handoff deadline"
            );
        }
    }

    async fn run_until_stop(&mut self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        let prepared_webhook_retry = self.startup_webhook_retry.take();
        let startup_was_prepared = prepared_webhook_retry.is_some();
        let mut webhook_retry = prepared_webhook_retry.unwrap_or_default();
        if self.webhook_work.is_some() && !startup_was_prepared {
            let Some(outcome) = self.run_webhook_attempt_until_shutdown(&mut shutdown).await else {
                return;
            };
            if self.record_webhook_attempt(
                WebhookAttemptTrigger::Startup,
                outcome,
                &mut webhook_retry,
            ) {
                return;
            }
        }
        // The lookup acquires a pooled connection and queries, neither of which
        // this task bounds, and the supervisor joins every child before aborting
        // the set — an uncancellable await here would hold daemon termination
        // for as long as PostgreSQL stayed unresponsive.
        let Some(complete_poll_age) = run_until_shutdown(
            &mut shutdown,
            self.store.load_complete_poll_age(&self.repository),
        )
        .await
        else {
            return;
        };
        // Anchored after the lookup rather than before it. PostgreSQL measures
        // the age when the statement runs, so whatever time the lookup itself
        // spent is already subtracted once; anchoring ahead of it subtracts the
        // same delay a second time and brings the sweep forward by however long
        // a contended pool made this read take.
        let poll_schedule_started = Instant::now();
        let complete_poll_age = match complete_poll_age {
            Ok(age) => age,
            Err(error) => {
                tracing::warn!(
                    repository = %self.repository.as_str(),
                    cause_code = "repository_watch_startup_poll_cadence_unavailable",
                    error = ?error,
                    "repository-watch startup could not inspect its durable poll cadence; an immediate full poll remains scheduled"
                );
                None
            }
        };
        let mut next_poll =
            initial_poll_deadline(poll_schedule_started, self.interval, complete_poll_age);
        let mut consecutive_poll_preemptions = 0_u32;
        loop {
            if *shutdown.borrow() {
                return;
            }
            match next_repository_wake(
                &mut shutdown,
                next_poll,
                &webhook_retry,
                &mut self.webhook_work,
            )
            .await
            {
                RepositoryWatchWake::Stop => return,
                RepositoryWatchWake::Continue => {}
                RepositoryWatchWake::Poll => {
                    let cycle_started = Instant::now();
                    let drain = webhook_retry.poll_drain();
                    let mut drained = None;
                    let mut trailing_failure = None;
                    let webhook_interrupt = poll_webhook_interrupt(
                        DrainRetryBackoff::of(&webhook_retry),
                        consecutive_poll_preemptions,
                    );
                    let outcome = self
                        .run_preemptible_attempt_until_shutdown(
                            drain,
                            &mut shutdown,
                            &webhook_retry,
                            webhook_interrupt,
                            &mut drained,
                            &mut trailing_failure,
                        )
                        .await;
                    let result = match outcome {
                        PollAttemptWait::Completed(result) => result,
                        PollAttemptWait::Shutdown => {
                            // A cancelled full poll may own spawned PR fetches.
                            self.finish_cancelled_webhook_attempt().await;
                            return;
                        }
                        PollAttemptWait::Continue => {
                            let _ = self.poller.drain_fetches_bounded().await;
                            self.poller.invalidate_freshness();
                            continue;
                        }
                        PollAttemptWait::WebhookRetry => {
                            let _ = self.poller.drain_fetches_bounded().await;
                            self.poller.invalidate_freshness();
                            webhook_retry.consume();
                            let Some(outcome) =
                                self.run_webhook_attempt_until_shutdown(&mut shutdown).await
                            else {
                                return;
                            };
                            if self.record_webhook_attempt(
                                WebhookAttemptTrigger::Retry,
                                outcome,
                                &mut webhook_retry,
                            ) {
                                return;
                            }
                            tracing::warn!(
                                repository = %self.repository.as_str(),
                                "repository-watch webhook retry interrupted a full poll"
                            );
                            continue;
                        }
                        PollAttemptWait::Webhook => {
                            let _ = self.poller.drain_fetches_bounded().await;
                            // A cancelled child can publish freshness until its
                            // final await completes. Invalidate only after every
                            // child is joined so none can repopulate partial state.
                            self.poller.invalidate_freshness();
                            let Some(outcome) =
                                self.run_webhook_attempt_until_shutdown(&mut shutdown).await
                            else {
                                return;
                            };
                            if self.record_webhook_attempt(
                                WebhookAttemptTrigger::Wake,
                                outcome,
                                &mut webhook_retry,
                            ) {
                                return;
                            }
                            consecutive_poll_preemptions =
                                consecutive_poll_preemptions.saturating_add(1);
                            tracing::debug!(
                                repository = %self.repository.as_str(),
                                consecutive_preemptions = consecutive_poll_preemptions,
                                "repository-watch webhook work preempted a full poll"
                            );
                            // Return the still-due poll to the scheduler rather
                            // than resuming it in an uninterruptible mode. A
                            // bounded drain page re-arms its wake while backlog
                            // remains, so each fresh attempt drains another page
                            // before entering an interruptible provider sweep.
                            // Once the page observes no remainder it stops
                            // re-arming and the complete poll proceeds; until
                            // then the count above is what ends the cycle, since
                            // sustained ingress alone never does.
                            continue;
                        }
                    };
                    let metrics = self.poller.attempt_metrics();
                    // A drain a poll performed decides the backoff exactly as one
                    // a wake or a retry performed does. Reading only the poll's
                    // own result would erase that: a dispatch failure after
                    // terminal state would arm a retry with nothing left to
                    // drain, and a projection failure would not be counted.
                    match drained {
                        Some(outcome) => webhook_retry.update_after_poll_drain(
                            outcome,
                            trailing_failure,
                            Instant::now(),
                        ),
                        // The poll deferred its drain to an owed retry, or
                        // failed before reaching it.
                        None => {
                            if result.is_err() && self.webhook_work.is_some() {
                                webhook_retry.arm_if_unowed(Instant::now());
                            }
                        }
                    }
                    match &result {
                        Ok(()) => {
                            tracing::debug!(
                                repository = %self.repository.as_str(),
                                "repository-watch polling attempt completed"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                repository = %self.repository.as_str(),
                                cause_code = error.cause_code(),
                                request_count = metrics.requests,
                                poll_wire_bytes = metrics.poll_wire_bytes,
                                cached_resource_count = metrics.cached_resources,
                                cached_wire_bytes = metrics.cached_wire_bytes,
                                "repository-watch polling attempt failed closed"
                            );
                        }
                    }
                    if result.is_err_and(RepositoryWatchAttemptError::is_permanent) {
                        return;
                    }
                    // The poll committed, so the next one starts its own
                    // preemption budget.
                    consecutive_poll_preemptions = 0;
                    next_poll = next_cadence_deadline(cycle_started, self.interval, Instant::now());
                }
                RepositoryWatchWake::WebhookWork => {
                    let Some(outcome) =
                        self.run_webhook_attempt_until_shutdown(&mut shutdown).await
                    else {
                        return;
                    };
                    if self.record_webhook_attempt(
                        WebhookAttemptTrigger::Wake,
                        outcome,
                        &mut webhook_retry,
                    ) {
                        return;
                    }
                }
                RepositoryWatchWake::WebhookRetry => {
                    // Taking the retry spends its deadline, so what the attempt
                    // earns decides the next one. Without this the deadline
                    // stays elapsed and this arm, which precedes the poll arm,
                    // selects the same attempt again on the very next pass.
                    webhook_retry.consume();
                    let Some(outcome) =
                        self.run_webhook_attempt_until_shutdown(&mut shutdown).await
                    else {
                        return;
                    };
                    if self.record_webhook_attempt(
                        WebhookAttemptTrigger::Retry,
                        outcome,
                        &mut webhook_retry,
                    ) {
                        return;
                    }
                }
            }
        }
    }

    async fn prepare_startup(&mut self) -> bool {
        if self.webhook_work.is_none() {
            self.startup_webhook_retry = Some(WebhookDrainRetry::default());
            return false;
        }
        let outcome = self.run_webhook_attempt_with_payload_deadline().await;
        let mut webhook_retry = WebhookDrainRetry::default();
        let must_stop = self.record_webhook_attempt(
            WebhookAttemptTrigger::Startup,
            outcome,
            &mut webhook_retry,
        );
        self.startup_webhook_retry = Some(webhook_retry);
        must_stop
    }

    async fn run_webhook_attempt_until_shutdown(
        &mut self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Option<WebhookAttemptOutcome> {
        run_until_shutdown(shutdown, self.run_webhook_attempt_with_payload_deadline()).await
    }

    /// Applies one attempt's outcome to the drain backoff and reports it.
    ///
    /// Answers whether the repository task must stop, which a permanent failure
    /// requires whichever step produced it.
    fn record_webhook_attempt(
        &self,
        trigger: WebhookAttemptTrigger,
        outcome: WebhookAttemptOutcome,
        webhook_retry: &mut WebhookDrainRetry,
    ) -> bool {
        match outcome {
            WebhookAttemptOutcome::Completed => {
                webhook_retry.update_after(&Ok(()));
                tracing::debug!(
                    repository = %self.repository.as_str(),
                    trigger = trigger.as_str(),
                    "repository-watch webhook attempt drained durable work"
                );
            }
            WebhookAttemptOutcome::DrainFailed(error) => {
                webhook_retry.update_after(&Err(error));
                tracing::error!(
                    repository = %self.repository.as_str(),
                    trigger = trigger.as_str(),
                    cause_code = error.cause_code(),
                    retry_seconds = webhook_retry.delay().as_secs(),
                    "repository-watch webhook drain failed; retry scheduled"
                );
            }
            WebhookAttemptOutcome::DrainedThenFailed(error) => {
                webhook_retry.arm_follow_up(Instant::now());
                tracing::error!(
                    repository = %self.repository.as_str(),
                    trigger = trigger.as_str(),
                    cause_code = error.cause_code(),
                    retry_seconds = webhook_retry.delay().as_secs(),
                    "repository-watch webhook attempt drained but its dispatch work failed"
                );
            }
            WebhookAttemptOutcome::FailedBeforeDrain(error) => {
                webhook_retry.arm_if_unowed(Instant::now());
                tracing::error!(
                    repository = %self.repository.as_str(),
                    trigger = trigger.as_str(),
                    cause_code = error.cause_code(),
                    retry_seconds = webhook_retry.delay().as_secs(),
                    "repository-watch webhook attempt failed before its drain; retry scheduled"
                );
            }
        }
        outcome
            .failure()
            .is_some_and(RepositoryWatchAttemptError::is_permanent)
    }

    /// Deletes expired terminal payload bytes after a successful full poll, at
    /// most once per purge interval, starting with the first poll after boot,
    /// independently of whether the surrounding attempt reports a deferred
    /// drain failure.
    ///
    /// A purge failure never fails the watch: the deletion covers only
    /// already-terminal deliveries, so it is retried on the next successful
    /// poll rather than propagated.
    async fn maybe_purge_expired_payloads(&mut self) {
        let mut next = self.payload_purge.next.lock().await;
        if Instant::now() < *next {
            return;
        }
        match self.webhook_store.purge_expired_payloads().await {
            Ok(purged) => {
                *next = Instant::now() + WEBHOOK_PAYLOAD_PURGE_INTERVAL;
                if purged > 0 {
                    tracing::info!(
                        repository = %self.repository.as_str(),
                        purged,
                        "expired webhook payload bytes deleted"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    repository = %self.repository.as_str(),
                    error = ?error,
                    "expired webhook payload purge failed"
                );
            }
        }
    }

    /// One full polling attempt, draining durable webhook work as part of its
    /// own sequence unless a retry already owes that drain.
    ///
    /// The drain is a step of this attempt, so running it while the backoff is
    /// owed would repeat at the poll cadence exactly the work the backoff
    /// exists to space out. The owed retry performs it instead.
    /// Reports the drain it performed through `drained`, so the caller can
    /// apply it to the backoff. A poll's own result cannot stand in for it:
    /// polling fails for reasons the drain never saw, and a drain can fail
    /// after every delivery it visited reached terminal state. Reports the
    /// cutoff and dispatch work that ran after that drain through
    /// `trailing_failure`, which the drain's outcome likewise cannot carry:
    /// nothing is left pending to wake a later attempt for it.
    #[cfg(test)]
    async fn run_attempt(
        &mut self,
        drain: WebhookDrain,
        drained: &mut Option<WebhookDrainOutcome>,
        trailing_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        self.poller.begin_attempt();
        let result = async {
            let accelerated = self
                .run_attempt_prelude(drain, drained, trailing_failure)
                .await?;
            let prepared = self.prepare_complete_poll().await?;
            self.finish_attempt(drain, accelerated, prepared, drained, trailing_failure)
                .await
        }
        .await;
        if result.is_err() {
            // Any failed attempt may leave published entries tied to an older
            // durable cursor, regardless of which step failed.
            self.poller.invalidate_freshness();
        }
        result
    }

    /// Runs one due poll with only its provider sweep cancellable by webhook work.
    ///
    /// Rule activation, dispatch, webhook projection, and cursor commit all
    /// remain outside that cancellation region. Cancelling the provider sweep
    /// therefore abandons only read-side work and its spawned fetches. Both an
    /// admission wake and an owed retry can interrupt it; backoff suppresses
    /// only admission, because a retry deadline must remain authoritative.
    async fn run_preemptible_attempt_until_shutdown(
        &mut self,
        drain: WebhookDrain,
        shutdown: &mut watch::Receiver<bool>,
        webhook_retry: &WebhookDrainRetry,
        webhook_interrupt: WebhookPollInterrupt,
        drained: &mut Option<WebhookDrainOutcome>,
        trailing_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> PollAttemptWait<Result<(), RepositoryWatchAttemptError>> {
        self.poller.begin_attempt();
        match drain {
            WebhookDrain::Run => observe_webhook_work_before_drain(&mut self.webhook_work),
            WebhookDrain::Deferred => {}
        }
        let Some(prelude) = run_until_shutdown(
            shutdown,
            self.run_attempt_prelude(drain, drained, trailing_failure),
        )
        .await
        else {
            self.poller.invalidate_freshness();
            return PollAttemptWait::Shutdown;
        };
        let accelerated = match prelude {
            Ok(accelerated) => accelerated,
            Err(error) => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::Completed(Err(error));
            }
        };
        let webhook_interrupt = match &accelerated {
            Ok(()) => webhook_interrupt,
            Err(_) => WebhookPollInterrupt::Suppressed,
        };

        let mut webhook_work = self.webhook_work.take();
        let outcome = await_poll_or_interrupt(
            self.prepare_complete_poll(),
            shutdown,
            webhook_retry,
            &mut webhook_work,
            webhook_interrupt,
        )
        .await;
        self.webhook_work = webhook_work;
        let prepared = match outcome {
            PollAttemptWait::Completed(Ok(prepared)) => prepared,
            PollAttemptWait::Completed(Err(error)) => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::Completed(Err(error));
            }
            PollAttemptWait::Continue => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::Continue;
            }
            PollAttemptWait::Shutdown => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::Shutdown;
            }
            PollAttemptWait::Webhook => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::Webhook;
            }
            PollAttemptWait::WebhookRetry => {
                self.poller.invalidate_freshness();
                return PollAttemptWait::WebhookRetry;
            }
        };

        let Some(result) = run_until_shutdown(
            shutdown,
            self.finish_attempt(drain, accelerated, prepared, drained, trailing_failure),
        )
        .await
        else {
            self.poller.invalidate_freshness();
            return PollAttemptWait::Shutdown;
        };
        if result.is_err() {
            self.poller.invalidate_freshness();
        }
        PollAttemptWait::Completed(result)
    }

    /// Performs the mutation-bearing work before the complete provider sweep.
    async fn run_attempt_prelude(
        &mut self,
        drain: WebhookDrain,
        drained: &mut Option<WebhookDrainOutcome>,
        trailing_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> Result<Result<(), RepositoryWatchAttemptError>, RepositoryWatchAttemptError> {
        // Any timed-out projection drain may have left pending work before it
        // installed delivery-specific settlement state. Do not let projection
        // backoff turn that timeout into a cursor-advancing deferred poll.
        if drain == WebhookDrain::Deferred && self.webhook_drain_timed_out {
            return Err(RepositoryWatchAttemptError::WebhookDrainTimedOut);
        }
        // A deferred drain may still own a targeted terminal/cursor completion,
        // or a prior settlement may not know whether its terminal write
        // committed. Do not let a complete poll advance the cursor until the
        // owed drain settles that durable state.
        if drain == WebhookDrain::Deferred
            && (self.webhook_targeted_completion.is_some()
                || self.webhook_terminal_ambiguous.is_some())
        {
            return Err(RepositoryWatchAttemptError::Persistence);
        }
        if !self.rules_activated {
            self.activate_rules().await?;
            self.rules_activated = true;
        }
        // A leading reconciliation failure is recorded and not
        // propagated: the pre-poll drain owes none of that work, and the
        // same steps run again after the poll, where a repeated failure
        // reports through `trailing_failure`.
        if let Err(error) = async {
            self.process_cutoffs().await?;
            self.process_dispatches().await
        }
        .await
        {
            tracing::warn!(
                repository = %self.repository.as_str(),
                cause_code = error.cause_code(),
                "repository-watch leading reconciliation failed; the poll and its drains continue"
            );
            // Recorded before the poll so an attempt that exits ahead of
            // the trailing pass still owes the dispatch its follow-up; the
            // trailing pass clears it when it meets the obligation.
            *trailing_failure = Some(error);
        }
        // Deliveries already admitted are projected before this poll runs.
        // A poll that observes the same transition would otherwise advance
        // the cursor past them, and every one of them would then apply to
        // state that already contains it and record nothing.
        //
        // The pre-poll failure is reported but not propagated here:
        // acceleration failing must not cancel the reconciliation sweep, or
        // one delivery whose targeted request keeps failing would abort
        // every scheduled poll.
        let accelerated = match drain {
            WebhookDrain::Run => {
                let outcome = self.process_webhook_deliveries_with_timeout().await;
                *drained = Some(outcome);
                if self.webhook_drain_timed_out && outcome.blocks_complete_poll_after_timeout() {
                    // A complete poll after a cancelled pre-drain could advance
                    // the durable cursor past the delivery that remains pending.
                    // Return to the scheduler so retained drain state settles
                    // before another complete sweep can commit.
                    return Err(RepositoryWatchAttemptError::WebhookDrainTimedOut);
                }
                if self.webhook_terminal_ambiguous.is_some() {
                    // The targeted terminal write may have committed or rolled
                    // back. Until a later drain settles that durable state, a
                    // complete poll must not advance the cursor past it.
                    return Err(RepositoryWatchAttemptError::Persistence);
                }
                outcome.failure().map_or(Ok(()), Err)
            }
            WebhookDrain::Deferred => Ok(()),
        };
        match drained.as_ref() {
            Some(WebhookDrainOutcome::ProjectionFailed(error)) => tracing::warn!(
                repository = %self.repository.as_str(),
                cause_code = error.cause_code(),
                "repository-watch webhook pre-poll drain failed; polling continues"
            ),
            Some(WebhookDrainOutcome::DispatchFailedAfterTerminal(error)) => tracing::warn!(
                repository = %self.repository.as_str(),
                cause_code = error.cause_code(),
                "repository-watch webhook pre-poll dispatch work failed after terminal records; polling continues"
            ),
            _ => {}
        }
        Ok(accelerated)
    }

    async fn finish_attempt(
        &mut self,
        drain: WebhookDrain,
        accelerated: Result<(), RepositoryWatchAttemptError>,
        prepared: PreparedCompletePoll,
        drained: &mut Option<WebhookDrainOutcome>,
        trailing_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        self.commit_complete_poll(prepared).await?;
        // Keyed on the poll succeeding rather than the whole attempt, so a
        // deferred drain failure cannot starve retention while one delivery
        // persistently fails.
        self.maybe_purge_expired_payloads().await;
        // Only once the pre-poll drain's projections succeeded: repeating
        // a projection-failed drain would spend the same provider and
        // database work twice inside one attempt, ahead of the backoff
        // that exists to space exactly that work out. A terminal dispatch
        // failure does not gate it — projection succeeded, and the
        // post-poll step still owes newly admitted deliveries their drain.
        let pre_drain_projection_failed = matches!(
            drained.as_ref(),
            Some(WebhookDrainOutcome::ProjectionFailed(_))
        );
        if drain == WebhookDrain::Run && !pre_drain_projection_failed {
            let outcome = self.process_webhook_deliveries_with_timeout().await;
            if let Some(error) = outcome.failure() {
                *drained = Some(outcome);
                return Err(error);
            }
            // A successful second drain does not erase a terminal dispatch
            // failure the first one reported: the caller's retry state
            // still owes that dispatch its follow-up.
            if !matches!(
                drained.as_ref(),
                Some(WebhookDrainOutcome::DispatchFailedAfterTerminal(_))
            ) {
                *drained = Some(outcome);
            }
        }
        accelerated?;
        // Both trailing steps report through `trailing_failure` rather than
        // escaping, because a drain that has already committed its work
        // owns neither of them. Letting the cutoff `?` out would leave the
        // caller reading a bare `Drained`: it would clear the deadline the
        // committed work needs and skip the dispatch step below, so that
        // work would wait for an unrelated admission or the next poll
        // instead of the follow-up this failure is owed.
        if let Err(error) = self.process_cutoffs().await {
            *trailing_failure = Some(error);
            return Err(error);
        }
        match self.process_dispatches().await {
            Ok(()) => {
                // The trailing pass met any leading dispatch obligation, so
                // the recorded leading failure no longer owes a follow-up.
                *trailing_failure = None;
                Ok(())
            }
            Err(error) => {
                *trailing_failure = Some(error);
                Err(error)
            }
        }
    }

    async fn run_webhook_attempt(&mut self, drain_deadline: Duration) -> WebhookAttemptOutcome {
        self.poller.begin_attempt();
        self.webhook_attempt_phase = WebhookAttemptPhase::BeforeDrain;
        let outcome = async {
            if !self.rules_activated {
                if let Err(error) = self.activate_rules().await {
                    return WebhookAttemptOutcome::FailedBeforeDrain(error);
                }
                self.rules_activated = true;
            }
            // Leading reconciliation failures do not gate the drain: pending
            // deliveries owe none of that work, and gating here would leave
            // every newly admitted delivery waiting on unrelated dispatch
            // trouble. The failure is preserved and reported once the drain
            // has run.
            let leading_failure = if let Err(error) = self.process_cutoffs().await {
                Some(error)
            } else {
                self.process_dispatches().await.err()
            };
            self.webhook_attempt_phase = WebhookAttemptPhase::Drain;
            let drained = self
                .process_webhook_deliveries_with_deadline(drain_deadline)
                .await;
            self.webhook_attempt_phase = WebhookAttemptPhase::AfterDrain;
            match drained {
                WebhookDrainOutcome::Drained => {}
                WebhookDrainOutcome::ProjectionFailed(error) => {
                    return WebhookAttemptOutcome::DrainFailed(error);
                }
                // The delivery is terminal and will not be reloaded, so this
                // failure is the surrounding dispatch work's, not the drain's.
                WebhookDrainOutcome::DispatchFailedAfterTerminal(error) => {
                    return WebhookAttemptOutcome::DrainedThenFailed(error);
                }
            }
            if let Some(error) = leading_failure {
                // The drain committed its work; the leading failure is the
                // surrounding dispatch work's, so it follows up rather than
                // advancing the drain backoff.
                return WebhookAttemptOutcome::DrainedThenFailed(error);
            }
            if let Err(error) = self.process_cutoffs().await {
                return WebhookAttemptOutcome::DrainedThenFailed(error);
            }
            match self.process_dispatches().await {
                Ok(()) => WebhookAttemptOutcome::Completed,
                Err(error) => WebhookAttemptOutcome::DrainedThenFailed(error),
            }
        }
        .await;
        if outcome.failure().is_some() {
            self.poller.invalidate_freshness();
        }
        outcome
    }

    async fn run_webhook_attempt_with_payload_deadline(&mut self) -> WebhookAttemptOutcome {
        self.webhook_drain_timed_out = false;
        let deadlines = match self.webhook_attempt_deadlines().await {
            Ok(deadlines) => deadlines,
            Err(error) => {
                tracing::error!(
                    repository = %self.repository.as_str(),
                    cause_code = RepositoryWatchAttemptError::Persistence.cause_code(),
                    cause = %error,
                    "repository-watch webhook attempt could not size its durable cursor payload"
                );
                return WebhookAttemptOutcome::FailedBeforeDrain(
                    RepositoryWatchAttemptError::Persistence,
                );
            }
        };
        self.run_webhook_attempt_with_deadlines(deadlines).await
    }

    async fn run_webhook_attempt_with_deadlines(
        &mut self,
        deadlines: WebhookAttemptDeadlines,
    ) -> WebhookAttemptOutcome {
        match timeout(deadlines.attempt, self.run_webhook_attempt(deadlines.drain)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let phase = self.webhook_attempt_phase;
                let dispatch_in_flight = self.finish_cancelled_webhook_attempt().await;
                // The drain's own deadline records this; the enclosing attempt
                // deadline can cancel the same drain — the reconciliation ahead
                // of it spends the margin between the two bounds — and left
                // unrecorded the fence in `run_attempt_prelude` never fires, so
                // the following complete poll commits a cursor past a delivery
                // this cancellation left pending.
                if phase.cancellation_fences_complete_poll() {
                    self.webhook_drain_timed_out = true;
                }
                let error = RepositoryWatchAttemptError::WebhookAttemptTimedOut;
                tracing::error!(
                    repository = %self.repository.as_str(),
                    timeout_seconds = deadlines.attempt.as_secs(),
                    drain_timeout_seconds = deadlines.drain.as_secs(),
                    cursor_payload_bytes = deadlines.cursor_payload_bytes,
                    cancelled_phase = phase.label(),
                    cause_code = error.cause_code(),
                    "repository-watch webhook attempt exceeded its deadline"
                );
                // Cancellation carries no failure of its own, so the cancelled
                // step decides the outcome: only a drain the deadline
                // interrupted has earned the growing projection backoff.
                phase.cancelled_outcome(error, dispatch_in_flight)
            }
        }
    }

    /// Settles any retained cursor commit, then sizes the resulting durable
    /// cursor under one fixed bound.
    ///
    /// The deadlines this returns are derived from the read, so they cannot
    /// bound it. Without this the sizing read would precede every attempt's
    /// `timeout`, leaving a stalled database able to hold the serialized
    /// repository task — and, through startup's unbounded join, the daemon —
    /// with no deadline to report.
    async fn webhook_attempt_deadlines(
        &mut self,
    ) -> Result<WebhookAttemptDeadlines, WebhookCursorSizingError> {
        timeout(WEBHOOK_CURSOR_SIZING_TIMEOUT, async {
            if let Some(settlement) = self.settle_webhook_targeted_completion().await {
                settlement.map_err(WebhookCursorSizingError::Settlement)?;
            }
            load_webhook_attempt_deadlines_unbounded(&self.store, &self.repository).await
        })
        .await
        .map_err(|_| WebhookCursorSizingError::TimedOut)?
    }

    /// Performs the cleanup every cancelled webhook attempt owes its successor.
    ///
    /// Cancellation itself must not wedge the repository task, so the poller's
    /// shared child fetch set is joined under its own bound; a later attempt
    /// drains that same set before it can spawn, preserving the
    /// no-interleaving policy. Either deadline can cancel a projected terminal
    /// write, so the carried shadow is settled here rather than at one call
    /// site.
    ///
    /// Reports whether the cancellation landed in the drain's post-terminal
    /// dispatch window and clears that marker, because nothing else does: a flag
    /// left set outlives its attempt and makes a later drain timeout report a
    /// dispatch failure for a projection that never terminalized, clearing the
    /// projection backoff and re-enabling poll drains.
    async fn finish_cancelled_webhook_attempt(&mut self) -> bool {
        let dispatch_in_flight = std::mem::take(&mut self.webhook_dispatch_in_flight);
        if timeout(
            WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT,
            self.poller.drain_fetches(),
        )
        .await
        .is_err()
        {
            tracing::error!(
                repository = %self.repository.as_str(),
                timeout_seconds = WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT.as_secs(),
                cause_code = "webhook_cancelled_fetch_drain_timed_out",
                "repository-watch cancelled fetch cleanup exceeded its deadline"
            );
        }
        self.poller.invalidate_freshness();
        // Only a projected terminal write can make the carried shadow
        // ambiguous. A targeted cursor commit is retained separately and
        // settled before another drain, so its delivery keeps the pre-commit
        // shadow needed to reproduce its projections.
        if let Some(key) = self.webhook_projected_terminal_in_flight.take() {
            self.webhook_shadow = None;
            self.webhook_shadow_superseded = false;
            self.webhook_terminal_ambiguous = Some(key);
        }
        dispatch_in_flight
    }

    async fn process_webhook_deliveries(&mut self) -> WebhookDrainOutcome {
        self.process_webhook_deliveries_with_budget(self.webhook_drain_work_budget)
            .await
    }

    async fn process_webhook_deliveries_with_budget(
        &mut self,
        work_budget: Option<Duration>,
    ) -> WebhookDrainOutcome {
        self.webhook_drain_first_failure = None;
        self.webhook_drain_projection_failure = None;
        if let Some(Err(error)) = self.settle_webhook_targeted_completion().await {
            self.webhook_drain_first_failure = Some(error);
            return WebhookDrainOutcome::ProjectionFailed(error);
        }
        if let Some(key) = self.webhook_terminal_ambiguous
            && self
                .webhook_store
                .terminal_disposition_exists(key)
                .await
                .is_ok_and(|exists| exists)
        {
            self.webhook_terminal_ambiguous = None;
        }
        let Ok(page_size) = RepoWatchWebhookPendingPageSize::try_new(WEBHOOK_PENDING_PAGE_SIZE)
        else {
            return WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Persistence);
        };
        let started_at = Instant::now();
        let mut deferred: HashSet<RepoWatchWebhookDeliveryKey> = HashSet::new();
        let mut first_failure: Option<RepositoryWatchAttemptError> = None;
        let mut dispatch_failure: Option<RepositoryWatchAttemptError> = None;
        // The chronological first failure of either kind, because the drain's
        // error-level record names the first cause while the outcome keeps
        // projection priority for backoff classification.
        let mut chronological_first: Option<RepositoryWatchAttemptError> = None;
        let mut pages = 0_usize;
        // Every receipt this drain has visited, so a deferred head cannot be
        // reloaded ahead of what follows it. A page bounded by bytes can hold
        // nothing but that head, which would otherwise leave every later
        // receipt permanently unreachable.
        let mut after_receipt: Option<NonZeroU64> = None;
        'drain: loop {
            let deliveries = match self
                .webhook_store
                .load_pending(&self.repository, page_size, after_receipt)
                .await
            {
                Ok(deliveries) => deliveries,
                Err(error) => {
                    // The same error-level record a per-delivery failure earns:
                    // a page this drain could not read is a drain failure, and
                    // the poll caller's own record is a warning about polling.
                    // The record names the chronological first cause when a
                    // delivery already failed on an earlier page; the outcome
                    // keeps the page-load error for classification.
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = chronological_first
                            .unwrap_or(RepositoryWatchAttemptError::Persistence)
                            .cause_code(),
                        cause = %error,
                        "repository-watch webhook drain could not load its pending page"
                    );
                    return WebhookDrainOutcome::ProjectionFailed(
                        RepositoryWatchAttemptError::Persistence,
                    );
                }
            };
            if deliveries.is_empty() {
                if deferred.is_empty() {
                    // Nothing is pending at all, so a poll that left the shadow
                    // in place hands it over now. There is no await between the
                    // observation and the replacement.
                    self.replace_superseded_webhook_shadow();
                }
                break;
            }
            let mut page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
            for delivery in &deliveries {
                after_receipt = Some(delivery.receipt().sequence());
                if deferred.contains(&delivery.key()) {
                    continue;
                }
                let terminalized = match self
                    .process_webhook_delivery(delivery, &mut page, &mut dispatch_failure)
                    .await
                {
                    Ok(()) => {
                        if self.webhook_terminal_ambiguous == Some(delivery.key()) {
                            self.webhook_terminal_ambiguous = None;
                        }
                        true
                    }
                    Err(error) => {
                        if error.stops_webhook_page() {
                            tracing::warn!(
                                repository = %self.repository.as_str(),
                                hook_id = delivery.key().hook_id().get(),
                                delivery_id = %delivery.key().delivery_id(),
                                cause_code = error.cause_code(),
                                "repository-wide webhook refresh failure stopped the current drain page"
                            );
                            first_failure.get_or_insert(error);
                            chronological_first.get_or_insert(error);
                            break 'drain;
                        }
                        // A delivery whose targeted refresh cannot succeed stays
                        // the oldest pending row, so failing the whole drain on
                        // it would starve every later receipt sequence forever.
                        // The shadow baseline is left alone: this delivery
                        // recorded nothing, and discarding what earlier ones
                        // projected would supersede their dependents.
                        tracing::warn!(
                            repository = %self.repository.as_str(),
                            hook_id = delivery.key().hook_id().get(),
                            delivery_id = %delivery.key().delivery_id(),
                            cause_code = error.cause_code(),
                            "webhook delivery deferred so later receipts drain"
                        );
                        deferred.insert(delivery.key());
                        if first_failure.is_none() {
                            first_failure = Some(error);
                            self.webhook_drain_projection_failure = Some(error);
                        }
                        false
                    }
                };
                if chronological_first.is_none() {
                    chronological_first = first_failure.or(dispatch_failure);
                    self.webhook_drain_first_failure = chronological_first;
                }
                if terminalized && work_budget.is_some_and(|budget| started_at.elapsed() >= budget)
                {
                    self.request_webhook_drain_continuation();
                    tracing::info!(
                        repository = %self.repository.as_str(),
                        work_budget_seconds = work_budget.map_or(0, |budget| budget.as_secs()),
                        cause_code = "webhook_projection_drain_work_budget_exhausted",
                        "progressing repository-watch webhook drain yielded before its deadline"
                    );
                    break 'drain;
                }
            }
            pages += 1;
            if pages >= WEBHOOK_DRAIN_PAGE_LIMIT {
                // More may remain behind this page, so the shadow this drain
                // advanced still speaks for deliveries the next one will read.
                self.request_webhook_drain_continuation();
                break;
            }
        }
        // A projection failure outranks a dispatch one: it is the failure a
        // later drain can still retry, and it is what the backoff spaces out.
        // The error-level record instead names the chronological first cause,
        // which the drain contract promises an error-only sink.
        match first_failure.or(dispatch_failure) {
            // Emitted here rather than by the caller because all three callers
            // — startup, wake, and retry — reach the drain through this one
            // function, and a full poll's own failure record is a warning about
            // polling. An error-only sink would otherwise learn nothing about a
            // drain that failed inside a poll.
            Some(error) => {
                tracing::error!(
                    repository = %self.repository.as_str(),
                    cause_code = chronological_first.unwrap_or(error).cause_code(),
                    "repository-watch webhook drain page retained a failed delivery"
                );
                match first_failure {
                    Some(error) => WebhookDrainOutcome::ProjectionFailed(error),
                    None => WebhookDrainOutcome::DispatchFailedAfterTerminal(error),
                }
            }
            None => WebhookDrainOutcome::Drained,
        }
    }

    async fn process_webhook_deliveries_with_timeout(&mut self) -> WebhookDrainOutcome {
        self.webhook_drain_timed_out = false;
        let deadlines = match self.webhook_attempt_deadlines().await {
            Ok(deadlines) => deadlines,
            Err(error) => {
                tracing::error!(
                    repository = %self.repository.as_str(),
                    cause_code = RepositoryWatchAttemptError::Persistence.cause_code(),
                    cause = %error,
                    "repository-watch webhook drain could not size its durable cursor payload"
                );
                return WebhookDrainOutcome::ProjectionFailed(
                    RepositoryWatchAttemptError::Persistence,
                );
            }
        };
        self.process_webhook_deliveries_with_deadline(deadlines.drain)
            .await
    }

    async fn process_webhook_deliveries_with_deadline(
        &mut self,
        deadline: Duration,
    ) -> WebhookDrainOutcome {
        self.webhook_drain_timed_out = false;
        match timeout(deadline, self.process_webhook_deliveries()).await {
            Ok(outcome) => {
                self.webhook_drain_first_failure = None;
                self.webhook_drain_projection_failure = None;
                outcome
            }
            Err(_) => {
                self.webhook_drain_timed_out = true;
                // A future implementation may use the poller's bounded child
                // fetch set while hydrating a delivery. The shared cancellation
                // cleanup bounds that join and settles the carried shadow, so
                // the poller's next attempt drains the same shared set before
                // it can spawn, preserving the no-interleaving policy.
                let dispatch_in_flight = self.finish_cancelled_webhook_attempt().await;
                let first_failure = self.webhook_drain_first_failure.take();
                let projection_failure = self.webhook_drain_projection_failure.take();
                if let Some(first_failure) = first_failure {
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = first_failure.cause_code(),
                        "repository-watch webhook drain retained an earlier failure before its deadline"
                    );
                }
                let error = RepositoryWatchAttemptError::WebhookDrainTimedOut;
                tracing::error!(
                    repository = %self.repository.as_str(),
                    timeout_seconds = deadline.as_secs(),
                    cause_code = error.cause_code(),
                    "repository-watch webhook drain exceeded its attempt deadline"
                );
                if let Some(projection_failure) = projection_failure {
                    WebhookDrainOutcome::ProjectionFailed(projection_failure)
                } else if dispatch_in_flight {
                    WebhookDrainOutcome::DispatchFailedAfterTerminal(first_failure.unwrap_or(error))
                } else {
                    WebhookDrainOutcome::ProjectionFailed(error)
                }
            }
        }
    }

    /// Hands the shadow baseline over to the cursor a full poll committed.
    ///
    /// Called only where the pending page has just been observed empty, with no
    /// await between the two, so a delivery admitted after that observation is
    /// newer than the committed cursor and correctly reloads from it.
    fn replace_superseded_webhook_shadow(&mut self) {
        if self.webhook_shadow_superseded {
            self.webhook_shadow = None;
            self.webhook_shadow_superseded = false;
        }
    }

    /// Seeds the shadow baseline from the durable cursor when the repository
    /// task does not already carry one.
    ///
    /// Reports whether it had to, because a delivery projected against a
    /// freshly seeded baseline is the accepted cross-drain gap and carries that
    /// cause on its projections.
    async fn seed_webhook_shadow(
        &mut self,
        pending: &PendingRepoWatchWebhookDelivery,
    ) -> Result<bool, RepositoryWatchAttemptError> {
        if self.webhook_shadow.is_some() {
            return Ok(false);
        }
        let loaded = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|error| {
                tracing::warn!(
                    repository = %self.repository.as_str(),
                    hook_id = pending.key().hook_id().get(),
                    delivery_id = %pending.key().delivery_id(),
                    cause_code = RepositoryWatchAttemptError::Persistence.cause_code(),
                    cause = %error,
                    "repository-watch webhook drain could not read the cursor its shadow seeds from"
                );
                RepositoryWatchAttemptError::Persistence
            })?;
        // Warning level and delivery-keyed, matching what repo-watch requires of
        // an individual failure record; the one error-level record stays the
        // drain's own first-cause summary, so an error-only sink sees one
        // incident.
        //
        // A repository with no cursor row reports the same sanitized cause as a
        // failed read, and the read itself succeeded, so the absence is the
        // whole evidence a deferred delivery leaves behind.
        let Some(cursor) = loaded else {
            tracing::warn!(
                repository = %self.repository.as_str(),
                hook_id = pending.key().hook_id().get(),
                delivery_id = %pending.key().delivery_id(),
                cause_code = RepositoryWatchAttemptError::Persistence.cause_code(),
                cause = "the repository has no durable cursor row",
                "repository-watch webhook drain has no cursor for its shadow to seed from"
            );
            return Err(RepositoryWatchAttemptError::Persistence);
        };
        self.webhook_shadow = Some(WebhookShadowBaseline::from_cursor(&cursor));
        Ok(true)
    }

    /// Re-arms this repository's own webhook wake so a bounded drain resumes
    /// after the scheduler has had its turn.
    fn request_webhook_drain_continuation(&self) {
        if let Some(nudge) = &self.webhook_nudge {
            // The wake coalesces, so one already unobserved carries this too,
            // and a send fails only once the worker's own receiver is gone.
            let _ = nudge.send(());
        }
    }

    /// Projects one delivery to terminal state.
    ///
    /// A failure of the dispatch work that follows a terminal disposition is
    /// reported through `dispatch_failure` rather than returned, because the
    /// delivery is durable by then: draining again cannot retry it, and the
    /// drain's backoff must not treat it as a projection that failed.
    async fn process_webhook_delivery(
        &mut self,
        pending: &PendingRepoWatchWebhookDelivery,
        page: &mut RepoWatchTargetedRefreshCoalescerV1,
        dispatch_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let delivery = RepoWatchWebhookDeliveryV1::new(RepoWatchWebhookDeliveryV1Input {
            repository: pending.repository().clone(),
            hook_id: pending.key().hook_id(),
            delivery_id: pending.key().delivery_id(),
            event: pending.event_name().to_owned(),
            action: pending.action_name().map(str::to_owned),
            receipt_sequence: pending.receipt().sequence(),
            body_digest: *pending.body_digest(),
        });
        let mapping = match map_repo_watch_webhook_delivery_v1(&delivery, pending.body()) {
            Ok(mapping) => mapping,
            Err(error) => {
                self.record_webhook_terminal(
                    pending,
                    Vec::new(),
                    RepoWatchWebhookDisposition::Quarantined,
                    Some(webhook_mapping_error_code(error)),
                )
                .await?;
                return Ok(());
            }
        };
        match mapping {
            RepoWatchWebhookMappingV1::Ignored(reason) => {
                let outcome_code = webhook_ignored_reason_code(reason);
                tracing::debug!(
                    repository = %self.repository.as_str(),
                    hook_id = pending.key().hook_id().get(),
                    delivery_id = %pending.key().delivery_id(),
                    event = pending.event_name(),
                    action = pending.action_name(),
                    outcome_code,
                    "signature-valid webhook delivery is outside the mapped set"
                );
                self.record_webhook_terminal(
                    pending,
                    Vec::new(),
                    RepoWatchWebhookDisposition::Ignored,
                    Some(outcome_code),
                )
                .await
            }
            RepoWatchWebhookMappingV1::MappedNoChange(reason) => {
                self.record_webhook_terminal(
                    pending,
                    Vec::new(),
                    RepoWatchWebhookDisposition::Ignored,
                    Some(webhook_no_change_code(reason)),
                )
                .await
            }
            RepoWatchWebhookMappingV1::Patch(patch) if self.webhook_primary => {
                self.process_primary_webhook_patch(pending, patch, page, dispatch_failure)
                    .await
            }
            RepoWatchWebhookMappingV1::Patch(patch) => {
                let cause = self
                    .seed_webhook_shadow(pending)
                    .await?
                    .then_some(RepoWatchWebhookParityCauseV1::CrossDrainShadowGap);
                let Some(shadow) = self.webhook_shadow.clone() else {
                    return Err(RepositoryWatchAttemptError::Persistence);
                };
                let applied =
                    match apply_repo_watch_observation_patch_v1(&shadow.observation, &patch) {
                        Ok(applied) => applied,
                        Err(_) => {
                            self.record_webhook_terminal(
                                pending,
                                Vec::new(),
                                RepoWatchWebhookDisposition::Quarantined,
                                Some("patch_incoherent"),
                            )
                            .await?;
                            return Ok(());
                        }
                    };
                match applied {
                    RepoWatchObservationApplyV1::DuplicateState => {
                        self.record_webhook_terminal(
                            pending,
                            Vec::new(),
                            RepoWatchWebhookDisposition::DuplicateState,
                            None,
                        )
                        .await
                    }
                    RepoWatchObservationApplyV1::Superseded => {
                        self.record_webhook_terminal(
                            pending,
                            Vec::new(),
                            RepoWatchWebhookDisposition::Superseded,
                            None,
                        )
                        .await
                    }
                    RepoWatchObservationApplyV1::Ignored(reason) => {
                        // Projecting nothing is the point: polling could never
                        // produce this fact, so a projection would stand as a
                        // webhook-only parity row nothing can ever match.
                        self.record_webhook_terminal(
                            pending,
                            Vec::new(),
                            RepoWatchWebhookDisposition::Ignored,
                            Some(webhook_ignored_reason_code(reason)),
                        )
                        .await
                    }
                    RepoWatchObservationApplyV1::Applied(observation) => {
                        let (projections, identity_frontier) = shadow_event_projections(
                            &self.repository,
                            &shadow,
                            &observation,
                            cause,
                        )?;
                        self.record_webhook_terminal(
                            pending,
                            projections,
                            RepoWatchWebhookDisposition::Projected,
                            None,
                        )
                        .await?;
                        // The shadow advances only once the disposition is
                        // durable, so a retry after a failed record derives the
                        // same projections rather than an empty duplicate.
                        // Advancing it also clears any supersession a poll left
                        // pending: the baseline now carries facts newer than
                        // that cursor, so handing it over would discard them.
                        self.webhook_shadow = Some(WebhookShadowBaseline {
                            observation,
                            identity_frontier,
                            merged_pull_request_baselines: shadow
                                .merged_pull_request_baselines
                                .clone(),
                        });
                        self.webhook_shadow_superseded = false;
                        Ok(())
                    }
                    RepoWatchObservationApplyV1::NeedsTargetedRefresh {
                        observation,
                        refreshes,
                    } => {
                        let (mut projections, identity_frontier) = shadow_event_projections(
                            &self.repository,
                            &shadow,
                            &observation,
                            cause,
                        )?;
                        let unissued = page.unissued(&refreshes);
                        // The provider query runs before anything is recorded, so
                        // a transient fetch failure leaves this delivery pending
                        // and retryable instead of terminal with a targeted query
                        // that never happened.
                        let prepared = match self.prepare_targeted_refresh(&unissued).await? {
                            PreparedTargetedRefreshOutcome::SupersededTarget => {
                                // The provider proved a targeted head stale
                                // before anything was recorded: the derived
                                // projections describe state the repository has
                                // already left, so the delivery is superseded
                                // and the shadow stays exactly as it was.
                                return self
                                    .record_webhook_terminal(
                                        pending,
                                        Vec::new(),
                                        RepoWatchWebhookDisposition::Superseded,
                                        None,
                                    )
                                    .await;
                            }
                            PreparedTargetedRefreshOutcome::NoTargets => None,
                            PreparedTargetedRefreshOutcome::Prepared(prepared) => Some(prepared),
                        };
                        // Only refreshes actually sent are recorded, so neither a
                        // branch-only delivery naming no pull request nor one
                        // whose hydration this page already issued can claim a
                        // query the poller never made.
                        let issued = prepared
                            .as_ref()
                            .map(|prepared| prepared.queried.clone())
                            .unwrap_or_default();
                        projections.extend(
                            issued
                                .iter()
                                .map(targeted_query_projection)
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                        if let Some(prepared) = prepared {
                            // Retain the exact cursor commit, projections, and
                            // resulting shadow as one completion. Cancellation
                            // of the outer drain cannot separate those durable
                            // steps or lose targeted-query provenance.
                            let settlement = self
                                .complete_targeted_webhook_projection(
                                    prepared,
                                    pending.key(),
                                    projections,
                                    WebhookShadowBaseline {
                                        observation,
                                        identity_frontier,
                                        merged_pull_request_baselines: shadow
                                            .merged_pull_request_baselines
                                            .clone(),
                                    },
                                )
                                .await?;
                            // A superseded commit leaves this delivery terminal
                            // but never reached the cursor, so the coalescer must
                            // not treat its hydration as landed: a later delivery
                            // for the same pull request on this page still owes
                            // the targeted query this one failed to commit.
                            if settlement == TargetedRefreshSettlement::Landed {
                                page.record_issued(&issued);
                            }
                        } else {
                            self.record_webhook_terminal(
                                pending,
                                projections,
                                RepoWatchWebhookDisposition::Projected,
                                None,
                            )
                            .await?;
                            self.webhook_shadow = Some(WebhookShadowBaseline {
                                observation,
                                identity_frontier,
                                merged_pull_request_baselines: shadow
                                    .merged_pull_request_baselines
                                    .clone(),
                            });
                            self.webhook_shadow_superseded = false;
                        }
                        self.webhook_dispatch_in_flight = true;
                        let dispatch_result = self.process_dispatches().await;
                        self.webhook_dispatch_in_flight = false;
                        if let Err(error) = dispatch_result {
                            // Carries the identity here because this delivery
                            // is already terminal: it never reaches the drain
                            // page's deferral record, and the classified
                            // outcome the attempt reports names only a cause.
                            tracing::warn!(
                                repository = %self.repository.as_str(),
                                hook_id = pending.key().hook_id().get(),
                                delivery_id = %pending.key().delivery_id(),
                                receipt_sequence = pending.receipt().sequence().get(),
                                cause_code = error.cause_code(),
                                "webhook delivery dispatch failed after it reached terminal state"
                            );
                            dispatch_failure.get_or_insert(error);
                        }
                        Ok(())
                    }
                }
            }
        }
    }

    /// Applies one mapped delivery to the durable cursor under primary mode.
    ///
    /// The baseline is the durable cursor rather than an accumulated shadow:
    /// every applied delivery commits, so the next one reloads a cursor that
    /// already carries its predecessor. That also supplies the expected
    /// generation the optimistic commit needs, which an in-memory baseline
    /// advanced past the cursor could not.
    async fn process_primary_webhook_patch(
        &mut self,
        pending: &PendingRepoWatchWebhookDelivery,
        patch: RepoWatchObservationPatchV1,
        page: &mut RepoWatchTargetedRefreshCoalescerV1,
        dispatch_failure: &mut Option<RepositoryWatchAttemptError>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let cursor = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            .ok_or(RepositoryWatchAttemptError::Persistence)?;
        let baseline = WebhookShadowBaseline::from_cursor(&cursor);
        let applied = match apply_repo_watch_observation_patch_v1(&baseline.observation, &patch) {
            Ok(applied) => applied,
            Err(_) => {
                return self
                    .record_webhook_terminal(
                        pending,
                        Vec::new(),
                        RepoWatchWebhookDisposition::Quarantined,
                        Some("patch_incoherent"),
                    )
                    .await;
            }
        };
        let (observation, refreshes) = match applied {
            RepoWatchObservationApplyV1::DuplicateState => {
                return self
                    .record_webhook_terminal(
                        pending,
                        Vec::new(),
                        RepoWatchWebhookDisposition::DuplicateState,
                        None,
                    )
                    .await;
            }
            RepoWatchObservationApplyV1::Superseded => {
                return self
                    .record_webhook_terminal(
                        pending,
                        Vec::new(),
                        RepoWatchWebhookDisposition::Superseded,
                        None,
                    )
                    .await;
            }
            RepoWatchObservationApplyV1::Ignored(reason) => {
                // Committing nothing is the point: polling could never produce
                // this fact, so writing it would leave a durable event row for
                // a subject the complete sweep cannot reconcile toward.
                return self
                    .record_webhook_terminal(
                        pending,
                        Vec::new(),
                        RepoWatchWebhookDisposition::Ignored,
                        Some(webhook_ignored_reason_code(reason)),
                    )
                    .await;
            }
            RepoWatchObservationApplyV1::Applied(observation) => (observation, Vec::new()),
            RepoWatchObservationApplyV1::NeedsTargetedRefresh {
                observation,
                refreshes,
            } => (observation, refreshes.into_vec()),
        };
        // The provider query runs before anything is recorded, so a transient
        // fetch failure leaves this delivery pending and retryable rather than
        // terminal against state the poller never observed.
        let unissued = page.unissued(&refreshes);
        let (observation, issued) = match self
            .prepare_primary_webhook_refresh(
                &observation,
                &baseline.merged_pull_request_baselines,
                &unissued,
            )
            .await?
        {
            PreparedPrimaryRefreshOutcome::SupersededTarget => {
                // The provider proved a targeted head stale before anything was
                // recorded, so the delivery describes state the repository has
                // already left and the cursor stays exactly as it was.
                return self
                    .record_webhook_terminal(
                        pending,
                        Vec::new(),
                        RepoWatchWebhookDisposition::Superseded,
                        None,
                    )
                    .await;
            }
            PreparedPrimaryRefreshOutcome::Refreshed {
                observation,
                queried,
            } => (observation, queried),
        };
        let (events, identity_frontier) =
            primary_committed_occurrences(&self.repository, &baseline, &observation)?;
        let compacted = compact_cursor_observation(
            &observation,
            Some(&baseline.observation),
            &baseline.merged_pull_request_baselines,
        )?;
        // A primary delivery records no event projection. Parity compares
        // projections against poll-produced rows, and this delivery's own commit
        // is the durable row; projecting it too would leave a permanent
        // webhook_only row that no poll can ever match, since the poll starts
        // from the cursor this commit already advanced. Only the targeted
        // queries are recorded, and only those actually sent, so neither a
        // branch-only delivery naming no pull request nor one whose hydration
        // this page already issued can claim a query the poller never made.
        let projections = issued
            .iter()
            .map(targeted_query_projection)
            .collect::<Result<Vec<_>, _>>()?;
        let request = RepoWatchCommitRequest::from_webhook(
            Some(cursor.generation()),
            RepoWatchCursorCandidate::try_with_event_identity_frontier_and_merged_baselines(
                compacted.observation.clone(),
                identity_frontier.clone(),
                compacted.merged_pull_request_baselines.clone(),
            )
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
            events,
        );
        let settlement = self
            .complete_webhook_cursor_commit(
                request,
                pending.key(),
                projections,
                WebhookShadowBaseline {
                    observation: compacted.observation,
                    identity_frontier,
                    merged_pull_request_baselines: compacted.merged_pull_request_baselines,
                },
            )
            .await?;
        // A superseded commit leaves this delivery terminal but never reached
        // the cursor, so a later delivery for the same pull request on this page
        // still owes the targeted query this one failed to commit.
        if settlement == TargetedRefreshSettlement::Landed {
            page.record_issued(&issued);
        }
        self.webhook_dispatch_in_flight = true;
        let dispatch_result = self.process_dispatches().await;
        self.webhook_dispatch_in_flight = false;
        if let Err(error) = dispatch_result {
            // Carries the identity here because this delivery is already
            // terminal: it never reaches the drain page's deferral record, and
            // the classified outcome the attempt reports names only a cause.
            tracing::warn!(
                repository = %self.repository.as_str(),
                hook_id = pending.key().hook_id().get(),
                delivery_id = %pending.key().delivery_id(),
                receipt_sequence = pending.receipt().sequence().get(),
                cause_code = error.cause_code(),
                "webhook delivery dispatch failed after it reached terminal state"
            );
            dispatch_failure.get_or_insert(error);
        }
        Ok(())
    }

    /// Reconciles the pull requests one primary-mode delivery names.
    ///
    /// The patched observation is both the staleness baseline and the state the
    /// fetched pull requests merge into, so facts the payload supplied for
    /// untargeted subjects — a branch advance, say — survive the refresh.
    async fn prepare_primary_webhook_refresh(
        &self,
        patched: &RepoWatchObservation,
        merged_pull_request_baselines: &[RepoWatchMergedPullRequestBaselineV1],
        refreshes: &[RepoWatchTargetedRefreshV1],
    ) -> Result<PreparedPrimaryRefreshOutcome, RepositoryWatchAttemptError> {
        if refreshes.is_empty() {
            return Ok(PreparedPrimaryRefreshOutcome::Refreshed {
                observation: patched.clone(),
                queried: Vec::new(),
            });
        }
        let targets = targeted_pull_requests(patched, merged_pull_request_baselines, refreshes)?;
        if targets.is_empty() {
            return Ok(PreparedPrimaryRefreshOutcome::Refreshed {
                observation: patched.clone(),
                queried: Vec::new(),
            });
        }
        let (observation, superseded_targets) = match self
            .poller
            .poll_targeted_pull_requests_against_cursor(patched, &targets)
            .await?
        {
            TargetedPollOutcome::Observation {
                observation,
                superseded_targets,
            } => (observation, superseded_targets),
            TargetedPollOutcome::SupersededTarget => {
                return Ok(PreparedPrimaryRefreshOutcome::SupersededTarget);
            }
        };
        let applied_targets = targets
            .iter()
            .filter(|target| !superseded_targets.contains(&target.number))
            .cloned()
            .collect::<Vec<_>>();
        let queried = refreshes
            .iter()
            .filter(|refresh| refresh_reaches_a_target(refresh, &applied_targets))
            .cloned()
            .collect::<Vec<_>>();
        Ok(PreparedPrimaryRefreshOutcome::Refreshed {
            observation,
            queried,
        })
    }

    async fn record_webhook_terminal(
        &mut self,
        pending: &PendingRepoWatchWebhookDelivery,
        projections: Vec<RepoWatchWebhookProjection>,
        disposition: RepoWatchWebhookDisposition,
        outcome_code: Option<&str>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        // Only a projected record is followed by a shadow advance, so only its
        // uncertain durability can leave the baseline silently missing a
        // delivery. Records that change no shadow fact keep the accumulated
        // baseline even when their settlement stays unknown.
        let advances_shadow = matches!(disposition, RepoWatchWebhookDisposition::Projected);
        let request = RepoWatchWebhookTerminalRequest::try_new(
            projections,
            disposition,
            outcome_code.map(str::to_owned),
        )
        .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        self.webhook_projected_terminal_in_flight = advances_shadow.then_some(pending.key());
        for attempt in 1..=MAX_WEBHOOK_TERMINAL_ATTEMPTS {
            match self
                .webhook_store
                .record_terminal(pending.key(), &request)
                .await
            {
                Ok(_) => {
                    self.webhook_projected_terminal_in_flight = None;
                    if self.webhook_terminal_ambiguous == Some(pending.key()) {
                        self.webhook_terminal_ambiguous = None;
                    }
                    return Ok(());
                }
                // A commit whose result was lost in transit may already be
                // durable, and the delivery would then never be loaded again.
                // Reading settles which happened and cannot itself be
                // ambiguous, so the outcome is definitive without retrying the
                // write indefinitely against a connection that keeps dropping.
                Err(RepoWatchWebhookStoreError::CommitAmbiguous(_)) => {
                    match self
                        .webhook_store
                        .terminal_disposition_exists(pending.key())
                        .await
                    {
                        Ok(true) => {
                            self.webhook_projected_terminal_in_flight = None;
                            if self.webhook_terminal_ambiguous == Some(pending.key()) {
                                self.webhook_terminal_ambiguous = None;
                            }
                            return Ok(());
                        }
                        // A read that fails settles nothing, so it is retried
                        // rather than propagated: propagating would abandon a
                        // delivery that may already be durable.
                        Ok(false) | Err(_) => {
                            if attempt < MAX_WEBHOOK_TERMINAL_ATTEMPTS {
                                sleep(WEBHOOK_TERMINAL_RETRY_DELAY).await;
                            }
                        }
                    }
                }
                Err(_) => {
                    self.webhook_projected_terminal_in_flight = None;
                    return Err(RepositoryWatchAttemptError::Persistence);
                }
            }
        }
        // Every attempt was ambiguous or unreadable, so whether a disposition
        // is durable is unknown. A projected record's shadow is discarded
        // rather than trusted: if one did land, this delivery will never be
        // loaded again, and a baseline that silently missed it would supersede
        // its dependents. The next delivery re-seeds from the cursor and
        // records the gap.
        if advances_shadow {
            self.webhook_shadow = None;
            self.webhook_terminal_ambiguous = Some(pending.key());
        }
        self.webhook_projected_terminal_in_flight = None;
        Err(RepositoryWatchAttemptError::Persistence)
    }

    /// Runs one delivery's targeted provider queries without writing anything.
    ///
    /// The fetch is separated from its commit so a transient provider failure
    /// leaves the delivery pending and retryable, rather than terminal with a
    /// targeted query that never ran.
    async fn prepare_targeted_refresh(
        &mut self,
        refreshes: &[RepoWatchTargetedRefreshV1],
    ) -> Result<PreparedTargetedRefreshOutcome, RepositoryWatchAttemptError> {
        if refreshes.is_empty() {
            return Ok(PreparedTargetedRefreshOutcome::NoTargets);
        }
        let cursor = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            .ok_or(RepositoryWatchAttemptError::Persistence)?;
        let targets = targeted_pull_requests(
            cursor.candidate().observation(),
            cursor.candidate().merged_pull_request_baselines(),
            refreshes,
        )?;
        if targets.is_empty() {
            return Ok(PreparedTargetedRefreshOutcome::NoTargets);
        }
        let mut event_identity_frontier = cursor.candidate().event_identity_frontier().clone();
        let (observation, superseded_targets) = match self
            .poller
            .poll_targeted_pull_requests_against_cursor(cursor.candidate().observation(), &targets)
            .await?
        {
            TargetedPollOutcome::Observation {
                observation,
                superseded_targets,
            } => (observation, superseded_targets),
            TargetedPollOutcome::SupersededTarget => {
                return Ok(PreparedTargetedRefreshOutcome::SupersededTarget);
            }
        };
        // A refresh naming no pull request the cursor carries is never sent,
        // and one reaching only targets the provider proved stale never landed,
        // so neither is recorded as a query that happened.
        let applied_targets = targets
            .iter()
            .filter(|target| !superseded_targets.contains(&target.number))
            .cloned()
            .collect::<Vec<_>>();
        let queried = refreshes
            .iter()
            .filter(|refresh| refresh_reaches_a_target(refresh, &applied_targets))
            .cloned()
            .collect::<Vec<_>>();
        let targeted_pull_requests = applied_targets
            .iter()
            .map(|target| target.number)
            .collect::<Vec<_>>();
        let events = derive_repo_watch_events_with_merged_baselines(
            &self.repository,
            Some(cursor.candidate().observation()),
            cursor.candidate().merged_pull_request_baselines(),
            &observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|_| RepositoryWatchAttemptError::Differ)?;
        let compacted = compact_cursor_observation(
            &observation,
            Some(cursor.candidate().observation()),
            cursor.candidate().merged_pull_request_baselines(),
        )?;
        Ok(PreparedTargetedRefreshOutcome::Prepared(
            PreparedTargetedRefresh {
                generation: cursor.generation(),
                candidate:
                    RepoWatchCursorCandidate::try_with_event_identity_frontier_and_merged_baselines(
                        compacted.observation,
                        event_identity_frontier,
                        compacted.merged_pull_request_baselines,
                    )
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                events,
                queried,
                targeted_pull_requests,
            },
        ))
    }

    /// Commits a targeted refresh and its exact webhook projection as one
    /// retained completion that survives cancellation of the outer drain.
    async fn complete_targeted_webhook_projection(
        &mut self,
        prepared: PreparedTargetedRefresh,
        key: RepoWatchWebhookDeliveryKey,
        projections: Vec<RepoWatchWebhookProjection>,
        shadow: WebhookShadowBaseline,
    ) -> Result<TargetedRefreshSettlement, RepositoryWatchAttemptError> {
        // A targeted refresh reconciles through the poller's own credential and
        // normalizer, so the rows it produces are poll-produced facts even when
        // a delivery asked for them.
        let refreshed_shadow = merge_targeted_refresh_into_webhook_shadow(
            shadow,
            &prepared.candidate,
            &prepared.targeted_pull_requests,
        )?;
        let request = RepoWatchCommitRequest::new(
            Some(prepared.generation),
            prepared.candidate,
            prepared.events,
        );
        self.complete_webhook_cursor_commit(request, key, projections, refreshed_shadow)
            .await
    }

    /// Commits one webhook-driven cursor write and its exact projections as a
    /// single retained completion that survives cancellation of the outer drain.
    async fn complete_webhook_cursor_commit(
        &mut self,
        request: RepoWatchCommitRequest,
        key: RepoWatchWebhookDeliveryKey,
        projections: Vec<RepoWatchWebhookProjection>,
        shadow: WebhookShadowBaseline,
    ) -> Result<TargetedRefreshSettlement, RepositoryWatchAttemptError> {
        let store = self.store.clone();
        let webhook_store = self.webhook_store.clone();
        let poller = Arc::clone(&self.poller);
        let repository = self.repository.clone();
        // The disposition records what this delivery did, not what mode was
        // configured (`202608250500_repo_watch_webhook_primary.sql`), and the
        // commit request already carries that fact: `from_webhook` marks rows a
        // primary delivery owns, and `new` marks the poll-produced rows a
        // targeted refresh reconciles through the poller's own credential.
        //
        // A committed disposition is the only durable evidence of primary
        // ownership when the applied observation derived no event, and
        // recording it here — before the cursor write, where the two-step
        // handoff already records the terminal row — is what lets the parity
        // view end its measurement at the repository's first primary commit
        // instead of at the first webhook-produced event, which a context-only
        // delivery never writes.
        //
        // A shadow-mode targeted refresh reaches this helper too, by way of
        // `complete_targeted_webhook_projection`. Its rows are poll-produced
        // and its projections are the shadow observations parity compares
        // against them, so it stays `Projected`: recording it as committed
        // would set `primary_start` for the repository and permanently drop
        // every later poll event from parity in a deployment that never entered
        // primary mode, ending the very measurement the delivery belongs to.
        let disposition = match request.producer() {
            RepoWatchEventProducer::Webhook => RepoWatchWebhookDisposition::Committed,
            RepoWatchEventProducer::Poll => RepoWatchWebhookDisposition::Projected,
        };
        let terminal = RepoWatchWebhookTerminalRequest::try_new(projections, disposition, None)
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        let supersession_epoch = self.webhook_shadow_supersession_epoch;
        self.webhook_targeted_completion = Some(RetainedTargetedWebhookCompletion::new(
            tokio::spawn(async move {
                // Persist the terminal disposition and its exact projections first.
                // This is the durable recovery handoff: if shutdown later aborts
                // cursor advancement, restart excludes the delivery without losing
                // its projections, and an ordinary poll can advance the old cursor.
                record_webhook_terminal_request(&webhook_store, key, &terminal)
                    .await
                    .map_err(|error| TargetedWebhookCompletionError::Terminal(key, error))?;
                let outcome = store
                    .commit(&repository, request)
                    .await
                    .map_err(|_| TargetedWebhookCompletionError::Cursor)?;
                match outcome {
                    RepoWatchCommitOutcome::Committed(cursor)
                    | RepoWatchCommitOutcome::Replayed(cursor)
                    | RepoWatchCommitOutcome::Unchanged(cursor) => {
                        poller.publish_freshness(cursor.generation());
                    }
                    RepoWatchCommitOutcome::Conflict { current: _ } => {
                        // This fetch never became cursor state, but it already
                        // recorded unpublished freshness. Leaving those entries
                        // behind would let the next commit's `publish_freshness`
                        // stamp them with a generation they never reached, and
                        // a later poll would then reuse detail the cursor does
                        // not carry. A competing writer owns the cursor, which
                        // is exactly the condition this clearing exists for.
                        poller.invalidate_freshness();
                        return Ok(TargetedWebhookCompletion::CursorSuperseded { key });
                    }
                }
                Ok(TargetedWebhookCompletion::Applied {
                    key,
                    shadow,
                    supersession_epoch,
                })
            }),
        ));
        self.settle_webhook_targeted_completion()
            .await
            .ok_or(RepositoryWatchAttemptError::Persistence)?
    }

    /// Settles a targeted completion retained across drain cancellation.
    ///
    /// Awaiting the handle by mutable reference means cancelling this caller
    /// leaves the database task and its handle intact. A later drain settles
    /// that exact commit before reading or writing subsequent repository work.
    async fn settle_webhook_targeted_completion(
        &mut self,
    ) -> Option<Result<TargetedRefreshSettlement, RepositoryWatchAttemptError>> {
        let result = {
            let handle = self.webhook_targeted_completion.as_mut()?;
            handle
                .join()
                .await
                .map_err(|_| TargetedWebhookCompletionError::Persistence)
                .and_then(|result| result)
        };
        self.webhook_targeted_completion = None;
        match result {
            Ok(TargetedWebhookCompletion::Applied {
                key,
                shadow,
                supersession_epoch,
            }) => {
                if self.webhook_terminal_ambiguous == Some(key) {
                    self.webhook_terminal_ambiguous = None;
                }
                self.webhook_shadow = Some(shadow);
                if self.webhook_shadow_supersession_epoch == supersession_epoch {
                    self.webhook_shadow_superseded = false;
                }
                Some(Ok(TargetedRefreshSettlement::Landed))
            }
            Ok(TargetedWebhookCompletion::CursorSuperseded { key }) => {
                if self.webhook_terminal_ambiguous == Some(key) {
                    self.webhook_terminal_ambiguous = None;
                }
                // The terminal disposition and projections are durable, while
                // a competing poll owns the current cursor. Hand the shadow
                // over immediately so later pending receipts seed from it.
                self.webhook_shadow = None;
                self.webhook_shadow_superseded = false;
                Some(Ok(TargetedRefreshSettlement::Superseded))
            }
            Err(TargetedWebhookCompletionError::Terminal(
                key,
                WebhookTerminalRecordError::Ambiguous,
            )) => {
                self.poller.invalidate_freshness();
                self.webhook_shadow = None;
                self.webhook_shadow_superseded = false;
                self.webhook_terminal_ambiguous = Some(key);
                Some(Err(RepositoryWatchAttemptError::Persistence))
            }
            Err(TargetedWebhookCompletionError::Cursor) => {
                self.poller.invalidate_freshness();
                // The terminal disposition and exact projections are durable,
                // but the cursor outcome is unknown. Reload the durable cursor
                // before projecting any later pending receipt.
                self.webhook_shadow = None;
                self.webhook_shadow_superseded = false;
                Some(Err(RepositoryWatchAttemptError::Persistence))
            }
            Err(_) => {
                self.poller.invalidate_freshness();
                Some(Err(RepositoryWatchAttemptError::Persistence))
            }
        }
    }

    async fn process_cutoffs(&self) -> Result<(), RepositoryWatchAttemptError> {
        // numeric-bound: guard - prevents a repeatedly quarantined lease from looping the repository task forever
        const MAX_EXPIRED_START_LEASES_PER_ATTEMPT: usize = 32;
        for _ in 0..MAX_EXPIRED_START_LEASES_PER_ATTEMPT {
            match self
                .dispatch_store
                .process_next_expired_start_lease(&self.repository, || {
                    DurableCommandId::from_uuid(uuid::Uuid::now_v7())
                })
                .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(RepoWatchDispatchRepositoryError::GoalCutoff(
                    error @ signalbox_persistence::goal::GoalRepositoryError::Corruption(_),
                )) => {
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = "repository_watch_expired_start_lease_corruption",
                        error = %error,
                        "repository-watch expired start lease quarantined a corrupt goal; cutoff processing continues"
                    );
                    continue;
                }
                Err(error @ RepoWatchDispatchRepositoryError::Corruption(_)) => {
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = "repository_watch_expired_start_lease_corruption",
                        error = %error,
                        "repository-watch expired start lease quarantined corrupt storage; cutoff processing continues"
                    );
                    continue;
                }
                Err(_) => return Err(RepositoryWatchAttemptError::Persistence),
            }
        }
        let mut processed = 0_usize;
        loop {
            match self
                .dispatch_store
                .process_next_lifecycle_cutoff(&self.repository, || {
                    DurableCommandId::from_uuid(uuid::Uuid::now_v7())
                })
                .await
            {
                Ok(true) => {
                    processed = processed.saturating_add(1);
                    if self.yield_after_reconciliation_quantum("lifecycle_cutoff", processed) {
                        return Ok(());
                    }
                }
                Ok(false) => break,
                Err(RepoWatchDispatchRepositoryError::GoalCutoff(
                    error @ signalbox_persistence::goal::GoalRepositoryError::Corruption(_),
                )) => {
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = "repository_watch_cutoff_corruption",
                        error = %error,
                        "repository-watch lifecycle cutoff quarantined a corrupt goal; dispatch processing continues"
                    );
                    processed = processed.saturating_add(1);
                    if self.yield_after_reconciliation_quantum("lifecycle_cutoff", processed) {
                        return Ok(());
                    }
                    continue;
                }
                Err(_) => return Err(RepositoryWatchAttemptError::Persistence),
            }
        }
        let mut processed = 0_usize;
        loop {
            match self
                .dispatch_store
                .process_next_convergence_cutoff(&self.repository, || {
                    DurableCommandId::from_uuid(uuid::Uuid::now_v7())
                })
                .await
            {
                Ok(true) => {
                    processed = processed.saturating_add(1);
                    if self.yield_after_reconciliation_quantum("convergence_cutoff", processed) {
                        return Ok(());
                    }
                }
                Ok(false) => break,
                Err(RepoWatchDispatchRepositoryError::GoalCutoff(
                    error @ signalbox_persistence::goal::GoalRepositoryError::Corruption(_),
                )) => {
                    tracing::error!(
                        repository = %self.repository.as_str(),
                        cause_code = "repository_watch_convergence_cutoff_corruption",
                        error = %error,
                        "repository-watch convergence cutoff quarantined a corrupt goal; dispatch processing continues"
                    );
                    processed = processed.saturating_add(1);
                    if self.yield_after_reconciliation_quantum("convergence_cutoff", processed) {
                        return Ok(());
                    }
                    continue;
                }
                Err(_) => return Err(RepositoryWatchAttemptError::Persistence),
            }
        }
        Ok(())
    }

    async fn activate_rules(&self) -> Result<(), RepositoryWatchAttemptError> {
        self.dispatch_store
            .reconcile_rules(&self.repository, &self.rules)
            .await
            .map_err(rule_activation_error)
    }

    async fn process_dispatches(&mut self) -> Result<(), RepositoryWatchAttemptError> {
        self.nudge_restored_module_sessions().await?;
        let unstarted = self
            .dispatch_store
            .load_unstarted_dispatch_sessions(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        for session in unstarted {
            self.nudge_dispatch_start(session);
        }
        let mut processed = 0_usize;
        for rule in &self.rules {
            while let Some(event) = self
                .dispatch_store
                .load_next_event(&self.repository, rule.id(), rule.version())
                .await
                .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            {
                let cursor = self
                    .store
                    .load_cursor(&self.repository)
                    .await
                    .map_err(|_| RepositoryWatchAttemptError::Persistence)?
                    .ok_or(RepositoryWatchAttemptError::Persistence)?;
                let content = UserContent::try_text(dispatch_context_json(&event))
                    .map_err(|_| RepositoryWatchAttemptError::Dispatch)?;
                let mut service = RepoWatchDispatchService::new(
                    UuidV7RepoWatchDispatchIdGenerator,
                    RepoWatchDispatchPersistence {
                        store: self.dispatch_store.clone(),
                        models: &self.models,
                        obligation: None,
                    },
                );
                let outcome = service
                    .evaluate(
                        event,
                        rule,
                        cursor.candidate().observation(),
                        &self.templates,
                        content,
                    )
                    .await
                    .map_err(|_| RepositoryWatchAttemptError::Dispatch)?;
                self.nudge_dispatched_sessions(&outcome);
                self.nudge_restored_module_sessions().await?;
                processed = processed.saturating_add(1);
                if self.yield_after_reconciliation_quantum("rule_evaluation", processed) {
                    return Ok(());
                }
            }
            while let Some(obligation) = self
                .dispatch_store
                .load_next_dispatch_obligation(
                    &self.repository,
                    rule.id(),
                    rule.version(),
                    RepoWatchDispatchRetryPolicy::production(),
                )
                .await
                .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            {
                let cursor = self
                    .store
                    .load_cursor(&self.repository)
                    .await
                    .map_err(|_| RepositoryWatchAttemptError::Persistence)?
                    .ok_or(RepositoryWatchAttemptError::Persistence)?;
                let event = obligation.latest_event().clone();
                let content = UserContent::try_text(owed_dispatch_context_json(
                    &obligation,
                    cursor.candidate().observation(),
                ))
                .map_err(|_| RepositoryWatchAttemptError::Dispatch)?;
                let mut service = RepoWatchDispatchService::new(
                    UuidV7RepoWatchDispatchIdGenerator,
                    RepoWatchDispatchPersistence {
                        store: self.dispatch_store.clone(),
                        models: &self.models,
                        obligation: Some(obligation),
                    },
                );
                let outcome = service
                    .evaluate(
                        event,
                        rule,
                        cursor.candidate().observation(),
                        &self.templates,
                        content,
                    )
                    .await
                    .map_err(|_| RepositoryWatchAttemptError::Dispatch)?;
                self.nudge_dispatched_sessions(&outcome);
                self.nudge_restored_module_sessions().await?;
                processed = processed.saturating_add(1);
                if self.yield_after_reconciliation_quantum("dispatch_obligation", processed) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    async fn nudge_restored_module_sessions(&self) -> Result<(), RepositoryWatchAttemptError> {
        let restored = self
            .dispatch_store
            .load_restored_module_sessions()
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        for session in restored {
            let _ = self.eligibility_nudge.nudge(session);
        }
        Ok(())
    }

    fn yield_after_reconciliation_quantum(&self, phase: &'static str, processed: usize) -> bool {
        if !repository_reconciliation_should_yield(
            processed,
            self.reconciliation_quantum,
            self.webhook_nudge.is_some(),
        ) {
            return false;
        }
        self.request_webhook_drain_continuation();
        tracing::info!(
            repository = %self.repository.as_str(),
            phase,
            processed,
            cause_code = "repository_watch_reconciliation_quantum_exhausted",
            "repository-watch reconciliation yielded to its webhook-aware scheduler"
        );
        true
    }

    fn nudge_dispatched_sessions(&self, outcome: &RepoWatchRuleEvaluationOutcome) {
        match outcome {
            RepoWatchRuleEvaluationOutcome::Dispatched { sessions, .. }
            | RepoWatchRuleEvaluationOutcome::Replayed { sessions, .. } => {
                for session in sessions {
                    self.nudge_dispatch_start(*session);
                }
            }
            RepoWatchRuleEvaluationOutcome::NotMatched
            | RepoWatchRuleEvaluationOutcome::Inactive
            | RepoWatchRuleEvaluationOutcome::TargetClosed
            | RepoWatchRuleEvaluationOutcome::TargetConverged
            | RepoWatchRuleEvaluationOutcome::Occupied
            | RepoWatchRuleEvaluationOutcome::Cooldown => {}
        }
    }

    fn nudge_dispatch_start(&self, session: signalbox_domain::SessionId) {
        let outcome = self.eligibility_nudge.nudge_dispatch_start(session);
        record_dispatch_start_nudge_outcome(&self.repository, session, outcome);
    }

    /// Loads the durable baseline and performs the read-only provider sweep.
    ///
    /// This phase may be abandoned for a webhook wake. It deliberately stops
    /// before the cursor commit so cancellation cannot leave a durable result
    /// whose caller did not observe.
    async fn prepare_complete_poll(
        &mut self,
    ) -> Result<PreparedCompletePoll, RepositoryWatchAttemptError> {
        let cursor = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        let previous = cursor
            .as_ref()
            .map(|cursor| cursor.candidate().observation());
        let cursor_generation = cursor.as_ref().map(|cursor| cursor.generation());
        let mut event_identity_frontier = cursor
            .as_ref()
            .map(|cursor| cursor.candidate().event_identity_frontier().clone())
            .unwrap_or_default();
        let polled = self
            .poller
            .poll_against_cursor(previous, cursor_generation)
            .await?;
        let merged_pull_request_baselines = cursor
            .as_ref()
            .map(|cursor| cursor.candidate().merged_pull_request_baselines())
            .unwrap_or_default();
        let events = derive_repo_watch_events_with_merged_baselines(
            &self.repository,
            previous,
            merged_pull_request_baselines,
            &polled.observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|error| match error.kind() {
            RepoWatchDifferFailureKind::BaselineCollection => RepositoryWatchAttemptError::Differ,
            RepoWatchDifferFailureKind::EventConstruction => RepositoryWatchAttemptError::Differ,
            // Its own cause code, because the frontier and a differ defect call
            // for different operator responses. The attempt stays retryable: a
            // later observation introducing no new stream still succeeds.
            RepoWatchDifferFailureKind::IdentityFrontier => {
                tracing::error!(
                    repository = %self.repository.as_str(),
                    cause_code = "repository_identity_frontier_exhausted",
                    error = %error,
                    "repository-watch identity frontier cannot assign another occurrence; an observation introducing no new stream can still succeed"
                );
                RepositoryWatchAttemptError::IdentityFrontier
            }
        })?;
        let compacted = compact_cursor_observation(
            &polled.observation,
            previous,
            merged_pull_request_baselines,
        )?;
        let retained_pull_requests = compacted
            .observation
            .state()
            .pull_requests()
            .iter()
            .map(|pull_request| pull_request.context().number())
            .collect::<HashSet<_>>();
        let convergence = polled
            .convergence
            .into_iter()
            .filter(|assessment| retained_pull_requests.contains(&assessment.number()))
            .collect();
        Ok(PreparedCompletePoll {
            cursor_generation,
            candidate:
                RepoWatchCursorCandidate::try_with_event_identity_frontier_and_merged_baselines(
                    compacted.observation,
                    event_identity_frontier,
                    compacted.merged_pull_request_baselines,
                )
                .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
            events,
            convergence,
            stale_review_clearances: polled.stale_review_clearances,
        })
    }

    /// Commits a completed provider sweep outside webhook cancellation.
    async fn commit_complete_poll(
        &mut self,
        prepared: PreparedCompletePoll,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let outcome = self
            .store
            .commit_with_convergence(
                &self.repository,
                RepoWatchCommitRequest::new(
                    prepared.cursor_generation,
                    prepared.candidate,
                    prepared.events,
                ),
                &prepared.convergence,
            )
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        match outcome {
            RepoWatchCommitOutcome::Committed(cursor)
            | RepoWatchCommitOutcome::Replayed(cursor)
            | RepoWatchCommitOutcome::Unchanged(cursor) => {
                // Published before the clearance sweep rather than after it.
                // The cursor is durable at this point, so the freshness this
                // poll recorded is legitimately tied to a committed generation,
                // and clearance revalidation reads exactly that entry to decide
                // whether the gating-check inventory has stood still since the
                // observation that raised the candidate. A publication the
                // sweep never reached would leave every candidate unsettled and
                // no review would ever be dismissed. A failed attempt still
                // invalidates every entry on its way out.
                self.poller.publish_freshness(cursor.generation());
                self.reconcile_pending_stale_review_clearances().await?;
                let planned_clearances = self
                    .store
                    .plan_stale_review_clearances(
                        &self.repository,
                        cursor.generation(),
                        &prepared.stale_review_clearances,
                    )
                    .await
                    .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
                for clearance in &planned_clearances {
                    if !self
                        .poller
                        .revalidate_stale_review_clearance(clearance, cursor.generation())
                        .await?
                    {
                        self.store
                            .release_stale_review_clearance_claim(
                                clearance.clearance_id(),
                                clearance.claim_token(),
                            )
                            .await
                            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
                        continue;
                    }
                    if self
                        .store
                        .renew_stale_review_clearance_claim(
                            clearance.clearance_id(),
                            clearance.claim_token(),
                        )
                        .await
                        .map_err(|_| RepositoryWatchAttemptError::Persistence)?
                        == RepoWatchStaleReviewClearanceRenewal::Lost
                    {
                        continue;
                    }
                    self.poller
                        .dismiss_review_node(DismissReviewInput {
                            review_node_id: clearance.review_node_id(),
                            dismissal_message: clearance.dismissal_message(),
                        })
                        .await?;
                    self.store
                        .record_stale_review_clearance_outcome(
                            clearance.clearance_id(),
                            clearance.claim_token(),
                            RepoWatchStaleReviewClearanceOutcome::Dismissed,
                            RepoWatchObservedReviewState::Dismissed,
                        )
                        .await
                        .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
                }
                // A full poll is the complete reconciliation sweep, so the
                // cursor it commits supersedes everything the webhook stream
                // had accumulated in memory. It is not handed over here: this
                // task cannot read the queue atomically with an admission
                // committing on the listener, so a delivery admitted while this
                // poll was fetching could be applied to a cursor that already
                // contains its transition. The handoff happens in the drain
                // instead, where an empty page and the replacement are decided
                // without an await between them.
                self.webhook_shadow_superseded = true;
                self.webhook_shadow_supersession_epoch =
                    self.webhook_shadow_supersession_epoch.wrapping_add(1);
                Ok(())
            }
            RepoWatchCommitOutcome::Conflict { current: _ } => {
                Err(RepositoryWatchAttemptError::Persistence)
            }
        }
    }

    async fn reconcile_pending_stale_review_clearances(
        &self,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let pending = self
            .store
            .claim_pending_stale_review_clearances(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        for clearance in &pending {
            // Observing a batch row costs provider requests, so a deeply
            // paginated batch can outlive the two-minute lease taken when it
            // was claimed. Re-establish ownership immediately before each row:
            // a lease another watcher has since taken belongs to that watcher,
            // and skipping the row leaves it to them instead of acting twice.
            if self
                .store
                .renew_stale_review_clearance_claim(
                    clearance.clearance_id(),
                    clearance.claim_token(),
                )
                .await
                .map_err(|_| RepositoryWatchAttemptError::Persistence)?
                == RepoWatchStaleReviewClearanceRenewal::Lost
            {
                continue;
            }
            match self
                .poller
                .observe_stale_review_clearance(clearance)
                .await?
            {
                StaleReviewClearanceObservation::StillBlocking => {
                    self.store
                        .release_stale_review_clearance_claim(
                            clearance.clearance_id(),
                            clearance.claim_token(),
                        )
                        .await
                        .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
                }
                StaleReviewClearanceObservation::Terminal {
                    outcome,
                    provider_state,
                } => {
                    // The lease can still expire between the renewal above and
                    // this write. That intent now belongs to its new claimant,
                    // whose own scan will settle it; failing the attempt here
                    // would instead abandon every row the batch has left.
                    match self
                        .store
                        .record_stale_review_clearance_outcome(
                            clearance.clearance_id(),
                            clearance.claim_token(),
                            outcome,
                            provider_state,
                        )
                        .await
                    {
                        Ok(()) => {}
                        Err(RepoWatchStoreError::StaleReviewClearanceMismatch) => continue,
                        Err(_) => return Err(RepositoryWatchAttemptError::Persistence),
                    }
                }
            }
        }
        Ok(())
    }
}

async fn record_webhook_terminal_request(
    store: &PostgresRepoWatchWebhookStore,
    key: RepoWatchWebhookDeliveryKey,
    request: &RepoWatchWebhookTerminalRequest,
) -> Result<(), WebhookTerminalRecordError> {
    for attempt in 1..=MAX_WEBHOOK_TERMINAL_ATTEMPTS {
        match store.record_terminal(key, request).await {
            Ok(_) => return Ok(()),
            Err(RepoWatchWebhookStoreError::CommitAmbiguous(_)) => {
                match store.terminal_disposition_exists(key).await {
                    Ok(true) => return Ok(()),
                    Ok(false) | Err(_) if attempt < MAX_WEBHOOK_TERMINAL_ATTEMPTS => {
                        sleep(WEBHOOK_TERMINAL_RETRY_DELAY).await;
                    }
                    Ok(false) | Err(_) => {}
                }
            }
            Err(_) => return Err(WebhookTerminalRecordError::Persistence),
        }
    }
    Err(WebhookTerminalRecordError::Ambiguous)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebhookTerminalRecordError {
    Persistence,
    Ambiguous,
}

#[derive(Debug)]
enum TargetedWebhookCompletionError {
    Persistence,
    Cursor,
    Terminal(RepoWatchWebhookDeliveryKey, WebhookTerminalRecordError),
}

struct RetainedTargetedWebhookCompletion {
    handle: JoinHandle<Result<TargetedWebhookCompletion, TargetedWebhookCompletionError>>,
}

impl RetainedTargetedWebhookCompletion {
    fn new(
        handle: JoinHandle<Result<TargetedWebhookCompletion, TargetedWebhookCompletionError>>,
    ) -> Self {
        Self { handle }
    }

    async fn join(
        &mut self,
    ) -> Result<
        Result<TargetedWebhookCompletion, TargetedWebhookCompletionError>,
        tokio::task::JoinError,
    > {
        (&mut self.handle).await
    }

    async fn abort_and_join(mut self) {
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for RetainedTargetedWebhookCompletion {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

enum TargetedWebhookCompletion {
    Applied {
        key: RepoWatchWebhookDeliveryKey,
        shadow: WebhookShadowBaseline,
        supersession_epoch: u64,
    },
    CursorSuperseded {
        key: RepoWatchWebhookDeliveryKey,
    },
}

/// Whether a settled targeted completion reached the durable cursor.
///
/// A superseded completion keeps its terminal disposition and projections, so
/// the delivery is done, but its fetch never became cursor state. Callers that
/// record consequences of the fetch landing must distinguish the two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetedRefreshSettlement {
    /// The commit reached the durable cursor.
    Landed,
    /// A competing writer owned the cursor, so this fetch never reached it.
    Superseded,
}

/// One complete provider sweep derived against a durable cursor but not yet
/// committed.
struct PreparedCompletePoll {
    cursor_generation: Option<RepoWatchCursorGeneration>,
    candidate: RepoWatchCursorCandidate,
    events: Vec<RepoWatchEventOccurrenceV1>,
    convergence: Vec<RepoWatchConvergenceAssessment>,
    stale_review_clearances: Vec<RepoWatchStaleReviewClearanceCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetedPullRequest {
    number: PullRequestNumber,
    expected_head: Option<CommitSha>,
}

/// One targeted refresh that has been fetched and derived but not committed.
/// What a targeted poll produced: a reconciled observation alongside any
/// targets the provider proved stale, or proof that every target moved.
#[derive(Debug, PartialEq)]
enum TargetedPollOutcome {
    Observation {
        observation: RepoWatchObservation,
        superseded_targets: Vec<PullRequestNumber>,
    },
    SupersededTarget,
}

/// What reconciling a primary-mode delivery's named pull requests produced.
enum PreparedPrimaryRefreshOutcome {
    /// The provider proved every targeted head stale; the delivery is superseded.
    SupersededTarget,
    /// The observation to commit, and the refreshes a request was issued for.
    Refreshed {
        observation: RepoWatchObservation,
        queried: Vec<RepoWatchTargetedRefreshV1>,
    },
}

/// What preparing a targeted refresh decided for the delivery that asked.
enum PreparedTargetedRefreshOutcome {
    /// No named pull request is carried by the cursor; nothing was queried.
    NoTargets,
    /// The provider proved a targeted head stale; the delivery is superseded.
    SupersededTarget,
    Prepared(PreparedTargetedRefresh),
}

struct PreparedTargetedRefresh {
    generation: RepoWatchCursorGeneration,
    candidate: RepoWatchCursorCandidate,
    events: Vec<RepoWatchEventOccurrenceV1>,
    /// The requested refreshes a provider request was actually issued for.
    queried: Vec<RepoWatchTargetedRefreshV1>,
    /// Pull requests whose refreshed state reached the candidate.
    targeted_pull_requests: Vec<PullRequestNumber>,
}

/// What one webhook drain has already projected, carried across the batch.
///
/// Dependent deliveries are drained together, and a projection that leaves the
/// durable cursor alone still moves the shadow experiment's baseline. Reloading
/// the unchanged cursor for every delivery would compare the second of two
/// dependent branch pushes against the first one's predecessor and record a
/// valid occurrence as superseded.
#[derive(Clone, Debug)]
struct WebhookShadowBaseline {
    observation: RepoWatchObservation,
    identity_frontier: RepoWatchEventIdentityFrontierV1,
    merged_pull_request_baselines: Vec<RepoWatchMergedPullRequestBaselineV1>,
}

impl WebhookShadowBaseline {
    fn from_cursor(cursor: &RepoWatchCursor) -> Self {
        Self {
            observation: cursor.candidate().observation().clone(),
            identity_frontier: cursor.candidate().event_identity_frontier().clone(),
            merged_pull_request_baselines: cursor
                .candidate()
                .merged_pull_request_baselines()
                .to_vec(),
        }
    }
}

/// Applies only the refreshed pull requests to the accumulated payload shadow.
///
/// The targeted candidate starts at the durable cursor, while the shadow may
/// already carry unrelated payload changes from this drain. Replacing either
/// collection wholesale would silently discard those newer changes.
fn merge_targeted_refresh_into_webhook_shadow(
    shadow: WebhookShadowBaseline,
    candidate: &RepoWatchCursorCandidate,
    targeted_pull_requests: &[PullRequestNumber],
) -> Result<WebhookShadowBaseline, RepositoryWatchAttemptError> {
    let targets = targeted_pull_requests
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut pull_requests = shadow
        .observation
        .state()
        .pull_requests()
        .iter()
        .filter(|pull_request| !targets.contains(&pull_request.context().number()))
        .cloned()
        .collect::<Vec<_>>();
    pull_requests.extend(
        candidate
            .observation()
            .state()
            .pull_requests()
            .iter()
            .filter(|pull_request| targets.contains(&pull_request.context().number()))
            .cloned(),
    );
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests,
        workflow_runs: shadow.observation.state().workflow_runs().to_vec(),
        branch_heads: shadow.observation.state().branch_heads().to_vec(),
    })
    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;

    let mut baselines = shadow
        .merged_pull_request_baselines
        .into_iter()
        .filter(|baseline| !targets.contains(&baseline.number()))
        .map(|baseline| (baseline.number(), baseline))
        .collect::<BTreeMap<_, _>>();
    baselines.extend(
        candidate
            .merged_pull_request_baselines()
            .iter()
            .filter(|baseline| targets.contains(&baseline.number()))
            .cloned()
            .map(|baseline| (baseline.number(), baseline)),
    );

    Ok(WebhookShadowBaseline {
        observation: RepoWatchObservation::new(
            shadow.observation.signal_reviewers().to_vec(),
            state,
        ),
        identity_frontier: merge_event_identity_frontiers(
            shadow.identity_frontier,
            candidate.event_identity_frontier(),
        )?,
        merged_pull_request_baselines: baselines.into_values().collect(),
    })
}

/// Joins two advances from the same durable frontier without moving a stream
/// backwards. A pull-request subject can be learned by either branch but
/// cannot conflict.
fn merge_event_identity_frontiers(
    shadow: RepoWatchEventIdentityFrontierV1,
    candidate: &RepoWatchEventIdentityFrontierV1,
) -> Result<RepoWatchEventIdentityFrontierV1, RepositoryWatchAttemptError> {
    let mut entries = shadow
        .entries()
        .map(|entry| (*entry.stream_identity(), entry))
        .collect::<BTreeMap<_, _>>();
    for candidate_entry in candidate.entries() {
        let identity = *candidate_entry.stream_identity();
        let Some(shadow_entry) = entries.get_mut(&identity) else {
            entries.insert(identity, candidate_entry);
            continue;
        };
        if let (Some(shadow_subject), Some(candidate_subject)) = (
            shadow_entry.pull_request_number(),
            candidate_entry.pull_request_number(),
        ) && shadow_subject != candidate_subject
        {
            return Err(RepositoryWatchAttemptError::Normalization);
        }
        let sequence = shadow_entry.sequence().max(candidate_entry.sequence());
        let subject = shadow_entry
            .pull_request_number()
            .or(candidate_entry.pull_request_number());
        *shadow_entry = match subject {
            Some(number) => {
                RepoWatchEventIdentityFrontierEntryV1::for_pull_request(identity, sequence, number)
            }
            None => RepoWatchEventIdentityFrontierEntryV1::new(identity, sequence),
        };
    }
    RepoWatchEventIdentityFrontierV1::try_from_entries(entries.into_values().collect())
        .map_err(|_| RepositoryWatchAttemptError::Normalization)
}

/// Derives the occurrences one primary-mode delivery commits.
///
/// Returns the event batch and the frontier it advances to. Primary mode
/// records no event projection, so this is the only derivation the delivery
/// performs and the committed rows are what a reader compares against.
fn primary_committed_occurrences(
    repository: &RepositorySlug,
    baseline: &WebhookShadowBaseline,
    observation: &RepoWatchObservation,
) -> Result<
    (
        Vec<RepoWatchEventOccurrenceV1>,
        RepoWatchEventIdentityFrontierV1,
    ),
    RepositoryWatchAttemptError,
> {
    let mut identity_frontier = baseline.identity_frontier.clone();
    let occurrences = derive_repo_watch_events_with_merged_baselines(
        repository,
        Some(&baseline.observation),
        &baseline.merged_pull_request_baselines,
        observation,
        &mut identity_frontier,
        &mut UuidV7RepoWatchEventIdGenerator,
    )
    .map_err(|_| RepositoryWatchAttemptError::Differ)?;
    Ok((occurrences, identity_frontier))
}

/// Derives one delivery's shadow projections and the frontier they advance to.
///
/// The baseline is read rather than advanced, so a delivery whose disposition
/// fails to record leaves the repository's accumulated shadow exactly as it was.
fn shadow_event_projections(
    repository: &RepositorySlug,
    baseline: &WebhookShadowBaseline,
    observation: &RepoWatchObservation,
    cause: Option<RepoWatchWebhookParityCauseV1>,
) -> Result<
    (
        Vec<RepoWatchWebhookProjection>,
        RepoWatchEventIdentityFrontierV1,
    ),
    RepositoryWatchAttemptError,
> {
    let mut identity_frontier = baseline.identity_frontier.clone();
    let projections = derive_repo_watch_events_with_merged_baselines(
        repository,
        Some(&baseline.observation),
        &baseline.merged_pull_request_baselines,
        observation,
        &mut identity_frontier,
        &mut UuidV7RepoWatchEventIdGenerator,
    )
    .map_err(|_| RepositoryWatchAttemptError::Differ)?
    .into_iter()
    // A delivery carries neither computed mergeability nor an aggregate check
    // rollup, so an occurrence of either kind here is an artefact of rebuilding
    // state rather than something the payload observed. Projecting it would
    // invent a webhook-only row for a value only polling can supply.
    .filter(|occurrence| {
        !matches!(
            occurrence.event().kind().name(),
            RepoWatchEventKindNameV1::MergeableStateChanged
                | RepoWatchEventKindNameV1::ChecksCompleted
        )
    })
    .map(|occurrence| {
        let content_identity = occurrence.content_identity();
        RepoWatchWebhookProjection::event(
            content_identity,
            occurrence.event().kind().name(),
            content_identity.as_bytes().to_vec(),
            cause,
        )
        .map_err(|_| RepositoryWatchAttemptError::Persistence)
    })
    .collect::<Result<Vec<_>, _>>()?;
    Ok((projections, identity_frontier))
}

fn targeted_query_projection(
    refresh: &RepoWatchTargetedRefreshV1,
) -> Result<RepoWatchWebhookProjection, RepositoryWatchAttemptError> {
    Ok(RepoWatchWebhookProjection::TargetedQuery(match refresh {
        RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request } => {
            RepoWatchWebhookTargetedQuery::PullRequestHydration(*pull_request)
        }
        RepoWatchTargetedRefreshV1::Mergeability {
            pull_request,
            expected_head: _,
        } => RepoWatchWebhookTargetedQuery::Mergeability(*pull_request),
        RepoWatchTargetedRefreshV1::CheckRollup {
            pull_request: _,
            expected_head,
        }
        | RepoWatchTargetedRefreshV1::CheckRollupForCommit {
            head: expected_head,
        } => RepoWatchWebhookTargetedQuery::CheckRollup(expected_head.clone()),
    }))
}

fn targeted_pull_requests(
    previous: &RepoWatchObservation,
    merged_pull_request_baselines: &[RepoWatchMergedPullRequestBaselineV1],
    refreshes: &[RepoWatchTargetedRefreshV1],
) -> Result<Vec<TargetedPullRequest>, RepositoryWatchAttemptError> {
    let mut targets = BTreeMap::new();
    for refresh in refreshes {
        match refresh {
            RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request } => {
                insert_targeted_pull_request(&mut targets, *pull_request, None)?;
            }
            RepoWatchTargetedRefreshV1::Mergeability {
                pull_request,
                expected_head,
            }
            | RepoWatchTargetedRefreshV1::CheckRollup {
                pull_request,
                expected_head,
            } => {
                insert_targeted_pull_request(
                    &mut targets,
                    *pull_request,
                    Some(expected_head.clone()),
                )?;
            }
            RepoWatchTargetedRefreshV1::CheckRollupForCommit { head } => {
                for pull_request in previous.state().pull_requests() {
                    if pull_request.context().head_sha() == head {
                        insert_targeted_pull_request(
                            &mut targets,
                            pull_request.context().number(),
                            Some(head.clone()),
                        )?;
                    }
                }
                for baseline in merged_pull_request_baselines {
                    if baseline.head_sha() == head {
                        insert_targeted_pull_request(
                            &mut targets,
                            baseline.number(),
                            Some(head.clone()),
                        )?;
                    }
                }
            }
        }
    }
    Ok(targets.into_values().collect())
}

/// Whether one requested refresh names a pull request the poller will fetch.
fn refresh_reaches_a_target(
    refresh: &RepoWatchTargetedRefreshV1,
    targets: &[TargetedPullRequest],
) -> bool {
    match refresh {
        RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request }
        | RepoWatchTargetedRefreshV1::Mergeability { pull_request, .. }
        | RepoWatchTargetedRefreshV1::CheckRollup { pull_request, .. } => {
            targets.iter().any(|target| target.number == *pull_request)
        }
        RepoWatchTargetedRefreshV1::CheckRollupForCommit { head } => targets
            .iter()
            .any(|target| target.expected_head.as_ref() == Some(head)),
    }
}

fn insert_targeted_pull_request(
    targets: &mut BTreeMap<u64, TargetedPullRequest>,
    number: PullRequestNumber,
    expected_head: Option<CommitSha>,
) -> Result<(), RepositoryWatchAttemptError> {
    match targets.entry(number.get()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(TargetedPullRequest {
                number,
                expected_head,
            });
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let retained = entry.get_mut();
            match (&retained.expected_head, expected_head) {
                (None, Some(expected)) => retained.expected_head = Some(expected),
                (Some(retained), Some(expected)) if retained != &expected => {
                    return Err(RepositoryWatchAttemptError::Normalization);
                }
                (None, None) | (Some(_), None) | (Some(_), Some(_)) => {}
            }
        }
    }
    Ok(())
}

const fn webhook_mapping_error_code(error: RepoWatchWebhookMappingError) -> &'static str {
    match error {
        RepoWatchWebhookMappingError::MalformedJson => "malformed_json",
        RepoWatchWebhookMappingError::MissingField(_) => "missing_field",
        RepoWatchWebhookMappingError::InvalidField(_) => "invalid_field",
        RepoWatchWebhookMappingError::RepositoryMismatch => "repository_mismatch",
        RepoWatchWebhookMappingError::ActionMismatch => "action_mismatch",
    }
}

const fn webhook_ignored_reason_code(reason: RepoWatchWebhookIgnoredReasonV1) -> &'static str {
    match reason {
        RepoWatchWebhookIgnoredReasonV1::UnmappedEvent => "unmapped_event",
        RepoWatchWebhookIgnoredReasonV1::UnmappedAction => "unmapped_action",
        RepoWatchWebhookIgnoredReasonV1::NonBranchPush => "non_branch_push",
        RepoWatchWebhookIgnoredReasonV1::ForeignWorkflowRepository => "foreign_workflow_repository",
        RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowBranch => "absent_workflow_branch",
        RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadRepository => {
            "absent_workflow_head_repository"
        }
        RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadBranch => "absent_workflow_head_branch",
    }
}

const fn webhook_no_change_code(reason: RepoWatchWebhookMappedNoChangeV1) -> &'static str {
    match reason {
        RepoWatchWebhookMappedNoChangeV1::Ping => "ping",
        RepoWatchWebhookMappedNoChangeV1::ReviewDismissed => "review_dismissed",
    }
}

struct RepoWatchDispatchPersistence<'configuration> {
    store: PostgresRepoWatchDispatchStore,
    models: &'configuration HubModelConfiguration,
    obligation: Option<RepoWatchDispatchObligation>,
}

impl RepoWatchDispatchTransaction for RepoWatchDispatchPersistence<'_> {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
        ids: &mut (impl SubmitInputIdGenerator + Send),
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        let select_definition = |alias: ModelAlias| self.models.resolve_alias(alias);
        match self.obligation.take() {
            Some(obligation) => {
                self.store
                    .handle_repo_watch_obligation_with_alias_resolver(
                        obligation,
                        evaluation,
                        ids,
                        select_definition,
                    )
                    .await
            }
            None => {
                self.store
                    .handle_repo_watch_evaluation_with_alias_resolver(
                        evaluation,
                        ids,
                        select_definition,
                    )
                    .await
            }
        }
    }
}

fn dispatch_context_json(event: &RepoWatchEvent) -> String {
    dispatch_context_value(event).to_string()
}

fn dispatch_context_value(event: &RepoWatchEvent) -> serde_json::Value {
    let event_value = serde_json::json!({
        "version": 1,
        "id": event.id().as_uuid().to_string(),
        "repo": event.repository().as_str(),
        "target": event_target_json(event.target()),
        "kind": event_kind_name(event.kind()),
        "payload": event_payload_json(event.kind()),
    });
    match event.target() {
        RepoWatchEventTarget::PullRequest(context) => serde_json::json!({
            "type": "pull_request",
            "repo": event.repository().as_str(),
            "number": context.number().get(),
            "head_sha": context.head_sha().as_str(),
            "event": event_value,
        }),
        RepoWatchEventTarget::Branch => {
            let RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            } = event.kind()
            else {
                return event_value;
            };
            serde_json::json!({
                "type": "branch",
                "repo": event.repository().as_str(),
                "branch": branch.as_str(),
                "workflow": workflow.as_str(),
                "conclusion": check_conclusion_name(*conclusion),
                "event": event_value,
            })
        }
    }
}

fn owed_dispatch_context_json(
    obligation: &RepoWatchDispatchObligation,
    observation: &RepoWatchObservation,
) -> String {
    owed_dispatch_context_json_parts(
        obligation.latest_event(),
        obligation.id(),
        obligation.first_event_id(),
        obligation.matched_event_count(),
        observation,
    )
}

fn owed_dispatch_context_json_parts(
    event: &RepoWatchEvent,
    obligation_id: uuid::Uuid,
    first_event_id: signalbox_domain::RepoWatchEventId,
    matched_event_count: u64,
    observation: &RepoWatchObservation,
) -> String {
    let mut context = dispatch_context_value(event);
    if let Some(object) = context.as_object_mut() {
        object.insert(
            String::from("delivery"),
            serde_json::json!({
                "mode": "owed_current_state",
                "obligation_id": obligation_id.to_string(),
                "matched_event_count": matched_event_count,
                "first_event_id": first_event_id.as_uuid().to_string(),
                "latest_event_id": event.id().as_uuid().to_string(),
                "current": current_dispatch_state_json(event, observation),
            }),
        );
    }
    context.to_string()
}

fn current_dispatch_state_json(
    event: &RepoWatchEvent,
    observation: &RepoWatchObservation,
) -> serde_json::Value {
    match event.target() {
        RepoWatchEventTarget::PullRequest(context) => observation
            .state()
            .pull_requests()
            .iter()
            .find(|pull_request| pull_request.context().number() == context.number())
            .map(current_pull_request_json)
            .unwrap_or_else(|| {
                serde_json::json!({
                    "type": "pull_request",
                    "number": context.number().get(),
                    "present": false,
                })
            }),
        RepoWatchEventTarget::Branch => current_branch_state_json(event, observation),
    }
}

fn current_pull_request_json(pull_request: &RepoWatchPullRequestState) -> serde_json::Value {
    serde_json::json!({
        "type": "pull_request",
        "present": true,
        "target": pull_request_context_json(pull_request.context()),
        "lifecycle": pull_request_lifecycle_name(pull_request.lifecycle()),
        "mergeable_state": mergeable_state_name(pull_request.mergeable_state()),
        "completed_check_suites": pull_request.completed_check_suites().iter().map(|suite| {
            serde_json::json!({
                "id": suite.id().get(),
                "completion_generation": suite.completion_generation().as_str(),
                "outcome": checks_outcome_name(suite.outcome()),
            })
        }).collect::<Vec<_>>(),
        "completed_check_runs": pull_request.completed_check_runs().iter().map(|run| {
            serde_json::json!({
                "id": run.id().get(),
                "completion_generation": run.completion_generation().as_str(),
                "name": run.name().as_str(),
                "conclusion": check_conclusion_name(run.conclusion()),
            })
        }).collect::<Vec<_>>(),
        "reviews": pull_request.reviews().iter().map(|review| {
            serde_json::json!({
                "id": review.id().get(),
                "reviewer": review.reviewer().as_str(),
                "state": review.state().map(review_state_name),
                "commit": review.commit().as_str(),
            })
        }).collect::<Vec<_>>(),
        "threads": pull_request.threads().iter().map(|thread| {
            serde_json::json!({
                "thread": thread.thread().as_str(),
                "state": review_thread_state_name(thread.state()),
            })
        }).collect::<Vec<_>>(),
        "reactions": pull_request.reactions().iter().map(|reaction| {
            serde_json::json!({
                "subject": reaction_subject_json(reaction.subject()),
                "reactor": reaction.reactor().as_str(),
                "content": reaction.content().as_str(),
            })
        }).collect::<Vec<_>>(),
    })
}

fn current_branch_state_json(
    event: &RepoWatchEvent,
    observation: &RepoWatchObservation,
) -> serde_json::Value {
    let RepoWatchEventKindV1::BranchWorkflowRunCompleted {
        branch, workflow, ..
    } = event.kind()
    else {
        return serde_json::json!({ "type": "branch", "present": false });
    };
    serde_json::json!({
        "type": "branch",
        "branch": branch.as_str(),
        "head_sha": observation.state().branch_heads().iter()
            .find(|head| head.branch() == branch)
            .map(|head| head.head().as_str()),
        "workflow": workflow.as_str(),
        "completed_runs": observation.state().workflow_runs().iter()
            .filter(|run| run.branch() == branch && run.workflow() == workflow)
            .map(|run| serde_json::json!({
                "id": run.id().get(),
                "workflow_id": run.workflow_id().get(),
                "attempt": run.attempt().get(),
                "conclusion": check_conclusion_name(run.conclusion()),
            }))
            .collect::<Vec<_>>(),
    })
}

fn pull_request_context_json(context: &PullRequestEventContext) -> serde_json::Value {
    serde_json::json!({
        "number": context.number().get(),
        "head_sha": context.head_sha().as_str(),
        "head_repo": context.head_repository().as_str(),
        "base_branch": context.base_branch().as_str(),
        "head_branch": context.head_branch().as_str(),
        "title": context.title().as_str(),
        "body": context.body().as_str(),
        "labels": context.labels().iter().map(LabelName::as_str).collect::<Vec<_>>(),
        "draft": context.draft(),
        "author": context.author().map(RepoWatchAuthorLogin::as_str),
    })
}

fn event_target_json(target: &RepoWatchEventTarget) -> serde_json::Value {
    match target {
        RepoWatchEventTarget::PullRequest(context) => serde_json::json!({
            "type": "pull_request",
            "number": context.number().get(),
            "head_sha": context.head_sha().as_str(),
            "head_repo": context.head_repository().as_str(),
            "base_branch": context.base_branch().as_str(),
            "head_branch": context.head_branch().as_str(),
            "title": context.title().as_str(),
            "body": context.body().as_str(),
            "labels": context.labels().iter().map(LabelName::as_str).collect::<Vec<_>>(),
            "draft": context.draft(),
            "author": context.author().map(RepoWatchAuthorLogin::as_str),
        }),
        RepoWatchEventTarget::Branch => serde_json::json!({ "type": "branch" }),
    }
}

const fn event_kind_name(kind: &RepoWatchEventKindV1) -> &'static str {
    match kind {
        RepoWatchEventKindV1::PullRequestOpened => "PullRequestOpened",
        RepoWatchEventKindV1::PullRequestClosed => "PullRequestClosed",
        RepoWatchEventKindV1::PullRequestMerged => "PullRequestMerged",
        RepoWatchEventKindV1::HeadChanged { .. } => "HeadChanged",
        RepoWatchEventKindV1::MergeableStateChanged { .. } => "MergeableStateChanged",
        RepoWatchEventKindV1::ChecksCompleted { .. } => "ChecksCompleted",
        RepoWatchEventKindV1::CheckRunCompleted { .. } => "CheckRunCompleted",
        RepoWatchEventKindV1::BranchWorkflowRunCompleted { .. } => "BranchWorkflowRunCompleted",
        RepoWatchEventKindV1::ReviewSubmitted { .. } => "ReviewSubmitted",
        RepoWatchEventKindV1::ThreadOpened { .. } => "ThreadOpened",
        RepoWatchEventKindV1::ThreadResolved { .. } => "ThreadResolved",
        RepoWatchEventKindV1::Labeled { .. } => "Labeled",
        RepoWatchEventKindV1::Unlabeled { .. } => "Unlabeled",
        RepoWatchEventKindV1::BaseAdvanced { .. } => "BaseAdvanced",
        RepoWatchEventKindV1::ReactionChanged { .. } => "ReactionChanged",
    }
}

fn event_payload_json(kind: &RepoWatchEventKindV1) -> serde_json::Value {
    match kind {
        RepoWatchEventKindV1::PullRequestOpened
        | RepoWatchEventKindV1::PullRequestClosed
        | RepoWatchEventKindV1::PullRequestMerged => serde_json::json!({}),
        RepoWatchEventKindV1::HeadChanged { previous, current } => serde_json::json!({
            "previous": previous.as_str(), "current": current.as_str()
        }),
        RepoWatchEventKindV1::MergeableStateChanged { current } => {
            serde_json::json!({ "current": mergeable_state_name(*current) })
        }
        RepoWatchEventKindV1::ChecksCompleted { outcome } => {
            serde_json::json!({ "outcome": checks_outcome_name(*outcome) })
        }
        RepoWatchEventKindV1::CheckRunCompleted { name, conclusion } => serde_json::json!({
            "name": name.as_str(), "conclusion": check_conclusion_name(*conclusion)
        }),
        RepoWatchEventKindV1::BranchWorkflowRunCompleted {
            branch,
            workflow,
            conclusion,
        } => serde_json::json!({
            "branch": branch.as_str(),
            "workflow": workflow.as_str(),
            "conclusion": check_conclusion_name(*conclusion),
        }),
        RepoWatchEventKindV1::ReviewSubmitted {
            reviewer,
            state,
            commit,
        } => serde_json::json!({
            "reviewer": reviewer.as_str(),
            "state": review_state_name(*state),
            "commit": commit.as_str(),
        }),
        RepoWatchEventKindV1::ThreadOpened { thread }
        | RepoWatchEventKindV1::ThreadResolved { thread } => {
            serde_json::json!({ "thread": thread.as_str() })
        }
        RepoWatchEventKindV1::Labeled { label } | RepoWatchEventKindV1::Unlabeled { label } => {
            serde_json::json!({ "label": label.as_str() })
        }
        RepoWatchEventKindV1::BaseAdvanced { branch } => {
            serde_json::json!({ "branch": branch.as_str() })
        }
        RepoWatchEventKindV1::ReactionChanged {
            subject,
            reactor,
            content,
            change,
        } => serde_json::json!({
            "subject": reaction_subject_json(*subject),
            "reactor": reactor.as_str(),
            "content": content.as_str(),
            "change": reaction_change_name(*change),
        }),
    }
}

fn reaction_subject_json(subject: ReactionSubject) -> serde_json::Value {
    match subject {
        ReactionSubject::PullRequestBody => serde_json::json!({ "type": "pull_request_body" }),
        ReactionSubject::IssueComment { id } => {
            serde_json::json!({ "type": "issue_comment", "id": id.get() })
        }
        ReactionSubject::ReviewComment { id } => {
            serde_json::json!({ "type": "review_comment", "id": id.get() })
        }
    }
}

const fn checks_outcome_name(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

const fn check_conclusion_name(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

const fn mergeable_state_name(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn review_state_name(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

const fn pull_request_lifecycle_name(value: RepoWatchPullRequestLifecycle) -> &'static str {
    match value {
        RepoWatchPullRequestLifecycle::Open => "open",
        RepoWatchPullRequestLifecycle::Closed => "closed",
        RepoWatchPullRequestLifecycle::Merged => "merged",
    }
}

const fn review_thread_state_name(value: RepoWatchThreadState) -> &'static str {
    match value {
        RepoWatchThreadState::Open => "open",
        RepoWatchThreadState::Resolved => "resolved",
    }
}

const fn reaction_change_name(value: ReactionChange) -> &'static str {
    match value {
        ReactionChange::Added => "added",
        ReactionChange::Removed => "removed",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryWatchAttemptError {
    Credential,
    Request,
    Rejected,
    ProviderUnavailable,
    ResponseTooLarge,
    InvalidResponse,
    InvalidEntityTag,
    MissingCachedResource,
    ResourceLimit,
    Normalization,
    PullRequestFetchAbandoned,
    Differ,
    IdentityFrontier,
    Dispatch,
    Persistence,
    WebhookDrainTimedOut,
    WebhookAttemptTimedOut,
    RetiredRuleIdentity,
    ChangedRuleIdentity,
    RegressedRuleVersion,
}

impl RepositoryWatchAttemptError {
    const fn cause_code(self) -> &'static str {
        match self {
            Self::Credential => "credential_unavailable",
            Self::Request => "github_request_failed",
            Self::Rejected => "github_request_rejected",
            Self::ProviderUnavailable => "github_provider_unavailable",
            Self::ResponseTooLarge => "github_response_too_large",
            Self::InvalidResponse => "github_response_invalid",
            Self::InvalidEntityTag => "github_entity_tag_invalid",
            Self::MissingCachedResource => "github_not_modified_without_accepted_state",
            Self::ResourceLimit => "repository_resource_limit_exceeded",
            Self::Normalization => "repository_state_invalid",
            Self::PullRequestFetchAbandoned => "repository_pull_request_fetch_abandoned",
            Self::Differ => "repository_differ_failed",
            Self::IdentityFrontier => "repository_identity_frontier_exhausted",
            Self::Dispatch => "repository_dispatch_failed",
            Self::Persistence => "repository_watch_persistence_failed",
            Self::WebhookDrainTimedOut => "webhook_projection_drain_timed_out",
            Self::WebhookAttemptTimedOut => "webhook_attempt_timed_out",
            Self::RetiredRuleIdentity => "repository_watch_rule_identity_retired",
            Self::ChangedRuleIdentity => "repository_watch_rule_identity_changed",
            Self::RegressedRuleVersion => "repository_watch_rule_version_regressed",
        }
    }

    /// Whether this failure should stop repository watching altogether.
    ///
    /// This is not "retrying cannot help". Returning `true` ends this
    /// repository's task, and `run_repository_watch` answers a task that ends
    /// before shutdown by aborting every other repository task and reporting
    /// `RepositoryTaskExited`, which the runtime treats as a lifecycle defect.
    /// The blast radius is therefore all repository watching, so only a
    /// failure that indicts the configuration behind every task belongs here.
    ///
    /// A rule identity that no longer matches its durable record is such a
    /// failure: the rules this daemon was started with no longer describe the
    /// database, and continuing would dispatch against a stale contract.
    ///
    /// An exhausted identity frontier is not. `StreamLimit` refuses only an
    /// observation that introduces a stream the frontier has never counted;
    /// streams already counted keep advancing at the ceiling, so a later
    /// observation that adds no new stream — the new label or reaction is
    /// removed, say — succeeds. Stopping on it would let one repository's
    /// transient over-limit observation disable every other repository's watch
    /// until restart. The recorded `repository_identity_frontier_exhausted`
    /// cause code is the operator's signal instead.
    const fn is_permanent(self) -> bool {
        match self {
            Self::RetiredRuleIdentity | Self::ChangedRuleIdentity | Self::RegressedRuleVersion => {
                true
            }
            Self::Credential
            | Self::Request
            | Self::Rejected
            | Self::ProviderUnavailable
            | Self::ResponseTooLarge
            | Self::InvalidResponse
            | Self::InvalidEntityTag
            | Self::MissingCachedResource
            | Self::ResourceLimit
            | Self::Normalization
            | Self::PullRequestFetchAbandoned
            | Self::Differ
            | Self::IdentityFrontier
            | Self::Dispatch
            | Self::Persistence
            | Self::WebhookDrainTimedOut
            | Self::WebhookAttemptTimedOut => false,
        }
    }

    /// Whether a delivery failure proves that later provider queries in this
    /// page cannot make independent progress.
    ///
    /// Persistence and target-specific provider failures retain page isolation:
    /// a poisoned receipt must not starve a healthy peer. Credential, transport,
    /// throttling, and provider-outage failures are repository-wide, so issuing
    /// the same doomed hydration for every peer only amplifies the outage.
    ///
    /// An exhausted resource budget joins them for a reason of its own: the
    /// budgets it reports — the request count and the wire bytes — are the
    /// attempt's, and a drain page runs inside one attempt. Once either is
    /// spent every later hydration on the page is refused, so continuing only
    /// spends transfer the ledger can no longer account for to learn the same
    /// failure once per receipt. A per-target pagination ceiling reports the
    /// same variant; stopping there is the conservative reading, and the
    /// receipts it leaves undrained are durable and re-attempted.
    ///
    /// `ResponseTooLarge` stays target-specific beside it: that ceiling is one
    /// response's, and a peer's response can still fit under it.
    const fn stops_webhook_page(self) -> bool {
        matches!(
            self,
            Self::Credential | Self::Request | Self::ProviderUnavailable | Self::ResourceLimit
        )
    }
}

/// Classifies a rejected REST response.
///
/// `403` carries two unrelated meanings on this provider, and only one of them
/// is repository-wide. A throttled request is reported as `403` and stopping the
/// page is right: every later targeted request would meet the same limit. A
/// credential that is simply not scoped for one endpoint — the Checks endpoints
/// are the live case — is also `403`, and is specific to that resource. Treating
/// the second as a provider outage stops the drain at the same oldest receipt on
/// every retry, so later payload-only and ignored deliveries behind it are never
/// attempted: an ordinary under-scoped token becomes a permanent per-repository
/// drain stall.
fn rejected_response_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> RepositoryWatchAttemptError {
    if status == StatusCode::UNAUTHORIZED {
        RepositoryWatchAttemptError::Credential
    } else if status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || (status == StatusCode::FORBIDDEN && response_is_throttled(headers, body))
    {
        RepositoryWatchAttemptError::ProviderUnavailable
    } else {
        RepositoryWatchAttemptError::Rejected
    }
}

/// Whether the provider says a rejection is one of its own rate limits.
///
/// The provider documents three ordered signals, and only the first two are
/// headers: `Retry-After` for a secondary limit, an exhausted
/// `X-RateLimit-Remaining` for a primary one. The third case is a secondary
/// limit carrying neither — the documented guidance there is to wait anyway —
/// and the rejection's own message is what names it. Reading the headers alone
/// would classify that case as a permission rejection and let the drain re-issue
/// the same doomed request for every later delivery on the page, which is the
/// amplification the page-stopping predicate exists to prevent. A
/// permission-scoped rejection carries neither the headers nor the message.
fn response_is_throttled(headers: &HeaderMap, body: &[u8]) -> bool {
    headers_report_a_rate_limit(headers) || rejection_reports_a_rate_limit(body)
}

/// The two header signals, which decide on their own when either is present.
fn headers_report_a_rate_limit(headers: &HeaderMap) -> bool {
    headers.contains_key(RETRY_AFTER)
        || headers
            .get("x-ratelimit-remaining")
            .and_then(|remaining| remaining.to_str().ok())
            .and_then(|remaining| remaining.trim().parse::<u64>().ok())
            .is_some_and(|remaining| remaining == 0)
}

/// Whether classifying this rejection still depends on reading its message.
///
/// Only the `403` carrying neither header is ambiguous. Every other rejection —
/// `401`, `429`, a server error, or a `403` a header already named as throttled
/// — is decided by the status and headers, and reading its body would buy
/// nothing while a slow-streaming or stalled response held the serialized
/// repository task to the request timeout, spending the drain deadline during
/// exactly the outage that produces those statuses.
fn rejection_needs_its_message(status: StatusCode, headers: &HeaderMap) -> bool {
    status == StatusCode::FORBIDDEN && !headers_report_a_rate_limit(headers)
}

/// Whether a rejection's own message names one of the provider's rate limits.
///
/// Read from the typed error envelope rather than by scanning the response
/// bytes. A body that was read successfully but carries no message, or none that
/// parses, is not evidence of throttling and leaves the classification to the
/// status and headers. A body that could not be read at all never reaches here:
/// that is a transport failure with a page-stopping meaning of its own, and the
/// caller reports it rather than reading an empty message as a permission
/// rejection.
fn rejection_reports_a_rate_limit(body: &[u8]) -> bool {
    // The provider's two documented spellings for a secondary limit; the older
    // one still appears on the same responses.
    const MARKERS: [&str; 2] = ["secondary rate limit", "abuse detection mechanism"];
    serde_json::from_slice::<ProviderRejection>(body)
        .ok()
        .and_then(|rejection| rejection.message)
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            MARKERS.iter().any(|marker| message.contains(marker))
        })
}

/// The provider's error envelope, of which only the operator-facing message
/// carries a classification signal this runtime reads.
#[derive(Deserialize)]
struct ProviderRejection {
    #[serde(default)]
    message: Option<String>,
}

/// Classifies an `HTTP 200` GraphQL error envelope.
///
/// Throttling and provider outage are reported through this envelope rather than
/// a status code, and both are repository-wide: thread hydration runs during
/// targeted refreshes, so a page that keeps going re-issues the identical doomed
/// request for every later delivery on it. Every other error type stays
/// `Rejected` and defers only its own receipt — a query-scoped `INTERNAL`
/// failure or a missing node is not evidence that a peer's request cannot make
/// independent progress.
fn graphql_envelope_error(errors: &[GraphQlError]) -> RepositoryWatchAttemptError {
    if errors.iter().any(GraphQlError::is_repository_wide) {
        RepositoryWatchAttemptError::ProviderUnavailable
    } else {
        RepositoryWatchAttemptError::Rejected
    }
}

fn rule_activation_error(error: RepoWatchDispatchRepositoryError) -> RepositoryWatchAttemptError {
    match error {
        RepoWatchDispatchRepositoryError::ReusedRuleIdentity { .. } => {
            RepositoryWatchAttemptError::RetiredRuleIdentity
        }
        RepoWatchDispatchRepositoryError::ChangedRuleIdentity { .. } => {
            RepositoryWatchAttemptError::ChangedRuleIdentity
        }
        RepoWatchDispatchRepositoryError::RegressedRuleVersion { .. } => {
            RepositoryWatchAttemptError::RegressedRuleVersion
        }
        RepoWatchDispatchRepositoryError::Database(_)
        | RepoWatchDispatchRepositoryError::CommitAmbiguous(_)
        | RepoWatchDispatchRepositoryError::EventStore(_)
        | RepoWatchDispatchRepositoryError::SessionCreation(_)
        | RepoWatchDispatchRepositoryError::InitialInput(_)
        | RepoWatchDispatchRepositoryError::GoalCommission(_)
        | RepoWatchDispatchRepositoryError::GoalCutoff(_)
        | RepoWatchDispatchRepositoryError::Corruption(_) => {
            RepositoryWatchAttemptError::Persistence
        }
    }
}

struct GitHubRepositoryPoller {
    repository: RepositorySlug,
    signal_reviewers: Vec<RepoWatchAuthorLogin>,
    credentials: FileCredentialAccess,
    credential_reference: CredentialReference,
    client: Client,
    rest_base: Url,
    graphql_url: Url,
    cache: Mutex<PollCache>,
    freshness: Mutex<HashMap<u64, PullRequestFreshness>>,
    // The child fetches one attempt spawns. Owned here rather than by the
    // attempt future so that cancelling an attempt cannot orphan its children:
    // dropping the future aborts them and releases the lock, but they stay
    // joinable, and whoever runs next — the following attempt, or the
    // repository task on its way out — joins them before proceeding.
    fetches: tokio::sync::Mutex<JoinSet<Result<FetchedPullRequest, RepositoryWatchAttemptError>>>,
}

async fn drain_pull_request_fetches<T: 'static>(
    fetches: &mut JoinSet<Result<T, RepositoryWatchAttemptError>>,
) -> Result<(), RepositoryWatchAttemptError> {
    timeout(WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT, fetches.shutdown())
        .await
        .map_err(|_| RepositoryWatchAttemptError::PullRequestFetchAbandoned)
}

struct PullRequestFreshness {
    updated_at: String,
    head_sha: CommitSha,
    settlement: PullRequestSettlement,
    gating_check_inventory: Vec<String>,
    skipped_polls: usize,
    // A fetch that never reached the durable cursor must not authorize reuse:
    // the next attempt would compare this updated_at against a stale committed
    // observation and skip the very changes that failed to commit. Reuse
    // therefore consults published entries only, so a forgotten publication
    // costs the optimization rather than an observation.
    published_generation: Option<RepoWatchCursorGeneration>,
}

#[derive(Clone)]
struct ListedPullRequest {
    updated_at: String,
    head_sha: CommitSha,
}

struct FetchedPullRequest {
    state: RepoWatchPullRequestState,
    settlement: PullRequestSettlement,
    convergence_evidence: FetchedConvergenceEvidence,
}

struct FetchedConvergenceEvidence {
    base_revision: CommitSha,
    gating_checks_settled: bool,
    gating_check_inventory_quiesced: bool,
    gating_check_inventory: Vec<String>,
    review_decision: RepoWatchReviewDecision,
    gating_check_count: u64,
    non_green_gating_checks: Vec<CheckRunName>,
}

impl FetchedConvergenceEvidence {
    fn assess(
        self,
        state: &RepoWatchPullRequestState,
        base_revision: CommitSha,
    ) -> Result<RepoWatchConvergenceAssessment, RepositoryWatchAttemptError> {
        if self.base_revision != base_revision {
            return Err(RepositoryWatchAttemptError::InvalidResponse);
        }
        RepoWatchConvergenceAssessment::try_new(RepoWatchConvergenceAssessmentInput {
            number: state.context().number(),
            head_sha: state.context().head_sha().clone(),
            base_branch: state.context().base_branch().clone(),
            base_revision,
            mergeable_state: state.mergeable_state(),
            settled: self.gating_checks_settled
                && self.gating_check_inventory_quiesced
                && state.mergeable_state() != MergeableState::Unknown,
            review_decision: self.review_decision,
            unresolved_threads: state
                .threads()
                .iter()
                .filter(|thread| thread.state() == RepoWatchThreadState::Open)
                .map(|thread| thread.thread().clone())
                .collect(),
            gating_check_count: self.gating_check_count,
            non_green_gating_checks: self.non_green_gating_checks,
        })
        .map_err(|_| RepositoryWatchAttemptError::Normalization)
    }
}

struct PolledRepository {
    observation: RepoWatchObservation,
    convergence: Vec<RepoWatchConvergenceAssessment>,
    stale_review_clearances: Vec<RepoWatchStaleReviewClearanceCandidate>,
}

#[derive(Debug)]
struct FetchedPullRequests {
    states: Vec<RepoWatchPullRequestState>,
    convergence: Vec<RepoWatchConvergenceAssessment>,
    stale_review_clearances: Vec<RepoWatchStaleReviewClearanceCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PullRequestSettlement {
    Settled,
    Unsettled,
}

impl GitHubRepositoryPoller {
    fn try_new(
        repository: RepositorySlug,
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
        credentials: FileCredentialAccess,
        credential_reference: CredentialReference,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let rest_base =
            Url::parse(REST_BASE_URL).map_err(|_| RepositoryWatchRuntimeConstructionError)?;
        Self::try_new_with_rest_base(
            repository,
            signal_reviewers,
            credentials,
            credential_reference,
            rest_base,
        )
    }

    fn try_new_with_rest_base(
        repository: RepositorySlug,
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
        credentials: FileCredentialAccess,
        credential_reference: CredentialReference,
        rest_base: Url,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| RepositoryWatchRuntimeConstructionError)?;
        let graphql_url = rest_base
            .join("graphql")
            .map_err(|_| RepositoryWatchRuntimeConstructionError)?;
        Ok(Self {
            repository,
            signal_reviewers,
            credentials,
            credential_reference,
            client,
            rest_base,
            graphql_url,
            cache: Mutex::new(PollCache::default()),
            freshness: Mutex::new(HashMap::new()),
            fetches: tokio::sync::Mutex::new(JoinSet::new()),
        })
    }

    fn cache(&self) -> MutexGuard<'_, PollCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn attempt_metrics(&self) -> PollAttemptMetrics {
        let cache = self.cache();
        PollAttemptMetrics {
            requests: cache.requests,
            poll_wire_bytes: cache.poll_wire_bytes,
            cached_resources: cache.resources.len(),
            cached_wire_bytes: cache.cached_wire_bytes,
        }
    }

    fn begin_attempt(&self) {
        self.cache().begin_attempt();
    }

    #[cfg(test)]
    async fn poll(
        self: &Arc<Self>,
        previous: Option<&RepoWatchObservation>,
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
        Ok(self
            .poll_against_cursor(previous, Some(RepoWatchCursorGeneration::INITIAL))
            .await?
            .observation)
    }

    async fn poll_against_cursor(
        self: &Arc<Self>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> Result<PolledRepository, RepositoryWatchAttemptError> {
        self.cache().begin_poll();
        let result = self.poll_complete(previous, cursor_generation).await;
        if result.is_ok() {
            self.cache().complete_poll();
        }
        result
    }

    async fn poll_targeted_pull_requests_against_cursor(
        &self,
        previous: &RepoWatchObservation,
        targets: &[TargetedPullRequest],
    ) -> Result<TargetedPollOutcome, RepositoryWatchAttemptError> {
        // A cancelled complete poll can leave child fetches in the shared set.
        // Settle them within the scheduler bound before issuing targeted
        // requests, so work from two attempts cannot interleave.
        let drained_survivors = self.drain_fetches_bounded().await?;
        // A survivor can record freshness after the cancellation path's first
        // invalidation and before this join completes. Clear that late state
        // before targeted work can publish it against a new cursor, while an
        // ordinary targeted refresh preserves published freshness for untouched
        // pull requests.
        if drained_survivors {
            self.invalidate_freshness();
        }
        let mut state = RepoWatchRepositoryStateInput {
            pull_requests: previous.state().pull_requests().to_vec(),
            workflow_runs: previous.state().workflow_runs().to_vec(),
            branch_heads: previous.state().branch_heads().to_vec(),
        };
        let mut superseded_targets = Vec::new();
        for target in targets {
            let retained_index = state
                .pull_requests
                .iter()
                .position(|pull_request| pull_request.context().number() == target.number);
            let retained = retained_index.map(|index| state.pull_requests[index].clone());
            self.forget_pull_request(target.number.get());
            let fetched = self
                .fetch_pull_request(target.number.get(), retained.as_ref())
                .await?;
            if target
                .expected_head
                .as_ref()
                .is_some_and(|expected| fetched.state.context().head_sha() != expected)
            {
                // The provider has already moved past the head this target
                // expected. A multi-target refresh keeps reconciling the
                // targets that still match; the caller drops this one so
                // retained state is never committed as if it were fetched.
                tracing::debug!(
                    repository = %self.repository.as_str(),
                    pull_request = target.number.get(),
                    "targeted repository refresh was superseded before its response"
                );
                superseded_targets.push(target.number);
                continue;
            }
            match retained_index {
                Some(index) => state.pull_requests[index] = fetched.state,
                None => state.pull_requests.push(fetched.state),
            }
        }
        if superseded_targets.len() == targets.len() {
            // Every target moved: nothing this delivery hydrated is current,
            // so the caller records the whole delivery superseded.
            return Ok(TargetedPollOutcome::SupersededTarget);
        }
        let state = RepoWatchRepositoryState::try_new(state)
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        Ok(TargetedPollOutcome::Observation {
            observation: RepoWatchObservation::new(self.signal_reviewers.clone(), state),
            superseded_targets,
        })
    }

    async fn poll_complete(
        self: &Arc<Self>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> Result<PolledRepository, RepositoryWatchAttemptError> {
        let listed = self.fetch_open_pull_numbers().await?;
        let mut pull_numbers: BTreeSet<u64> = listed.keys().copied().collect();
        if let Some(previous) = previous {
            for pull_request in previous.state().pull_requests() {
                if pull_request.lifecycle() == RepoWatchPullRequestLifecycle::Open {
                    pull_numbers.insert(pull_request.context().number().get());
                }
            }
        }
        // Anchor convergence assessments to the same branch-head snapshot that
        // will be committed in the cursor. Pull-request hydration is the long
        // phase of a poll, so reading branch heads afterwards creates a broad
        // window in which an ordinary base advance invalidates all evidence.
        let branch_heads = self.fetch_branch_heads().await?;
        let pull_requests = self
            .fetch_pull_requests(
                pull_numbers,
                &listed,
                previous,
                cursor_generation,
                &branch_heads,
            )
            .await?;
        let workflows = self.fetch_workflows().await?;
        let mut workflow_runs = Vec::new();
        let previous_workflow_runs = previous
            .map(|observation| observation.state().workflow_runs())
            .unwrap_or_default();
        for workflow in &workflows {
            workflow_runs.extend(
                self.fetch_workflow_runs(&branch_heads, workflow, previous_workflow_runs)
                    .await?,
            );
        }
        let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: pull_requests.states,
            workflow_runs,
            branch_heads,
        })
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        Ok(PolledRepository {
            observation: RepoWatchObservation::new(self.signal_reviewers.clone(), state),
            convergence: pull_requests.convergence,
            stale_review_clearances: pull_requests.stale_review_clearances,
        })
    }

    async fn fetch_pull_requests(
        self: &Arc<Self>,
        pull_numbers: BTreeSet<u64>,
        listed: &BTreeMap<u64, ListedPullRequest>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
        branch_heads: &[RepoWatchBranchHead],
    ) -> Result<FetchedPullRequests, RepositoryWatchAttemptError> {
        self.forget_unlisted_freshness(&pull_numbers);
        let mut fetches = self.fetches.lock().await;
        // A cancelled attempt drops this future mid-collection, which aborts
        // the children without joining them; they stay behind in the shared
        // set. Join any such survivor before spawning, so no child of an
        // earlier attempt can interleave with this one. A wedged survivor
        // fails this attempt back to the scheduler after a bounded wait.
        drain_pull_request_fetches(&mut fetches).await?;
        let collected = self
            .collect_pull_request_fetches(
                pull_numbers,
                listed,
                previous,
                cursor_generation,
                branch_heads,
                &mut fetches,
            )
            .await;
        // Dropping the set aborts the siblings but does not wait for them.
        // An aborted task only stops at its next await, so it can still charge
        // wire bytes, touch cache entries, or record freshness after this
        // attempt returns, landing that state in the next attempt. Wait for
        // every task to finish before the caller can begin another poll, but
        // return to the scheduler if a child does not finish cancellation.
        drain_pull_request_fetches(&mut fetches).await?;
        let mut pull_requests = collected?;
        pull_requests.sort_by_key(|pull_request| pull_request.state.context().number().get());
        let mut states = Vec::with_capacity(pull_requests.len());
        let mut convergence = Vec::with_capacity(pull_requests.len());
        let mut stale_review_clearances = Vec::new();
        for pull_request in pull_requests {
            let base_revision = branch_heads
                .iter()
                .find(|branch_head| {
                    branch_head.branch() == pull_request.state.context().base_branch()
                })
                .map(|branch_head| branch_head.head().clone())
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            let assessment = pull_request
                .convergence_evidence
                .assess(&pull_request.state, base_revision)?;
            // Clearance candidates are read from the assessment, which only
            // exists once the snapshot's base revision is known, so this runs
            // here rather than in the per-pull-request fetch task. The lookup
            // returns immediately unless a changes-requested review is the sole
            // remaining blocker, so the serial call costs nothing in the common
            // case.
            if pull_request.state.lifecycle() == RepoWatchPullRequestLifecycle::Open {
                stale_review_clearances
                    .extend(self.fetch_stale_review_clearances(&assessment).await?);
            }
            convergence.push(assessment);
            states.push(pull_request.state);
        }
        Ok(FetchedPullRequests {
            states,
            convergence,
            stale_review_clearances,
        })
    }

    /// Joins every child fetch a cancelled attempt left behind. The repository
    /// task calls this after cancelling an in-flight attempt, so a reported
    /// stop means no child is still resolving credentials, holding a
    /// connection, or touching shared state.
    async fn drain_fetches_bounded(&self) -> Result<bool, RepositoryWatchAttemptError> {
        let mut fetches = self.fetches.lock().await;
        let had_fetches = !fetches.is_empty();
        drain_pull_request_fetches(&mut fetches).await?;
        Ok(had_fetches)
    }

    /// Strict shutdown settlement. A clean repository-task exit means no child
    /// fetch remains able to hold resources or mutate shared freshness state.
    async fn drain_fetches(&self) {
        self.fetches.lock().await.shutdown().await;
    }

    /// Bounds cleanup after a caller cancels a poll that may own child fetches.
    async fn drain_fetches_within(&self, deadline: Duration) -> bool {
        timeout(deadline, self.drain_fetches()).await.is_ok()
    }

    async fn collect_pull_request_fetches(
        self: &Arc<Self>,
        pull_numbers: BTreeSet<u64>,
        listed: &BTreeMap<u64, ListedPullRequest>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
        branch_heads: &[RepoWatchBranchHead],
        fetches: &mut JoinSet<Result<FetchedPullRequest, RepositoryWatchAttemptError>>,
    ) -> Result<Vec<FetchedPullRequest>, RepositoryWatchAttemptError> {
        let mut pull_requests = Vec::with_capacity(pull_numbers.len());
        let mut pending = pull_numbers.into_iter();
        loop {
            while fetches.len() < MAX_CONCURRENT_PULL_REQUEST_FETCHES {
                let Some(number) = pending.next() else {
                    break;
                };
                let poller = Arc::clone(self);
                let listed_pull_request = listed.get(&number).cloned();
                let previous_pull_request = previous
                    .and_then(|observation| previous_pull_request(observation, number))
                    .cloned();
                let base_revision_unchanged = previous_pull_request.as_ref().is_some_and(|pull| {
                    previous.is_some_and(|observation| {
                        pull_request_base_revision_matches(observation, pull, branch_heads)
                    })
                });
                fetches.spawn(async move {
                    poller
                        .fetch_or_reuse_pull_request(
                            number,
                            listed_pull_request.as_ref(),
                            previous_pull_request.as_ref(),
                            cursor_generation,
                            base_revision_unchanged,
                        )
                        .await
                });
            }
            let Some(fetched) = fetches.join_next().await else {
                break;
            };
            pull_requests.push(
                fetched.map_err(|_| RepositoryWatchAttemptError::PullRequestFetchAbandoned)??,
            );
        }
        Ok(pull_requests)
    }

    async fn fetch_open_pull_numbers(
        &self,
    ) -> Result<BTreeMap<u64, ListedPullRequest>, RepositoryWatchAttemptError> {
        let mut numbers = BTreeMap::new();
        let mut page = 1_u16;
        loop {
            let url = self.repository_url(
                &["pulls"],
                &[
                    ("state", "open".to_owned()),
                    ("per_page", PAGE_SIZE.to_string()),
                    ("page", page.to_string()),
                ],
            )?;
            let response = self
                .conditional_json_page::<Vec<PullNumberResponse>>("pulls", Method::GET, url, None)
                .await?;
            let has_next = response.has_next_page;
            for value in response.value {
                positive(value.number)?;
                let head_sha = CommitSha::try_new(value.head.sha)
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
                numbers.insert(
                    value.number,
                    ListedPullRequest {
                        updated_at: value.updated_at,
                        head_sha,
                    },
                );
            }
            if !has_next {
                return Ok(numbers);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_or_reuse_pull_request(
        &self,
        number: u64,
        listed_pull_request: Option<&ListedPullRequest>,
        previous_pull_request: Option<&RepoWatchPullRequestState>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
        base_revision_unchanged: bool,
    ) -> Result<FetchedPullRequest, RepositoryWatchAttemptError> {
        if let (Some(listed), Some(previous)) = (listed_pull_request, previous_pull_request)
            && base_revision_unchanged
            && self.pull_request_detail_is_reusable(number, listed, previous, cursor_generation)
        {
            let reviews = self.fetch_reviews(number, Some(previous.reviews())).await?;
            let mut convergence_evidence =
                self.fetch_convergence_evidence(previous.context()).await?;
            convergence_evidence.gating_check_inventory_quiesced = self
                .gating_check_inventory_quiesced(
                    number,
                    listed,
                    cursor_generation,
                    &convergence_evidence.gating_check_inventory,
                );
            let threads = self.fetch_threads(number).await?;
            let reactions = self
                .fetch_reactions(number, Some(previous.reactions()))
                .await?;
            self.record_skipped_poll(number);
            self.record_gating_check_inventory(
                number,
                listed,
                convergence_evidence.gating_check_inventory.clone(),
            );
            let state = reuse_pull_request(previous, reviews, threads, reactions)?;
            return Ok(FetchedPullRequest {
                state,
                settlement: PullRequestSettlement::Settled,
                convergence_evidence,
            });
        }
        let mut fetched = self
            .fetch_pull_request(number, previous_pull_request)
            .await?;
        match listed_pull_request {
            Some(listed) => {
                fetched.convergence_evidence.gating_check_inventory_quiesced = self
                    .gating_check_inventory_quiesced(
                        number,
                        listed,
                        cursor_generation,
                        &fetched.convergence_evidence.gating_check_inventory,
                    );
                self.record_fetched_pull_request(
                    number,
                    listed,
                    fetched.settlement,
                    fetched.convergence_evidence.gating_check_inventory.clone(),
                );
            }
            None => self.forget_pull_request(number),
        }
        Ok(fetched)
    }

    fn pull_request_detail_is_reusable(
        &self,
        number: u64,
        listed: &ListedPullRequest,
        previous: &RepoWatchPullRequestState,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> bool {
        self.freshness().get(&number).is_some_and(|freshness| {
            freshness.published_generation == cursor_generation
                && cursor_generation.is_some()
                && freshness.updated_at == listed.updated_at
                && previous.context().head_sha() == &listed.head_sha
                && freshness.settlement == PullRequestSettlement::Settled
                && freshness.skipped_polls < MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS
        })
    }

    fn record_skipped_poll(&self, number: u64) {
        if let Some(freshness) = self.freshness().get_mut(&number) {
            freshness.skipped_polls = freshness.skipped_polls.saturating_add(1);
        }
    }

    fn gating_check_inventory_quiesced(
        &self,
        number: u64,
        listed: &ListedPullRequest,
        cursor_generation: Option<RepoWatchCursorGeneration>,
        gating_check_inventory: &[String],
    ) -> bool {
        self.freshness().get(&number).is_some_and(|freshness| {
            freshness.published_generation == cursor_generation
                && cursor_generation.is_some()
                && freshness.updated_at == listed.updated_at
                && freshness.head_sha == listed.head_sha
                && freshness.gating_check_inventory == gating_check_inventory
        })
    }

    fn record_gating_check_inventory(
        &self,
        number: u64,
        listed: &ListedPullRequest,
        gating_check_inventory: Vec<String>,
    ) {
        if let Some(freshness) = self.freshness().get_mut(&number) {
            freshness.updated_at = listed.updated_at.clone();
            freshness.head_sha = listed.head_sha.clone();
            freshness.gating_check_inventory = gating_check_inventory;
            freshness.published_generation = None;
        }
    }

    fn record_fetched_pull_request(
        &self,
        number: u64,
        listed: &ListedPullRequest,
        settlement: PullRequestSettlement,
        gating_check_inventory: Vec<String>,
    ) {
        self.freshness().insert(
            number,
            PullRequestFreshness {
                updated_at: listed.updated_at.clone(),
                head_sha: listed.head_sha.clone(),
                settlement,
                gating_check_inventory,
                skipped_polls: 0,
                published_generation: None,
            },
        );
    }

    fn publish_freshness(&self, generation: RepoWatchCursorGeneration) {
        for freshness in self.freshness().values_mut() {
            freshness.published_generation = Some(generation);
        }
    }

    /// Drops every freshness entry, published or not. After a failed attempt,
    /// a competing watcher may advance the durable cursor, so entries recorded
    /// against this process's prior baseline must authorize no further reuse.
    fn invalidate_freshness(&self) {
        self.freshness().clear();
    }

    fn forget_pull_request(&self, number: u64) {
        self.freshness().remove(&number);
    }

    fn forget_unlisted_freshness(&self, polled: &BTreeSet<u64>) {
        self.freshness().retain(|number, _| polled.contains(number));
    }

    fn freshness(&self) -> MutexGuard<'_, HashMap<u64, PullRequestFreshness>> {
        self.freshness
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn fetch_pull_request(
        &self,
        number: u64,
        previous_pull_request: Option<&RepoWatchPullRequestState>,
    ) -> Result<FetchedPullRequest, RepositoryWatchAttemptError> {
        let number_text = number.to_string();
        let detail: PullResponse = self
            .conditional_json(
                "pull",
                Method::GET,
                self.repository_url(&["pulls", &number_text], &[])?,
                None,
            )
            .await?;
        if detail.number != number {
            return Err(RepositoryWatchAttemptError::InvalidResponse);
        }
        let head_sha = CommitSha::try_new(detail.head.sha.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let context = normalize_pull_request_context(
            &detail,
            head_sha.clone(),
            previous_pull_request.map(RepoWatchPullRequestState::context),
        )?;
        let (completed_check_suites, check_suite_ids) = self.fetch_check_suites(&head_sha).await?;
        let (completed_check_runs, every_run_completed) =
            self.fetch_check_runs(&head_sha, &check_suite_ids).await?;
        let mergeable_state = match detail.mergeable {
            Some(true) => MergeableState::Mergeable,
            Some(false) => MergeableState::Conflicting,
            None => MergeableState::Unknown,
        };
        // Neither a check completion nor GitHub finishing its background
        // mergeability calculation moves the listing's updated_at, so a pull
        // request is only reusable when both have already come to rest.
        let settlement = if every_run_completed
            && completed_check_suites.len() == check_suite_ids.len()
            && mergeable_state != MergeableState::Unknown
        {
            PullRequestSettlement::Settled
        } else {
            PullRequestSettlement::Unsettled
        };
        let reviews = self
            .fetch_reviews(
                number,
                previous_pull_request.map(RepoWatchPullRequestState::reviews),
            )
            .await?;
        let convergence_evidence = self.fetch_convergence_evidence(&context).await?;
        let threads = self.fetch_threads(number).await?;
        let reactions = self
            .fetch_reactions(
                number,
                previous_pull_request.map(RepoWatchPullRequestState::reactions),
            )
            .await?;
        let state = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context,
            lifecycle: normalize_lifecycle(&detail)?,
            mergeable_state,
            completed_check_suites,
            completed_check_runs,
            reviews,
            threads,
            reactions,
        })
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        Ok(FetchedPullRequest {
            state,
            settlement,
            convergence_evidence,
        })
    }

    async fn fetch_check_suites(
        &self,
        head: &CommitSha,
    ) -> Result<
        (Vec<RepoWatchCheckSuiteObservation>, Vec<GitHubObjectId>),
        RepositoryWatchAttemptError,
    > {
        let mut observations = Vec::new();
        let mut suite_ids = Vec::new();
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<CheckSuitesResponse>(
                    "check-suites",
                    Method::GET,
                    self.repository_url(
                        &["commits", head.as_str(), "check-suites"],
                        &[
                            ("filter", "all".to_owned()),
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for suite in response.value.check_suites {
                let suite_id = object_id(suite.id)?;
                suite_ids.push(suite_id);
                if suite.status == "completed" {
                    observations.push(RepoWatchCheckSuiteObservation::new(
                        suite_id,
                        RepoWatchCheckCompletionGeneration::try_new(suite.updated_at)
                            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                        normalize_checks_outcome(suite.conclusion.as_deref())?,
                    ));
                }
            }
            if !has_next {
                return Ok((observations, suite_ids));
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_check_runs(
        &self,
        head: &CommitSha,
        suite_ids: &[GitHubObjectId],
    ) -> Result<(Vec<RepoWatchCheckRunObservation>, bool), RepositoryWatchAttemptError> {
        if suite_ids.is_empty() {
            return Ok((Vec::new(), true));
        }
        if commit_check_run_search_is_complete(suite_ids.len()) {
            return self
                .fetch_check_run_pages(&["commits", head.as_str(), "check-runs"])
                .await;
        }

        let mut observations = Vec::new();
        let mut every_run_completed = true;
        for suite_id in suite_ids {
            let suite_id = suite_id.get().to_string();
            let (mut suite_observations, every_suite_run_completed) = self
                .fetch_check_run_pages(&["check-suites", &suite_id, "check-runs"])
                .await?;
            observations.append(&mut suite_observations);
            every_run_completed &= every_suite_run_completed;
        }
        Ok((observations, every_run_completed))
    }

    async fn fetch_check_run_pages(
        &self,
        suffix: &[&str],
    ) -> Result<(Vec<RepoWatchCheckRunObservation>, bool), RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut every_run_completed = true;
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<CheckRunsResponse>(
                    "check-runs",
                    Method::GET,
                    self.repository_url(
                        suffix,
                        &[
                            ("filter", "all".to_owned()),
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for run in response.value.check_runs {
                if run.status == "completed" {
                    observations.push(RepoWatchCheckRunObservation::new(
                        object_id(run.id)?,
                        RepoWatchCheckCompletionGeneration::try_new(
                            run.completed_at
                                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?,
                        )
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                        CheckRunName::try_new(run.name)
                            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                        normalize_conclusion(run.conclusion.as_deref())?,
                    ));
                } else if !is_non_gating_check_name(&run.name) {
                    every_run_completed = false;
                }
            }
            if !has_next {
                break;
            }
            page = next_page(page)?;
        }
        Ok((observations, every_run_completed))
    }

    async fn fetch_reviews(
        &self,
        number: u64,
        previous: Option<&[RepoWatchReviewObservation]>,
    ) -> Result<Vec<RepoWatchReviewObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let number_text = number.to_string();
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<Vec<ReviewResponse>>(
                    "reviews",
                    Method::GET,
                    self.repository_url(
                        &["pulls", &number_text, "reviews"],
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for review in response.value {
                let state = match normalize_review_state(&review.state)? {
                    ProviderReviewState::Submitted(state) => Some(state),
                    ProviderReviewState::Dismissed => None,
                    ProviderReviewState::Pending => continue,
                };
                let id = object_id(review.id)?;
                let reviewer = match review.user {
                    Some(user) => RepoWatchAuthorLogin::try_new(user.login)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                    None => {
                        let Some(previous) = previous.and_then(|reviews| {
                            reviews.iter().find(|candidate| candidate.id() == id)
                        }) else {
                            continue;
                        };
                        previous.reviewer().clone()
                    }
                };
                observations.push(RepoWatchReviewObservation::new(
                    id,
                    reviewer,
                    state,
                    CommitSha::try_new(review.commit_id)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                ));
            }
            if !has_next {
                return Ok(observations);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_threads(
        &self,
        number: u64,
    ) -> Result<Vec<RepoWatchThreadObservation>, RepositoryWatchAttemptError> {
        let (namespace, name) = self
            .repository
            .as_str()
            .split_once('/')
            .ok_or(RepositoryWatchAttemptError::Normalization)?;
        let namespace = namespace.to_owned();
        let name = name.to_owned();
        let number =
            i64::try_from(number).map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let mut observations = Vec::new();
        let mut after: Option<String> = None;
        let mut page = 1_u16;
        loop {
            let body = serde_json::to_vec(&GraphQlRequest {
                query: REVIEW_THREADS_QUERY,
                variables: ThreadVariables {
                    namespace: &namespace,
                    name: &name,
                    number,
                    after: after.as_deref(),
                },
            })
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
            let response: GraphQlEnvelope<ThreadData> = self
                .conditional_json(
                    "threads",
                    Method::POST,
                    self.graphql_url.clone(),
                    Some(body),
                )
                .await?;
            if !response.errors.is_empty() {
                return Err(graphql_envelope_error(&response.errors));
            }
            let connection = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .map(|pull_request| pull_request.review_threads)
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            for thread in connection.nodes {
                observations.push(RepoWatchThreadObservation::new(
                    ReviewThreadId::try_new(thread.id)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                    if thread.is_resolved {
                        RepoWatchThreadState::Resolved
                    } else {
                        RepoWatchThreadState::Open
                    },
                ));
            }
            if !connection.page_info.has_next_page {
                return Ok(observations);
            }
            after = connection.page_info.end_cursor;
            if after.is_none() {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_convergence_evidence(
        &self,
        context: &PullRequestEventContext,
    ) -> Result<FetchedConvergenceEvidence, RepositoryWatchAttemptError> {
        let (namespace, name) = self
            .repository
            .as_str()
            .split_once('/')
            .ok_or(RepositoryWatchAttemptError::Normalization)?;
        let number = i64::try_from(context.number().get())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let mut after: Option<String> = None;
        let mut page = 1_u16;
        let mut gating_check_count = 0_u64;
        let mut gating_checks_settled = true;
        let mut gating_check_inventory = Vec::new();
        let mut non_green_gating_checks = Vec::new();
        let mut retained_review_decision = None;
        let mut retained_base_revision = None;
        loop {
            let body = serde_json::to_vec(&GraphQlRequest {
                query: CONVERGENCE_QUERY,
                variables: ThreadVariables {
                    namespace,
                    name,
                    number,
                    after: after.as_deref(),
                },
            })
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
            let response: GraphQlEnvelope<ConvergenceData> = self
                .conditional_json(
                    "convergence",
                    Method::POST,
                    self.graphql_url.clone(),
                    Some(body),
                )
                .await?;
            if !response.errors.is_empty() {
                return Err(graphql_envelope_error(&response.errors));
            }
            let pull_request = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            let _provider_mergeable_state = normalize_graphql_mergeable(&pull_request.mergeable)?;
            if pull_request.head_ref_oid != context.head_sha().as_str()
                || pull_request.base_ref_name != context.base_branch().as_str()
            {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            if retained_base_revision
                .replace(pull_request.base_ref_oid.clone())
                .is_some_and(|retained| retained != pull_request.base_ref_oid)
            {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            let review_decision =
                normalize_review_decision(pull_request.review_decision.as_deref())?;
            if retained_review_decision
                .replace(review_decision)
                .is_some_and(|retained| retained != review_decision)
            {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            let [commit_node] = pull_request.commits.nodes.as_slice() else {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            };
            let commit = &commit_node.commit;
            if commit.oid != pull_request.head_ref_oid {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            let Some(rollup) = commit.status_check_rollup.as_ref() else {
                if page != 1 {
                    return Err(RepositoryWatchAttemptError::InvalidResponse);
                }
                break;
            };
            for check in &rollup.contexts.nodes {
                if check.is_report_only() {
                    continue;
                }
                gating_check_inventory.push(check.name().to_owned());
                gating_check_count = gating_check_count
                    .checked_add(1)
                    .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
                if !check.complete() {
                    gating_checks_settled = false;
                }
                if !check.green() {
                    non_green_gating_checks.push(
                        CheckRunName::try_new(check.name().to_owned())
                            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                    );
                }
            }
            if !rollup.contexts.page_info.has_next_page {
                break;
            }
            after = rollup.contexts.page_info.end_cursor.clone();
            if after.is_none() {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            page = next_page(page)?;
        }
        let base_revision = CommitSha::try_new(
            retained_base_revision.ok_or(RepositoryWatchAttemptError::InvalidResponse)?,
        )
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        gating_check_inventory.sort_unstable();
        Ok(FetchedConvergenceEvidence {
            base_revision,
            gating_checks_settled,
            // Quiescence is not a property of one check-rollup read: it takes a
            // second observation to say the inventory stopped growing. This
            // read cannot know that, so it reports the conservative default and
            // every caller replaces it with the verdict
            // `GitHubRepositoryPoller::gating_check_inventory_quiesced` reads
            // from the freshness the last committed cursor published. A caller
            // that leaves the default in place reports every head unsettled.
            gating_check_inventory_quiesced: false,
            gating_check_inventory,
            review_decision: retained_review_decision
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?,
            gating_check_count,
            non_green_gating_checks,
        })
    }

    async fn fetch_stale_review_clearances(
        &self,
        assessment: &RepoWatchConvergenceAssessment,
    ) -> Result<Vec<RepoWatchStaleReviewClearanceCandidate>, RepositoryWatchAttemptError> {
        // Mirrors the candidate rule so a head that cannot yield a candidate
        // costs no provider request. The domain type re-checks every gate.
        if assessment.review_decision() != RepoWatchReviewDecision::ChangesRequested
            || !assessment.unresolved_threads().is_empty()
            || !assessment.non_green_gating_checks().is_empty()
            || !assessment.settled()
            || assessment.gating_check_count() == 0
            || assessment.mergeable_state() == MergeableState::Conflicting
        {
            return Ok(Vec::new());
        }
        let (namespace, name) = self
            .repository
            .as_str()
            .split_once('/')
            .ok_or(RepositoryWatchAttemptError::Normalization)?;
        let number = i64::try_from(assessment.number().get())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let mut after: Option<String> = None;
        let mut page = 1_u16;
        let mut candidates = Vec::new();
        loop {
            let body = serde_json::to_vec(&GraphQlRequest {
                query: BLOCKING_REVIEWS_QUERY,
                variables: ThreadVariables {
                    namespace,
                    name,
                    number,
                    after: after.as_deref(),
                },
            })
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
            let response: GraphQlEnvelope<BlockingReviewData> = self
                .conditional_json(
                    "blocking-reviews",
                    Method::POST,
                    self.graphql_url.clone(),
                    Some(body),
                )
                .await?;
            if !response.errors.is_empty() {
                return Err(graphql_envelope_error(&response.errors));
            }
            let pull_request = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            if pull_request.head_ref_oid != assessment.head_sha().as_str()
                || pull_request.base_ref_oid != assessment.base_revision().as_str()
                || normalize_review_decision(pull_request.review_decision.as_deref())?
                    != RepoWatchReviewDecision::ChangesRequested
            {
                return Ok(Vec::new());
            }
            for review in pull_request.latest_opinionated_reviews.nodes {
                if review.state != "CHANGES_REQUESTED" {
                    continue;
                }
                let Some(author) = review.author else {
                    return Ok(Vec::new());
                };
                let reviewer = RepoWatchAuthorLogin::try_new(author.login)
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
                let Some(commit) = review.commit else {
                    return Ok(Vec::new());
                };
                let reviewed_head_sha = CommitSha::try_new(commit.oid)
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
                if &reviewed_head_sha == assessment.head_sha() {
                    return Ok(Vec::new());
                }
                candidates.push(
                    RepoWatchStaleReviewClearanceCandidate::try_new(
                        assessment,
                        review.id,
                        reviewer,
                        reviewed_head_sha,
                    )
                    .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?,
                );
            }
            if !pull_request
                .latest_opinionated_reviews
                .page_info
                .has_next_page
            {
                candidates.sort_by(|left, right| left.review_node_id().cmp(right.review_node_id()));
                return Ok(candidates);
            }
            after = pull_request.latest_opinionated_reviews.page_info.end_cursor;
            if after.is_none() {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            page = next_page(page)?;
        }
    }

    /// Re-reads the provider immediately before a dismissal and reports
    /// whether the planned clearance still holds against live evidence.
    ///
    /// `cursor_generation` is the generation the poll that raised this
    /// candidate committed, and the freshness it published is what proves the
    /// gating-check inventory has stood still: a candidate is only admissible
    /// when the inventory this re-read observes is the one that committed
    /// generation recorded for the same head and update stamp.
    async fn revalidate_stale_review_clearance(
        &self,
        clearance: &RepoWatchPlannedStaleReviewClearance,
        cursor_generation: RepoWatchCursorGeneration,
    ) -> Result<bool, RepositoryWatchAttemptError> {
        let number_text = clearance.number().get().to_string();
        let detail: PullResponse = self
            .conditional_json(
                "pull-clearance-revalidation",
                Method::GET,
                self.repository_url(&["pulls", &number_text], &[])?,
                None,
            )
            .await?;
        if detail.number != clearance.number().get()
            || normalize_lifecycle(&detail)? != RepoWatchPullRequestLifecycle::Open
        {
            return Ok(false);
        }
        let head_sha = CommitSha::try_new(detail.head.sha.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        if &head_sha != clearance.current_head_sha() {
            return Ok(false);
        }
        let base_branch = BranchName::try_new(detail.base.reference.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        if &base_branch != clearance.base_branch() {
            return Ok(false);
        }
        let mergeable_state = match detail.mergeable {
            Some(true) => MergeableState::Mergeable,
            Some(false) => MergeableState::Conflicting,
            None => MergeableState::Unknown,
        };
        let context = normalize_pull_request_context(&detail, head_sha.clone(), None)?;
        let mut evidence = self.fetch_convergence_evidence(&context).await?;
        // The same quiescence rule the polling path applies, against the same
        // published freshness. Here the two observations being compared are the
        // committed poll that raised this candidate and this pre-dismissal
        // re-read, so a gating check that appeared in between leaves the head
        // unsettled and the review undismissed until a later poll sees the
        // inventory hold still.
        let listed = ListedPullRequest {
            updated_at: detail.updated_at.clone(),
            head_sha: head_sha.clone(),
        };
        evidence.gating_check_inventory_quiesced = self.gating_check_inventory_quiesced(
            clearance.number().get(),
            &listed,
            Some(cursor_generation),
            &evidence.gating_check_inventory,
        );
        if &evidence.base_revision != clearance.base_revision()
            || evidence.review_decision != RepoWatchReviewDecision::ChangesRequested
            || !evidence.non_green_gating_checks.is_empty()
            || mergeable_state == MergeableState::Conflicting
        {
            return Ok(false);
        }
        let unresolved_threads = self
            .fetch_threads(clearance.number().get())
            .await?
            .into_iter()
            .filter(|thread| thread.state() == RepoWatchThreadState::Open)
            .map(|thread| thread.thread().clone())
            .collect::<Vec<_>>();
        if !unresolved_threads.is_empty() {
            return Ok(false);
        }
        let assessment =
            RepoWatchConvergenceAssessment::try_new(RepoWatchConvergenceAssessmentInput {
                number: clearance.number(),
                head_sha: clearance.current_head_sha().clone(),
                base_branch: clearance.base_branch().clone(),
                base_revision: evidence.base_revision,
                mergeable_state,
                // Clearance candidacy does consult this, and refuses every
                // unsettled head, so it is computed from the same evidence the
                // polling path uses: finished exact-head checks, an inventory
                // quiesced against the published freshness above, and a decided
                // mergeable state.
                settled: evidence.gating_checks_settled
                    && evidence.gating_check_inventory_quiesced
                    && mergeable_state != MergeableState::Unknown,
                review_decision: evidence.review_decision,
                unresolved_threads,
                gating_check_count: evidence.gating_check_count,
                non_green_gating_checks: evidence.non_green_gating_checks,
            })
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let candidates = self.fetch_stale_review_clearances(&assessment).await?;
        Ok(candidates.iter().any(|candidate| {
            candidate.review_node_id() == clearance.review_node_id()
                && candidate.reviewed_head_sha() == clearance.reviewed_head_sha()
        }))
    }

    async fn dismiss_review_node(
        &self,
        input: DismissReviewInput<'_>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let DismissReviewInput {
            review_node_id,
            dismissal_message,
        } = input;
        let body = serde_json::to_vec(&GraphQlRequest {
            query: DISMISS_REVIEW_MUTATION,
            variables: DismissReviewVariables {
                review: review_node_id,
                message: dismissal_message,
            },
        })
        .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
        let response: GraphQlEnvelope<DismissReviewData> = self
            .conditional_json(
                "dismiss-review",
                Method::POST,
                self.graphql_url.clone(),
                Some(body),
            )
            .await?;
        if !response.errors.is_empty() {
            return Err(graphql_envelope_error(&response.errors));
        }
        let review = response
            .data
            .and_then(|data| data.dismiss_pull_request_review)
            .and_then(|payload| payload.pull_request_review)
            .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
        if review.id != review_node_id || review.state != "DISMISSED" {
            return Err(RepositoryWatchAttemptError::InvalidResponse);
        }
        Ok(())
    }

    async fn observe_stale_review_clearance(
        &self,
        clearance: &RepoWatchPlannedStaleReviewClearance,
    ) -> Result<StaleReviewClearanceObservation, RepositoryWatchAttemptError> {
        let mut after: Option<String> = None;
        let mut page = 1_u16;
        loop {
            let body = serde_json::to_vec(&GraphQlRequest {
                query: REVIEW_CLEARANCE_STATE_QUERY,
                variables: ReviewNodeVariables {
                    review: clearance.review_node_id(),
                    after: after.as_deref(),
                },
            })
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
            let response: GraphQlEnvelope<ReviewClearanceStateData> = self
                .conditional_json(
                    "review-clearance-state",
                    Method::POST,
                    self.graphql_url.clone(),
                    Some(body),
                )
                .await?;
            if !response.errors.is_empty() {
                return Err(graphql_envelope_error(&response.errors));
            }
            let review = response
                .data
                .and_then(|data| data.node)
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            if review.id != clearance.review_node_id()
                || review.pull_request.number != clearance.number().get()
            {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            let provider_state = normalize_observed_review_state(&review.state)?;
            if let Some(outcome) = terminal_clearance_outcome(provider_state) {
                return Ok(StaleReviewClearanceObservation::Terminal {
                    outcome,
                    provider_state,
                });
            }
            match review.pull_request.state.as_str() {
                "OPEN" => {}
                "CLOSED" | "MERGED" => {
                    return Ok(StaleReviewClearanceObservation::Terminal {
                        outcome: RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere,
                        provider_state,
                    });
                }
                _ => return Err(RepositoryWatchAttemptError::InvalidResponse),
            }
            if review.pull_request.head_ref_oid != clearance.current_head_sha().as_str()
                || review.pull_request.base_ref_name != clearance.base_branch().as_str()
                || review.pull_request.base_ref_oid != clearance.base_revision().as_str()
            {
                return Ok(StaleReviewClearanceObservation::Terminal {
                    outcome: RepoWatchStaleReviewClearanceOutcome::Superseded,
                    provider_state,
                });
            }
            if review
                .commit
                .as_ref()
                .is_some_and(|commit| commit.oid != clearance.reviewed_head_sha().as_str())
            {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            if normalize_review_decision(review.pull_request.review_decision.as_deref())?
                != RepoWatchReviewDecision::ChangesRequested
            {
                return Ok(StaleReviewClearanceObservation::Terminal {
                    outcome: RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere,
                    provider_state,
                });
            }
            if review
                .pull_request
                .latest_opinionated_reviews
                .nodes
                .iter()
                .any(|candidate| candidate.id == clearance.review_node_id())
            {
                return Ok(StaleReviewClearanceObservation::StillBlocking);
            }
            if !review
                .pull_request
                .latest_opinionated_reviews
                .page_info
                .has_next_page
            {
                return Ok(StaleReviewClearanceObservation::Terminal {
                    outcome: RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere,
                    provider_state,
                });
            }
            after = review
                .pull_request
                .latest_opinionated_reviews
                .page_info
                .end_cursor;
            if after.is_none() {
                return Err(RepositoryWatchAttemptError::InvalidResponse);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_reactions(
        &self,
        number: u64,
        previous: Option<&[RepoWatchReactionObservation]>,
    ) -> Result<Vec<RepoWatchReactionObservation>, RepositoryWatchAttemptError> {
        if self.signal_reviewers.is_empty() {
            return Ok(Vec::new());
        }
        let number_text = number.to_string();
        let mut observations = self
            .fetch_reaction_pages(
                &["issues", &number_text, "reactions"],
                ReactionSubject::PullRequestBody,
                previous,
            )
            .await?;
        let issue_comments = self
            .fetch_comment_ids("issue-comments", &["issues", &number_text, "comments"])
            .await?;
        for id in issue_comments {
            let id_text = id.get().to_string();
            observations.extend(
                self.fetch_reaction_pages(
                    &["issues", "comments", &id_text, "reactions"],
                    ReactionSubject::IssueComment { id },
                    previous,
                )
                .await?,
            );
        }
        let review_comments = self
            .fetch_comment_ids("review-comments", &["pulls", &number_text, "comments"])
            .await?;
        for id in review_comments {
            let id_text = id.get().to_string();
            observations.extend(
                self.fetch_reaction_pages(
                    &["pulls", "comments", &id_text, "reactions"],
                    ReactionSubject::ReviewComment { id },
                    previous,
                )
                .await?,
            );
        }
        Ok(observations)
    }

    async fn fetch_comment_ids(
        &self,
        resource_kind: &'static str,
        suffix: &[&str],
    ) -> Result<Vec<GitHubObjectId>, RepositoryWatchAttemptError> {
        let mut ids = Vec::new();
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<Vec<CommentResponse>>(
                    resource_kind,
                    Method::GET,
                    self.repository_url(
                        suffix,
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for comment in response.value {
                ids.push(object_id(comment.id)?);
            }
            if !has_next {
                return Ok(ids);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_reaction_pages(
        &self,
        suffix: &[&str],
        subject: ReactionSubject,
        previous: Option<&[RepoWatchReactionObservation]>,
    ) -> Result<Vec<RepoWatchReactionObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut identity_incomplete = false;
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<Vec<ReactionResponse>>(
                    "reactions",
                    Method::GET,
                    self.repository_url(
                        suffix,
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for reaction in response.value {
                if reaction.user.is_none() {
                    identity_incomplete = true;
                    continue;
                }
                if let Some(reactor) = reaction
                    .user
                    .as_ref()
                    .and_then(|user| self.signal_reviewer(&user.login))
                {
                    observations.push(RepoWatchReactionObservation::new(
                        subject,
                        reactor,
                        ReactionContent::try_new(reaction.content)
                            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                    ));
                }
            }
            if !has_next {
                if identity_incomplete {
                    observations.extend(
                        previous
                            .into_iter()
                            .flatten()
                            .filter(|reaction| reaction.subject() == subject)
                            .filter(|reaction| self.signal_reviewers.contains(reaction.reactor()))
                            .cloned(),
                    );
                }
                return Ok(observations);
            }
            page = next_page(page)?;
        }
    }

    fn signal_reviewer(&self, login: &str) -> Option<RepoWatchAuthorLogin> {
        self.signal_reviewers
            .iter()
            .find(|reviewer| reviewer.as_str().eq_ignore_ascii_case(login))
            .cloned()
    }

    async fn fetch_branch_heads(
        &self,
    ) -> Result<Vec<RepoWatchBranchHead>, RepositoryWatchAttemptError> {
        let mut heads = Vec::new();
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<Vec<BranchResponse>>(
                    "branches",
                    Method::GET,
                    self.repository_url(
                        &["branches"],
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for branch in response.value {
                heads.push(RepoWatchBranchHead::new(
                    BranchName::try_new(branch.name)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                    CommitSha::try_new(branch.commit.sha)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                ));
            }
            if !has_next {
                return Ok(heads);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_workflows(&self) -> Result<Vec<WorkflowResponse>, RepositoryWatchAttemptError> {
        let mut workflows = Vec::new();
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<WorkflowsResponse>(
                    "workflows",
                    Method::GET,
                    self.repository_url(
                        &["actions", "workflows"],
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for workflow in &response.value.workflows {
                positive(workflow.id)?;
                WorkflowName::try_new(workflow.name.clone())
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
            }
            workflows.extend(response.value.workflows);
            if !has_next {
                return Ok(workflows);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_workflow_runs(
        &self,
        branches: &[RepoWatchBranchHead],
        workflow: &WorkflowResponse,
        previous: &[RepoWatchWorkflowRunObservation],
    ) -> Result<Vec<RepoWatchWorkflowRunObservation>, RepositoryWatchAttemptError> {
        if branches.is_empty() {
            return Ok(Vec::new());
        }
        let workflow_id = workflow.id.to_string();
        let workflow_identity = object_id(workflow.id)?;
        let workflow_name = WorkflowName::try_new(workflow.name.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let mut pending_branches = branches
            .iter()
            .map(|branch| (branch.branch().as_str(), branch.branch()))
            .collect::<HashMap<_, _>>();
        let mut observations = Vec::with_capacity(pending_branches.len());
        let mut page = 1_u16;
        loop {
            let response = self
                .conditional_json_page::<WorkflowRunsResponse>(
                    "workflow-runs",
                    Method::GET,
                    self.repository_url(
                        &["actions", "workflows", &workflow_id, "runs"],
                        &[
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.has_next_page;
            for run in response.value.workflow_runs {
                let Some(branch) = run
                    .head_branch
                    .as_deref()
                    .and_then(|branch| pending_branches.get(branch))
                    .copied()
                    .cloned()
                else {
                    continue;
                };
                let Some(head_repository) = run.head_repository else {
                    continue;
                };
                let head_repository = RepositorySlug::try_new(head_repository.full_name)
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
                if head_repository != self.repository {
                    continue;
                }
                if run.status != "completed" {
                    continue;
                }
                let run_id = object_id(run.id)?;
                let run_attempt = RepoWatchWorkflowRunAttempt::new(
                    NonZeroU64::new(run.run_attempt)
                        .ok_or(RepositoryWatchAttemptError::Normalization)?,
                );
                let candidate = RepoWatchWorkflowRunObservation::new(
                    run_id,
                    workflow_identity,
                    run_attempt,
                    branch.clone(),
                    workflow_name.clone(),
                    normalize_conclusion(run.conclusion.as_deref())?,
                );
                let latest = previous
                    .iter()
                    .find(|prior| {
                        prior.branch() == &branch && prior.workflow_id() == workflow_identity
                    })
                    .filter(|prior| {
                        (prior.id().get(), prior.attempt().get())
                            > (candidate.id().get(), candidate.attempt().get())
                    })
                    .cloned()
                    .unwrap_or(candidate);
                observations.push(latest);
                pending_branches.remove(branch.as_str());
            }
            if pending_branches.is_empty() {
                return Ok(observations);
            }
            if !has_next {
                for branch in pending_branches.values() {
                    observations.extend(
                        previous
                            .iter()
                            .find(|prior| {
                                prior.branch() == *branch
                                    && prior.workflow_id() == workflow_identity
                            })
                            .cloned(),
                    );
                }
                return Ok(observations);
            }
            page = next_page(page)?;
        }
    }

    fn repository_url(
        &self,
        suffix: &[&str],
        query: &[(&str, String)],
    ) -> Result<Url, RepositoryWatchAttemptError> {
        let mut url = self.rest_base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| RepositoryWatchAttemptError::Request)?;
            segments.pop_if_empty();
            segments.push("repos");
            let (namespace, name) = self
                .repository
                .as_str()
                .split_once('/')
                .ok_or(RepositoryWatchAttemptError::Normalization)?;
            segments.push(namespace);
            segments.push(name);
            segments.extend(suffix.iter().copied());
        }
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        Ok(url)
    }

    // Each chunk is charged against the shared per-attempt budget as it
    // arrives, so concurrent reads account for exactly what they consumed. A
    // reservation of each read's upper bound would instead understate the
    // remaining budget by whatever the other in-flight reads never used, and
    // fail an attempt whose true total fits.
    async fn read_bounded(
        &self,
        resource_kind: &'static str,
        mut response: Response,
    ) -> Result<Vec<u8>, RepositoryWatchAttemptError> {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| RepositoryWatchAttemptError::Request)?
        {
            let next = body
                .len()
                .checked_add(chunk.len())
                .ok_or(RepositoryWatchAttemptError::ResponseTooLarge)?;
            if next > MAX_RESPONSE_BYTES {
                return Err(RepositoryWatchAttemptError::ResponseTooLarge);
            }
            self.cache()
                .record_poll_wire_bytes(resource_kind, chunk.len())?;
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn conditional_json<T>(
        &self,
        resource_kind: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T, RepositoryWatchAttemptError>
    where
        T: Any + Clone + DeserializeOwned + Send + Sync,
    {
        Ok(self
            .conditional_json_response(resource_kind, method, url, body, None)
            .await?
            .value)
    }

    async fn conditional_json_page<T>(
        &self,
        resource_kind: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<ConditionalJsonResponse<T>, RepositoryWatchAttemptError>
    where
        T: Any + Clone + DeserializeOwned + PageResponse + Send + Sync,
    {
        self.conditional_json_response(resource_kind, method, url, body, Some(T::item_count))
            .await
    }

    async fn conditional_json_response<T>(
        &self,
        resource_kind: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        page_item_count: Option<fn(&T) -> usize>,
    ) -> Result<ConditionalJsonResponse<T>, RepositoryWatchAttemptError>
    where
        T: Any + Clone + DeserializeOwned + Send + Sync,
    {
        let key = ResourceKey::new(resource_kind, &method, &url, body.as_deref());
        let page_is_at_cap = page_item_count.is_some() && result_page(&url) == MAX_RESULT_PAGES;
        self.cache().touch(key.clone())?;
        let credential = self
            .credentials
            .resolve(&self.credential_reference)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Credential)?;
        if credential.expose_bytes().is_empty()
            || credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES
        {
            return Err(RepositoryWatchAttemptError::Credential);
        }
        let mut authorization = Vec::with_capacity(7 + credential.expose_bytes().len());
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(credential.expose_bytes());
        let mut authorization = HeaderValue::from_bytes(&authorization)
            .map_err(|_| RepositoryWatchAttemptError::Credential)?;
        authorization.set_sensitive(true);
        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, USER_AGENT_VALUE);
        let cached_entity_tag = self.cache().entity_tag(&key).cloned();
        if let Some(entity_tag) = &cached_entity_tag {
            request = request.header(IF_NONE_MATCH, entity_tag.as_str());
        }
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| RepositoryWatchAttemptError::Request)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            let refreshed_entity_tag = response.headers().get(ETAG).map(entity_tag).transpose()?;
            let mut accepted = self
                .cache()
                .accepted_for_validator::<ConditionalJsonResponse<T>>(
                    &key,
                    cached_entity_tag.as_ref(),
                    refreshed_entity_tag,
                )?;
            if page_item_count.is_some() && accepted.page_is_full && !accepted.has_next_page {
                accepted.has_next_page = true;
            }
            return Ok(accepted);
        }
        if response.status() != StatusCode::OK {
            let status = response.status();
            if !rejection_needs_its_message(status, response.headers()) {
                return Err(rejected_response_error(status, response.headers(), &[]));
            }
            // The provider separates a secondary rate limit from a
            // permission-scoped rejection by the message alone when neither
            // rate-limit header is present, so this one rejection is read —
            // under the same ceiling every other body uses — before it is
            // classified. A read that fails is reported as the transport failure
            // it is: that already stops the page, while the permission rejection
            // an empty body would be read as does not, and continuing to hydrate
            // later deliveries over a transport that just failed is the
            // amplification this classification exists to prevent.
            let headers = response.headers().clone();
            let body = self.read_bounded(resource_kind, response).await?;
            return Err(rejected_response_error(status, &headers, &body));
        }
        // The cached pair stays in place while this body is read and parsed.
        // Two open pull requests sharing a head SHA fetch the same check-suite
        // and check-run keys concurrently, so dropping the pair before the
        // await would let a concurrent `304` resolve its accepted state against
        // a hole and fail an otherwise valid attempt. A changed response
        // replaces the pair atomically below, and only an unparseable one
        // invalidates it.
        let response_entity_tag = response.headers().get(ETAG).map(entity_tag).transpose()?;
        let has_next_page = has_next_link(&response)?;
        let bytes = self.read_bounded(resource_kind, response).await?;
        let Ok(value) = serde_json::from_slice::<T>(&bytes) else {
            self.cache().remove(&key);
            return Err(RepositoryWatchAttemptError::InvalidResponse);
        };
        let accepted = ConditionalJsonResponse {
            page_is_full: page_item_count.is_some_and(|item_count| item_count(&value) == PAGE_SIZE),
            value,
            has_next_page,
        };
        match response_entity_tag {
            Some(entity_tag) if !page_is_at_cap => {
                self.cache()
                    .insert(key, entity_tag, bytes.len(), accepted.clone());
            }
            // A response that is deliberately not cached leaves any prior pair
            // in place. Removing it would reopen the same window as removing
            // before the read: a concurrent request for this key that already
            // took a 304 may still be about to resolve its accepted state. The
            // prior pair stays correct on its own terms, since a later
            // validator match means the provider still considers that body
            // current.
            Some(_) | None => {}
        }
        Ok(accepted)
    }
}

#[derive(Clone)]
struct ConditionalJsonResponse<T> {
    value: T,
    has_next_page: bool,
    page_is_full: bool,
}

trait PageResponse {
    fn item_count(&self) -> usize;
}

impl<T> PageResponse for Vec<T> {
    fn item_count(&self) -> usize {
        self.len()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResourceKey(String);

impl ResourceKey {
    fn new(kind: &str, method: &Method, url: &Url, body: Option<&[u8]>) -> Self {
        let mut digest = Sha256::new();
        digest.update(method.as_str().as_bytes());
        digest.update([0]);
        digest.update(url.as_str().as_bytes());
        digest.update([0]);
        if let Some(body) = body {
            digest.update(body);
        }
        let digest: [u8; 32] = digest.finalize().into();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Self(format!("{kind}/{hex}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntityTag(String);

impl EntityTag {
    fn as_str(&self) -> &str {
        &self.0
    }
}

fn entity_tag(value: &HeaderValue) -> Result<EntityTag, RepositoryWatchAttemptError> {
    let value = value
        .to_str()
        .map_err(|_| RepositoryWatchAttemptError::InvalidEntityTag)?;
    if value.is_empty()
        || value.len() > MAX_ENTITY_TAG_BYTES
        || !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Err(RepositoryWatchAttemptError::InvalidEntityTag);
    }
    Ok(EntityTag(value.to_owned()))
}

fn has_next_link(response: &Response) -> Result<bool, RepositoryWatchAttemptError> {
    for value in response.headers().get_all(LINK) {
        let value = value
            .to_str()
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
        if value.split(',').any(|link| {
            link.split(';')
                .skip(1)
                .any(|parameter| parameter.trim() == "rel=\"next\"")
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn result_page(url: &Url) -> u16 {
    url.query_pairs()
        .find(|(name, _)| name == "page")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0)
}

const fn commit_check_run_search_is_complete(suite_count: usize) -> bool {
    suite_count > 0 && suite_count <= MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH
}

struct CachedResource {
    entity_tag: EntityTag,
    wire_bytes: usize,
    accepted: Box<dyn Any + Send + Sync>,
    idle_polls: usize,
}

#[derive(Default)]
struct PollCache {
    resources: HashMap<ResourceKey, CachedResource>,
    touched: HashSet<ResourceKey>,
    requests: usize,
    poll_wire_bytes: usize,
    cached_wire_bytes: usize,
}

#[derive(Clone, Copy)]
struct PollAttemptMetrics {
    requests: usize,
    poll_wire_bytes: usize,
    cached_resources: usize,
    cached_wire_bytes: usize,
}

impl PollCache {
    fn begin_attempt(&mut self) {
        self.requests = 0;
        self.poll_wire_bytes = 0;
    }

    fn begin_poll(&mut self) {
        self.touched.clear();
        self.begin_attempt();
    }

    fn touch(&mut self, key: ResourceKey) -> Result<(), RepositoryWatchAttemptError> {
        self.requests = self
            .requests
            .checked_add(1)
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
        if self.requests > MAX_REQUESTS_PER_POLL {
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        self.touched.insert(key);
        Ok(())
    }

    fn complete_poll(&mut self) {
        let touched = &self.touched;
        self.resources.retain(|key, resource| {
            if touched.contains(key) {
                resource.idle_polls = 0;
                return true;
            }
            resource.idle_polls = resource.idle_polls.saturating_add(1);
            resource.idle_polls <= MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS
        });
        self.cached_wire_bytes = self
            .resources
            .values()
            .map(|resource| resource.wire_bytes)
            .sum();
    }

    fn record_poll_wire_bytes(
        &mut self,
        resource_kind: &'static str,
        wire_bytes: usize,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let projected = self
            .poll_wire_bytes
            .checked_add(wire_bytes)
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
        if projected > MAX_POLL_WIRE_BYTES {
            tracing::warn!(
                resource_kind,
                accepted_poll_wire_bytes = self.poll_wire_bytes,
                next_chunk_bytes = wire_bytes,
                projected_poll_wire_bytes = projected,
                "repository-watch poll wire budget exceeded"
            );
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        self.poll_wire_bytes = projected;
        Ok(())
    }

    fn entity_tag(&self, key: &ResourceKey) -> Option<&EntityTag> {
        self.resources.get(key).map(|resource| &resource.entity_tag)
    }

    // Reading the accepted body and rebinding the entity tag happen under one
    // lock, and the rebind only lands when the pair is still the one this
    // request validated. Two open pull requests sharing a head SHA issue
    // identical conditional requests, so a concurrent 200 can install a
    // different pair in between; binding this request's validator onto that
    // pair would leave a tag describing a body it never validated, and a later
    // 304 against that tag would then reuse the wrong body.
    fn accepted_for_validator<T: Any + Clone>(
        &mut self,
        key: &ResourceKey,
        validated: Option<&EntityTag>,
        refreshed: Option<EntityTag>,
    ) -> Result<T, RepositoryWatchAttemptError> {
        let resource = self
            .resources
            .get_mut(key)
            .ok_or(RepositoryWatchAttemptError::MissingCachedResource)?;
        let accepted = resource
            .accepted
            .downcast_ref::<T>()
            .cloned()
            .ok_or(RepositoryWatchAttemptError::MissingCachedResource)?;
        if let (Some(refreshed), Some(validated)) = (refreshed, validated)
            && &resource.entity_tag == validated
        {
            resource.entity_tag = refreshed;
        }
        Ok(accepted)
    }

    // Admission is an accelerator, never a precondition for an observation: a
    // resource that does not fit is shed and simply refetched unconditionally
    // on the next attempt. Failing the attempt instead would let the retention
    // bound cap the transfer bound, because every resource already fetched in
    // the current attempt is touched and therefore not evictable.
    fn insert<T: Any + Send + Sync>(
        &mut self,
        key: ResourceKey,
        entity_tag: EntityTag,
        wire_bytes: usize,
        accepted: T,
    ) {
        self.insert_with_resource_limit(
            key,
            entity_tag,
            wire_bytes,
            accepted,
            MAX_CACHED_RESOURCES,
        );
    }

    fn insert_with_resource_limit<T: Any + Send + Sync>(
        &mut self,
        key: ResourceKey,
        entity_tag: EntityTag,
        wire_bytes: usize,
        accepted: T,
        resource_limit: usize,
    ) {
        while !self.resources.contains_key(&key) && self.resources.len() >= resource_limit {
            if !self.evict_one_untouched() {
                break;
            }
        }
        if !self.resources.contains_key(&key) && self.resources.len() >= resource_limit {
            return;
        }
        let Ok(mut projected_bytes) = self.projected_cached_bytes(&key, wire_bytes) else {
            return;
        };
        while projected_bytes > MAX_CACHED_WIRE_BYTES {
            if !self.evict_one_untouched() {
                break;
            }
            let Ok(next_projection) = self.projected_cached_bytes(&key, wire_bytes) else {
                return;
            };
            projected_bytes = next_projection;
        }
        if projected_bytes > MAX_CACHED_WIRE_BYTES {
            return;
        }
        self.resources.insert(
            key,
            CachedResource {
                entity_tag,
                wire_bytes,
                accepted: Box::new(accepted),
                idle_polls: 0,
            },
        );
        self.cached_wire_bytes = projected_bytes;
    }

    fn projected_cached_bytes(
        &self,
        key: &ResourceKey,
        wire_bytes: usize,
    ) -> Result<usize, RepositoryWatchAttemptError> {
        let replaced_bytes = self
            .resources
            .get(key)
            .map_or(0, |resource| resource.wire_bytes);
        self.cached_wire_bytes
            .checked_sub(replaced_bytes)
            .and_then(|retained| retained.checked_add(wire_bytes))
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)
    }

    fn evict_one_untouched(&mut self) -> bool {
        let stale = self
            .resources
            .keys()
            .find(|cached| !self.touched.contains(*cached))
            .cloned();
        if let Some(stale) = stale {
            self.remove(&stale);
            true
        } else {
            false
        }
    }

    fn remove(&mut self, key: &ResourceKey) {
        if let Some(resource) = self.resources.remove(key) {
            self.cached_wire_bytes = self.cached_wire_bytes.saturating_sub(resource.wire_bytes);
        }
    }
}

/// Builds the durable cursor view after event derivation has observed the full
/// provider state.
///
/// A merged pull request has already contributed its terminal lifecycle event
/// and recurring-stream frontier at this boundary. Retaining its title, body,
/// reviews, checks, threads, and reactions in every later cursor only makes
/// webhook refreshes repeatedly transfer and decode terminal history. Closed
/// but unmerged pull requests remain for one later complete poll, preserving
/// the existing current-state view for that distinct lifecycle.
struct CompactedCursorObservation {
    observation: RepoWatchObservation,
    merged_pull_request_baselines: Vec<RepoWatchMergedPullRequestBaselineV1>,
}

fn compact_cursor_observation(
    observation: &RepoWatchObservation,
    previous: Option<&RepoWatchObservation>,
    retained_merged_baselines: &[RepoWatchMergedPullRequestBaselineV1],
) -> Result<CompactedCursorObservation, RepositoryWatchAttemptError> {
    let state = observation.state();
    // A storage-version-three cursor carries its merged pull requests in full
    // and no baselines at all, and a complete poll fetches only listed open
    // pull requests and previously open ones, so those merged entries are
    // absent from `observation`. Deriving baselines from the current
    // observation alone would drop exactly the state the migration preserved
    // for this commit to compact. Seed from the prior observation first so the
    // retained baselines and the current observation below still win wherever
    // they carry a fresher form of the same pull request.
    let mut merged_pull_request_baselines = BTreeMap::new();
    if let Some(previous) = previous {
        for pull_request in previous.state().pull_requests() {
            if let Some(baseline) = RepoWatchMergedPullRequestBaselineV1::from_merged_state(
                pull_request,
                previous.signal_reviewers(),
            )
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?
            {
                merged_pull_request_baselines.insert(baseline.number(), baseline);
            }
        }
    }
    merged_pull_request_baselines.extend(
        retained_merged_baselines
            .iter()
            .cloned()
            .map(|baseline| (baseline.number(), baseline)),
    );
    for pull_request in state.pull_requests() {
        if let Some(baseline) = RepoWatchMergedPullRequestBaselineV1::from_merged_state(
            pull_request,
            observation.signal_reviewers(),
        )
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?
        {
            merged_pull_request_baselines.insert(baseline.number(), baseline);
        } else {
            merged_pull_request_baselines.remove(&pull_request.context().number());
        }
    }
    let compacted = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: state
            .pull_requests()
            .iter()
            .filter(|pull_request| {
                pull_request.lifecycle() != RepoWatchPullRequestLifecycle::Merged
            })
            .cloned()
            .collect(),
        workflow_runs: state.workflow_runs().to_vec(),
        branch_heads: state.branch_heads().to_vec(),
    })
    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
    Ok(CompactedCursorObservation {
        observation: RepoWatchObservation::new(observation.signal_reviewers().to_vec(), compacted),
        merged_pull_request_baselines: merged_pull_request_baselines.into_values().collect(),
    })
}

fn pull_request_base_revision<'a>(
    observation: &'a RepoWatchObservation,
    pull_request: &RepoWatchPullRequestState,
) -> Option<&'a CommitSha> {
    observation
        .state()
        .branch_heads()
        .iter()
        .find(|head| head.branch() == pull_request.context().base_branch())
        .map(RepoWatchBranchHead::head)
}

fn pull_request_base_revision_matches(
    observation: &RepoWatchObservation,
    pull_request: &RepoWatchPullRequestState,
    branch_heads: &[RepoWatchBranchHead],
) -> bool {
    pull_request_base_revision(observation, pull_request)
        == branch_heads
            .iter()
            .find(|head| head.branch() == pull_request.context().base_branch())
            .map(RepoWatchBranchHead::head)
}

fn reuse_pull_request(
    previous: &RepoWatchPullRequestState,
    reviews: Vec<RepoWatchReviewObservation>,
    threads: Vec<RepoWatchThreadObservation>,
    reactions: Vec<RepoWatchReactionObservation>,
) -> Result<RepoWatchPullRequestState, RepositoryWatchAttemptError> {
    RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: previous.context().clone(),
        lifecycle: previous.lifecycle(),
        mergeable_state: previous.mergeable_state(),
        completed_check_suites: previous.completed_check_suites().to_vec(),
        completed_check_runs: previous.completed_check_runs().to_vec(),
        reviews,
        threads,
        reactions,
    })
    .map_err(|_| RepositoryWatchAttemptError::Normalization)
}

fn previous_pull_request(
    previous: &RepoWatchObservation,
    number: u64,
) -> Option<&RepoWatchPullRequestState> {
    previous
        .state()
        .pull_requests()
        .iter()
        .find(|pull_request| pull_request.context().number().get() == number)
}

fn next_page(page: u16) -> Result<u16, RepositoryWatchAttemptError> {
    let next = page
        .checked_add(1)
        .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
    if next > MAX_RESULT_PAGES {
        Err(RepositoryWatchAttemptError::ResourceLimit)
    } else {
        Ok(next)
    }
}

fn positive(value: u64) -> Result<NonZeroU64, RepositoryWatchAttemptError> {
    NonZeroU64::new(value).ok_or(RepositoryWatchAttemptError::Normalization)
}

fn object_id(value: u64) -> Result<GitHubObjectId, RepositoryWatchAttemptError> {
    positive(value).map(GitHubObjectId::new)
}

fn normalize_pull_request_context(
    response: &PullResponse,
    head_sha: CommitSha,
    previous: Option<&PullRequestEventContext>,
) -> Result<PullRequestEventContext, RepositoryWatchAttemptError> {
    let labels = response
        .labels
        .iter()
        .map(|label| {
            LabelName::try_new(label.name.clone())
                .map_err(|_| RepositoryWatchAttemptError::Normalization)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let author = response
        .user
        .as_ref()
        .map(|user| RepoWatchAuthorLogin::try_new(user.login.clone()))
        .transpose()
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
    let head_repository = response
        .head
        .repo
        .as_ref()
        .map(|repository| RepositorySlug::try_new(repository.full_name.clone()))
        .transpose()
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?
        .or_else(|| previous.map(|context| context.head_repository().clone()))
        .ok_or(RepositoryWatchAttemptError::Normalization)?;
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(positive(response.number)?),
        head_sha,
        head_repository,
        base_branch: BranchName::try_new(response.base.reference.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
        head_branch: BranchName::try_new(response.head.reference.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
        title: PullRequestTitle::try_new(response.title.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
        body: PullRequestBody::try_new(response.body.clone().unwrap_or_default())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
        labels,
        draft: response.draft,
        author,
    }))
}

fn normalize_lifecycle(
    response: &PullResponse,
) -> Result<RepoWatchPullRequestLifecycle, RepositoryWatchAttemptError> {
    match (response.state.as_str(), response.merged_at.is_some()) {
        ("open", false) => Ok(RepoWatchPullRequestLifecycle::Open),
        ("closed", true) => Ok(RepoWatchPullRequestLifecycle::Merged),
        ("closed", false) => Ok(RepoWatchPullRequestLifecycle::Closed),
        _ => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

fn normalize_checks_outcome(
    conclusion: Option<&str>,
) -> Result<ChecksOutcome, RepositoryWatchAttemptError> {
    match normalize_conclusion(conclusion)? {
        CheckConclusion::Success | CheckConclusion::Neutral | CheckConclusion::Skipped => {
            Ok(ChecksOutcome::Success)
        }
        CheckConclusion::Failure
        | CheckConclusion::Cancelled
        | CheckConclusion::TimedOut
        | CheckConclusion::ActionRequired
        | CheckConclusion::Stale
        | CheckConclusion::StartupFailure => Ok(ChecksOutcome::Failure),
    }
}

fn normalize_conclusion(
    conclusion: Option<&str>,
) -> Result<CheckConclusion, RepositoryWatchAttemptError> {
    match conclusion {
        Some("success") => Ok(CheckConclusion::Success),
        Some("failure") => Ok(CheckConclusion::Failure),
        Some("neutral") => Ok(CheckConclusion::Neutral),
        Some("cancelled") => Ok(CheckConclusion::Cancelled),
        Some("skipped") => Ok(CheckConclusion::Skipped),
        Some("timed_out") => Ok(CheckConclusion::TimedOut),
        Some("action_required") => Ok(CheckConclusion::ActionRequired),
        Some("stale") => Ok(CheckConclusion::Stale),
        Some("startup_failure") => Ok(CheckConclusion::StartupFailure),
        Some(_) | None => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

enum ProviderReviewState {
    Submitted(ReviewState),
    Dismissed,
    Pending,
}

fn normalize_review_state(state: &str) -> Result<ProviderReviewState, RepositoryWatchAttemptError> {
    match state {
        "APPROVED" => Ok(ProviderReviewState::Submitted(ReviewState::Approved)),
        "CHANGES_REQUESTED" => Ok(ProviderReviewState::Submitted(
            ReviewState::ChangesRequested,
        )),
        "COMMENTED" => Ok(ProviderReviewState::Submitted(ReviewState::Commented)),
        "DISMISSED" => Ok(ProviderReviewState::Dismissed),
        "PENDING" => Ok(ProviderReviewState::Pending),
        _ => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

fn normalize_observed_review_state(
    state: &str,
) -> Result<RepoWatchObservedReviewState, RepositoryWatchAttemptError> {
    match state {
        "APPROVED" => Ok(RepoWatchObservedReviewState::Approved),
        "CHANGES_REQUESTED" => Ok(RepoWatchObservedReviewState::ChangesRequested),
        "COMMENTED" => Ok(RepoWatchObservedReviewState::Commented),
        "DISMISSED" => Ok(RepoWatchObservedReviewState::Dismissed),
        "PENDING" => Ok(RepoWatchObservedReviewState::Pending),
        _ => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

#[derive(Clone, Deserialize)]
struct PullNumberResponse {
    number: u64,
    updated_at: String,
    head: ListedPullHeadResponse,
}

#[derive(Clone, Deserialize)]
struct ListedPullHeadResponse {
    sha: String,
}

#[derive(Clone, Deserialize)]
struct PullResponse {
    number: u64,
    state: String,
    // The same stamp the pulls listing carries, so a detail read can be
    // compared against the freshness a committed poll recorded from the
    // listing.
    updated_at: String,
    merged_at: Option<String>,
    mergeable: Option<bool>,
    head: PullReferenceResponse,
    base: PullReferenceResponse,
    title: String,
    body: Option<String>,
    labels: Vec<LabelResponse>,
    draft: bool,
    user: Option<UserResponse>,
}

#[derive(Clone, Deserialize)]
struct PullReferenceResponse {
    sha: String,
    #[serde(rename = "ref")]
    reference: String,
    repo: Option<RepositoryResponse>,
}

#[derive(Clone, Deserialize)]
struct RepositoryResponse {
    full_name: String,
}

#[derive(Clone, Deserialize)]
struct LabelResponse {
    name: String,
}

#[derive(Clone, Deserialize)]
struct UserResponse {
    login: String,
}

#[derive(Clone, Deserialize)]
struct CheckSuitesResponse {
    check_suites: Vec<CheckSuiteResponse>,
}

impl PageResponse for CheckSuitesResponse {
    fn item_count(&self) -> usize {
        self.check_suites.len()
    }
}

#[derive(Clone, Deserialize)]
struct CheckSuiteResponse {
    id: u64,
    status: String,
    conclusion: Option<String>,
    updated_at: String,
}

#[derive(Clone, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRunResponse>,
}

impl PageResponse for CheckRunsResponse {
    fn item_count(&self) -> usize {
        self.check_runs.len()
    }
}

#[derive(Clone, Deserialize)]
struct CheckRunResponse {
    id: u64,
    status: String,
    name: String,
    conclusion: Option<String>,
    /// The provider defines `updated_at` on a check suite and never on a check
    /// run: a run carries `started_at` and `completed_at` only. A completed
    /// run's completion generation is therefore its `completed_at`, which the
    /// provider populates exactly when the run reaches `completed`; a required
    /// member the provider does not define makes every real page undecodable.
    completed_at: Option<String>,
}

#[derive(Clone, Deserialize)]
struct ReviewResponse {
    id: u64,
    user: Option<UserResponse>,
    state: String,
    commit_id: String,
}

#[derive(Clone, Deserialize)]
struct CommentResponse {
    id: u64,
}

#[derive(Clone, Deserialize)]
struct ReactionResponse {
    user: Option<UserResponse>,
    content: String,
}

#[derive(Clone, Deserialize)]
struct BranchResponse {
    name: String,
    commit: BranchCommitResponse,
}

#[derive(Clone, Deserialize)]
struct BranchCommitResponse {
    sha: String,
}

#[derive(Clone, Deserialize)]
struct WorkflowsResponse {
    workflows: Vec<WorkflowResponse>,
}

impl PageResponse for WorkflowsResponse {
    fn item_count(&self) -> usize {
        self.workflows.len()
    }
}

#[derive(Clone, Deserialize)]
struct WorkflowResponse {
    id: u64,
    name: String,
}

#[derive(Clone, Deserialize)]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRunResponse>,
}

impl PageResponse for WorkflowRunsResponse {
    fn item_count(&self) -> usize {
        self.workflow_runs.len()
    }
}

#[derive(Clone, Deserialize)]
struct WorkflowRunResponse {
    id: u64,
    run_attempt: u64,
    head_branch: Option<String>,
    status: String,
    conclusion: Option<String>,
    head_repository: Option<RepositoryResponse>,
}

#[derive(Serialize)]
struct GraphQlRequest<T> {
    query: &'static str,
    variables: T,
}

#[derive(Serialize)]
struct ThreadVariables<'a> {
    namespace: &'a str,
    name: &'a str,
    number: i64,
    after: Option<&'a str>,
}

#[derive(Serialize)]
struct DismissReviewVariables<'a> {
    review: &'a str,
    message: &'a str,
}

struct DismissReviewInput<'a> {
    review_node_id: &'a str,
    dismissal_message: &'a str,
}

#[derive(Serialize)]
struct ReviewNodeVariables<'a> {
    review: &'a str,
    after: Option<&'a str>,
}

#[derive(Clone, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Clone, Deserialize)]
struct GraphQlError {
    // The provider's closed error taxonomy. Absent on a query the server
    // rejected without one, which is target-specific by construction.
    #[serde(rename = "type", default)]
    error_type: Option<String>,
    // The same taxonomy's second carrier. The provider spells a classification
    // in `type` or in `extensions.code` depending on which layer rejected the
    // query, and `RATE_LIMITED` in particular is carried here on the envelopes
    // the API's own documentation shows. `crates/tools-github` reads both for
    // the same reason.
    #[serde(default)]
    extensions: Option<GraphQlErrorExtensions>,
}

#[derive(Clone, Deserialize)]
struct GraphQlErrorExtensions {
    #[serde(default)]
    code: Option<String>,
}

impl GraphQlError {
    /// The classifications that describe the repository's provider rather than
    /// the query, so no later request on the page can succeed either.
    const REPOSITORY_WIDE_CODES: [&'static str; 2] = ["RATE_LIMITED", "SERVICE_UNAVAILABLE"];

    fn is_repository_wide(&self) -> bool {
        self.classifications().any(|classification| {
            Self::REPOSITORY_WIDE_CODES
                .iter()
                .any(|wide| wide.eq_ignore_ascii_case(classification))
        })
    }

    fn classifications(&self) -> impl Iterator<Item = &str> {
        self.error_type.as_deref().into_iter().chain(
            self.extensions
                .as_ref()
                .and_then(|extensions| extensions.code.as_deref()),
        )
    }
}

#[derive(Clone, Deserialize)]
struct ThreadData {
    repository: Option<ThreadRepository>,
}

#[derive(Clone, Deserialize)]
struct ThreadRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ThreadPullRequest>,
}

#[derive(Clone, Deserialize)]
struct ThreadPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: ThreadConnection,
}

#[derive(Clone, Deserialize)]
struct ThreadConnection {
    nodes: Vec<ThreadResponse>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Clone, Deserialize)]
struct ThreadResponse {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

#[derive(Clone, Deserialize)]
struct ConvergenceData {
    repository: Option<ConvergenceRepository>,
}

#[derive(Clone, Deserialize)]
struct ConvergenceRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ConvergencePullRequest>,
}

#[derive(Clone, Deserialize)]
struct ConvergencePullRequest {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    mergeable: String,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    commits: ConvergenceCommitConnection,
}

#[derive(Clone, Deserialize)]
struct ConvergenceCommitConnection {
    nodes: Vec<ConvergenceCommitNode>,
}

#[derive(Clone, Deserialize)]
struct ConvergenceCommitNode {
    commit: ConvergenceCommit,
}

#[derive(Clone, Deserialize)]
struct ConvergenceCommit {
    oid: String,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<ConvergenceCheckRollup>,
}

#[derive(Clone, Deserialize)]
struct ConvergenceCheckRollup {
    contexts: ConvergenceCheckConnection,
}

#[derive(Clone, Deserialize)]
struct ConvergenceCheckConnection {
    nodes: Vec<ConvergenceCheck>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewData {
    repository: Option<BlockingReviewRepository>,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<BlockingReviewPullRequest>,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewPullRequest {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(rename = "latestOpinionatedReviews")]
    latest_opinionated_reviews: BlockingReviewConnection,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewConnection {
    nodes: Vec<BlockingReviewNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewNode {
    id: String,
    state: String,
    author: Option<BlockingReviewAuthor>,
    commit: Option<BlockingReviewCommit>,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewAuthor {
    login: String,
}

#[derive(Clone, Deserialize)]
struct BlockingReviewCommit {
    oid: String,
}

#[derive(Clone, Deserialize)]
struct DismissReviewData {
    #[serde(rename = "dismissPullRequestReview")]
    dismiss_pull_request_review: Option<DismissReviewPayload>,
}

#[derive(Clone, Deserialize)]
struct DismissReviewPayload {
    #[serde(rename = "pullRequestReview")]
    pull_request_review: Option<DismissedReview>,
}

#[derive(Clone, Deserialize)]
struct DismissedReview {
    id: String,
    state: String,
}

#[derive(Clone, Deserialize)]
struct ReviewClearanceStateData {
    node: Option<ReviewClearanceState>,
}

#[derive(Clone, Deserialize)]
struct ReviewClearanceState {
    id: String,
    state: String,
    commit: Option<BlockingReviewCommit>,
    #[serde(rename = "pullRequest")]
    pull_request: ReviewClearancePullRequest,
}

#[derive(Clone, Deserialize)]
struct ReviewClearancePullRequest {
    number: u64,
    state: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(rename = "latestOpinionatedReviews")]
    latest_opinionated_reviews: ReviewClearanceReviewConnection,
}

#[derive(Clone, Deserialize)]
struct ReviewClearanceReviewConnection {
    nodes: Vec<ReviewClearanceReviewNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Clone, Deserialize)]
struct ReviewClearanceReviewNode {
    id: String,
}

enum StaleReviewClearanceObservation {
    StillBlocking,
    Terminal {
        outcome: RepoWatchStaleReviewClearanceOutcome,
        provider_state: RepoWatchObservedReviewState,
    },
}

const fn terminal_clearance_outcome(
    provider_state: RepoWatchObservedReviewState,
) -> Option<RepoWatchStaleReviewClearanceOutcome> {
    match provider_state {
        RepoWatchObservedReviewState::Dismissed => {
            Some(RepoWatchStaleReviewClearanceOutcome::AlreadyDismissed)
        }
        RepoWatchObservedReviewState::ChangesRequested => None,
        RepoWatchObservedReviewState::Approved
        | RepoWatchObservedReviewState::Commented
        | RepoWatchObservedReviewState::Pending => {
            Some(RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere)
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "__typename")]
enum ConvergenceCheck {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
    },
}

impl ConvergenceCheck {
    fn name(&self) -> &str {
        match self {
            Self::CheckRun { name, .. } => name,
            Self::StatusContext { context, .. } => context,
        }
    }

    fn is_report_only(&self) -> bool {
        is_non_gating_check_name(self.name())
    }

    fn complete(&self) -> bool {
        match self {
            Self::CheckRun { status, .. } => status == "COMPLETED",
            Self::StatusContext { state, .. } => state != "PENDING",
        }
    }

    fn green(&self) -> bool {
        match self {
            Self::CheckRun {
                status, conclusion, ..
            } => {
                status == "COMPLETED"
                    && matches!(
                        conclusion.as_deref(),
                        Some("SUCCESS" | "SKIPPED" | "NEUTRAL")
                    )
            }
            Self::StatusContext { state, .. } => state == "SUCCESS",
        }
    }
}

fn is_non_gating_check_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    NON_GATING_CHECK_NAME_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
}

fn normalize_graphql_mergeable(value: &str) -> Result<MergeableState, RepositoryWatchAttemptError> {
    match value {
        "MERGEABLE" => Ok(MergeableState::Mergeable),
        "CONFLICTING" => Ok(MergeableState::Conflicting),
        "UNKNOWN" => Ok(MergeableState::Unknown),
        _ => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

fn normalize_review_decision(
    value: Option<&str>,
) -> Result<RepoWatchReviewDecision, RepositoryWatchAttemptError> {
    match value {
        None => Ok(RepoWatchReviewDecision::None),
        Some("APPROVED") => Ok(RepoWatchReviewDecision::Approved),
        Some("REVIEW_REQUIRED") => Ok(RepoWatchReviewDecision::ReviewRequired),
        Some("CHANGES_REQUESTED") => Ok(RepoWatchReviewDecision::ChangesRequested),
        Some(_) => Err(RepositoryWatchAttemptError::InvalidResponse),
    }
}

#[derive(Clone, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        error::Error,
        fs,
        io::{self, Write},
        num::{NonZeroU16, NonZeroU64},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use signalbox_application::derive_repo_watch_events;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Notify, oneshot, watch},
        task::{JoinHandle, JoinSet},
        time::{Instant, sleep},
    };

    use super::{
        CheckConclusion, ChecksOutcome, ConvergenceCheck, DrainRetryBackoff, EntityTag,
        FileCredentialAccess, GitHubRepositoryPoller, GraphQlEnvelope, GraphQlError,
        GraphQlErrorExtensions, HeaderMap, HeaderValue, ListedPullRequest, MAX_CACHED_WIRE_BYTES,
        MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH, MAX_CONCURRENT_PULL_REQUEST_FETCHES,
        MAX_CONSECUTIVE_POLL_PREEMPTIONS, MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS,
        MAX_POLL_WIRE_BYTES, MergeableState, PAGE_SIZE, PollAttemptWait, PollCache,
        PreparedTargetedRefresh, PullRequestSettlement, PullResponse, RETRY_AFTER, ReactionContent,
        RepoWatchAuthorLogin, RepoWatchBranchHead, RepoWatchConvergenceAssessment,
        RepoWatchConvergenceAssessmentInput, RepoWatchCursorGeneration, RepoWatchEventKindNameV1,
        RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
        RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
        RepoWatchRepositoryStateInput, RepoWatchReviewDecision, RepoWatchReviewObservation,
        RepoWatchStaleReviewClearanceCandidate, RepoWatchThreadState, RepoWatchWorkflowRunAttempt,
        RepoWatchWorkflowRunObservation, RepositorySlug, RepositoryWatchAttemptError,
        RepositoryWatchChildExit, RepositoryWatchRuntimeConstructionError,
        RepositoryWatchRuntimeError, RepositoryWatchTask, RepositoryWatchWake, ResourceKey,
        ReviewState, StatusCode, TargetedPollOutcome, TargetedPullRequest,
        TargetedRefreshSettlement, Url, UuidV7RepoWatchEventIdGenerator,
        WEBHOOK_CURSOR_SIZING_TIMEOUT, WEBHOOK_DRAIN_ATTEMPT_TIMEOUT,
        WEBHOOK_DRAIN_MAX_ATTEMPT_TIMEOUT, WEBHOOK_DRAIN_RETRY_DELAY,
        WEBHOOK_DRAIN_RETRY_MAX_DELAY, WEBHOOK_DRAIN_TIMEOUT_PAYLOAD_QUANTUM_BYTES,
        WEBHOOK_DRAIN_TIMEOUT_PER_PAYLOAD_QUANTUM, WEBHOOK_PENDING_PAGE_SIZE,
        WebhookAttemptDeadlines, WebhookAttemptOutcome, WebhookAttemptPhase, WebhookDrain,
        WebhookDrainOutcome, WebhookDrainProgress, WebhookDrainRetry, WebhookPayloadPurgeSchedule,
        WebhookPollInterrupt, WebhookShadowBaseline, WorkflowName, WorkflowResponse,
        await_poll_or_interrupt, commit_check_run_search_is_complete, compact_cursor_observation,
        dispatch_context_json, graphql_envelope_error, initial_poll_deadline,
        inspect_webhook_drain, merge_targeted_refresh_into_webhook_shadow, next_cadence_deadline,
        next_repository_wake, normalize_checks_outcome, normalize_pull_request_context, object_id,
        observe_webhook_work_before_drain, owed_dispatch_context_json_parts,
        poll_webhook_interrupt, record_dispatch_start_nudge_outcome, rejected_response_error,
        rejection_needs_its_message, repository_reconciliation_should_yield, rule_activation_error,
        run_until_shutdown, supervise_repository_tasks, targeted_pull_requests,
    };
    use signalbox_application::{
        EligibilityNudgeOutcome, InProcessEligibilityWorkSource,
        RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventIdentityFrontierV1,
        RepoWatchMergedPullRequestBaselineV1, RepoWatchTargetedRefreshV1,
    };
    use signalbox_domain::{
        BranchName, CommitSha, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionSubject,
        RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindV1, RepoWatchRuleId,
        RepoWatchRuleIdentityField, RepoWatchRuleVersion,
    };
    use signalbox_model_runtime::CredentialReference;
    use signalbox_persistence::{
        disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
        disposable_test_container_labels, local_test_connection_options, migrate,
        repo_watch::{
            PostgresRepoWatchStore, RepoWatchCommitRequest, RepoWatchCursorCandidate,
            RepoWatchEventPageSize, RepoWatchEventProducer,
            RepoWatchPlannedStaleReviewClearanceFixture, RepoWatchStaleReviewClearanceClaimToken,
            RepoWatchStaleReviewClearanceId,
        },
        repo_watch_dispatch::{PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError},
        repo_watch_webhook::{
            PostgresRepoWatchWebhookStore, RepoWatchWebhookAdmission, RepoWatchWebhookDeliveryKey,
            RepoWatchWebhookDisposition,
        },
        scheduler::PostgresEligibilitySweep,
    };
    use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
    };
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::fmt::MakeWriter;

    use crate::{SessionTemplateConfiguration, configuration::checked_in_example_configuration};

    const WATCHED_REPOSITORY: &str = "namespace/project";
    const CREDENTIAL_REFERENCE: &str = "repository-watch:namespace/project";
    const CREDENTIAL_FILE_NAME: &str = "watch-token";
    const CREDENTIAL_VALUE: &str = "fixture-token";
    const ENTITY_TAG: &str = "\"fixture-etag\"";
    const NEXT_PAGE_LINK: &str = "<https://api.github.com/next>; rel=\"next\"";
    const PULLS_TARGET: &str = "/repos/namespace/project/pulls?state=open&per_page=100&page=1";
    const BRANCHES_TARGET: &str = "/repos/namespace/project/branches?per_page=100&page=1";
    const WORKFLOWS_TARGET: &str = "/repos/namespace/project/actions/workflows?per_page=100&page=1";
    const SECOND_WORKFLOWS_PAGE_TARGET: &str =
        "/repos/namespace/project/actions/workflows?per_page=100&page=2";
    const PULL_DETAIL_TARGET: &str = "/repos/namespace/project/pulls/7";
    const CHECK_SUITES_TARGET: &str = "/repos/namespace/project/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-suites?filter=all&per_page=100&page=1";
    const COMMIT_CHECK_RUNS_TARGET: &str = "/repos/namespace/project/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs?filter=all&per_page=100&page=1";
    const REVIEWS_TARGET: &str = "/repos/namespace/project/pulls/7/reviews?per_page=100&page=1";
    const THREADS_TARGET: &str = "/graphql";
    const PULL_REACTIONS_TARGET: &str =
        "/repos/namespace/project/issues/7/reactions?per_page=100&page=1";
    const ISSUE_COMMENTS_TARGET: &str =
        "/repos/namespace/project/issues/7/comments?per_page=100&page=1";
    const ISSUE_COMMENT_REACTIONS_TARGET: &str =
        "/repos/namespace/project/issues/comments/41/reactions?per_page=100&page=1";
    const REVIEW_COMMENTS_TARGET: &str =
        "/repos/namespace/project/pulls/7/comments?per_page=100&page=1";
    const REVIEW_COMMENT_REACTIONS_TARGET: &str =
        "/repos/namespace/project/pulls/comments/51/reactions?per_page=100&page=1";
    const MAIN_WORKFLOW_TARGET: &str =
        "/repos/namespace/project/actions/workflows/61/runs?per_page=100&page=1";
    const SECOND_MAIN_WORKFLOW_PAGE_TARGET: &str =
        "/repos/namespace/project/actions/workflows/61/runs?per_page=100&page=2";
    const EMPTY_LIST: &str = "[]";
    const EMPTY_CHECK_SUITE_LIST: &str = "{\"check_suites\":[]}";
    const CONCURRENT_FETCH_PULL_NUMBERS: std::ops::RangeInclusive<u64> = 1..=9;
    const CONCURRENT_FETCH_DELAY: Duration = Duration::from_millis(20);
    // Longer than any await this module's tests perform, so a child parked in
    // a response carrying it can only stop by being aborted and joined.
    const CANCELLED_FETCH_DELAY: Duration = Duration::from_secs(60);
    // Arbitrary: any open pull request exercises cancellation. The constant
    // keeps the scripted response target, the generated detail, the listing,
    // and the fetch set on the same pull request.
    const CANCELLED_FETCH_PULL_NUMBER: u64 = 7;
    const PULL_UPDATED_AT: &str = "2026-08-03T12:30:00Z";
    const CHANGED_PULL_UPDATED_AT: &str = "2026-08-03T12:30:01Z";
    const POLL_INTERVAL: Duration = Duration::from_secs(300);
    const SHORT_CYCLE: Duration = Duration::from_secs(75);
    const SHORT_CYCLE_REMAINDER: Duration = Duration::from_secs(225);
    const OVERRUNNING_CYCLE: Duration = Duration::from_secs(900);
    const EMPTY_WORKFLOW_LIST: &str = "{\"workflows\":[]}";
    const MALFORMED_JSON: &str = "not-json";
    const CACHE_RESOURCE_KEY: &str = "fixture/resource";
    const CACHE_RETAINED_KEY: &str = "fixture/retained";
    const CACHE_STALE_KEY: &str = "fixture/stale";
    const CACHE_REPLACEMENT_KEY: &str = "fixture/replacement";
    const TEST_CACHE_RESOURCE_LIMIT: usize = 2;
    const CACHE_WIRE_BYTES: usize = 1;
    const CONCURRENTLY_REPLACED_ENTITY_TAG: &str = "\"fixture-etag-replaced\"";
    const REFRESHED_ENTITY_TAG: &str = "\"fixture-etag-refreshed\"";
    const CACHE_KEY_KIND: &str = "fixture-page";
    const CACHE_KEY_QUERY_VALUE: &str = "provider-controlled-branch";
    const CACHE_KEY_URL: &str =
        "https://api.github.com/repos/namespace/project/runs?branch=provider-controlled-branch";
    const EXPECTED_LIFECYCLE: RepoWatchPullRequestLifecycle = RepoWatchPullRequestLifecycle::Open;
    const EXPECTED_MERGEABLE_STATE: MergeableState = MergeableState::Conflicting;
    const EXPECTED_CHECKS_OUTCOME: ChecksOutcome = ChecksOutcome::Success;
    const EXPECTED_CHECK_RUN_CONCLUSION: CheckConclusion = CheckConclusion::Failure;
    const EXPECTED_REVIEW_STATE: Option<ReviewState> = Some(ReviewState::Approved);
    const EXPECTED_DISMISSED_REVIEW_STATE: Option<ReviewState> = None;
    const EXPECTED_OPEN_THREAD_STATE: RepoWatchThreadState = RepoWatchThreadState::Open;
    const EXPECTED_RESOLVED_THREAD_STATE: RepoWatchThreadState = RepoWatchThreadState::Resolved;
    const EXPECTED_FEATURE_WORKFLOW_CONCLUSION: CheckConclusion = CheckConclusion::Failure;
    const EXPECTED_MAIN_WORKFLOW_CONCLUSION: CheckConclusion = CheckConclusion::Success;
    const PULL_NUMBER: u64 = 7;
    const BASE_ADVANCE_EVENT_ID: u128 = 72;
    const HEAD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OWED_EVENT_HEAD_SHA: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const OWED_MATCH_COUNT: u64 = 51;
    const CHANGED_LISTED_HEAD_SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const BASE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD_REPOSITORY: &str = "fork/repository";
    const HEAD_BRANCH: &str = "feature/watch";
    const AGENT_HEAD_BRANCH: &str = "agent/dependent";
    const BASE_BRANCH: &str = "main";
    const PULL_TITLE: &str = "Exercise repository watch";
    const PULL_BODY: &str = "Typed fixture body";
    const PULL_AUTHOR: &str = "pull-author";
    const PULL_LABEL: &str = "watch-me";
    const CHECK_RUN_NAME: &str = "build";
    /// The completed check suite's `updated_at`, the provider member the poller
    /// adopts as a suite's completion generation. It differs from the run's so
    /// a poller reading a suite's generation onto a run cannot pass.
    const CHECK_SUITE_COMPLETION_GENERATION: &str = "2026-08-03T12:34:56Z";
    /// The completed check run's `completed_at`, the provider member the poller
    /// adopts as a run's completion generation.
    const CHECK_RUN_COMPLETION_GENERATION: &str = "2026-08-03T12:35:08Z";
    const QUEUED_CHECK_SUITE_UPDATED_AT: &str = "2026-08-03T12:35:18Z";
    const WORKFLOW_NAME: &str = "CI";
    const REVIEWER: &str = "signal-reviewer";
    const STALE_REVIEW_NODE_ID: &str = "PRR_fixture_stale";
    const STALE_REVIEW_HEAD_SHA: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const DISMISSAL_MESSAGE: &str = "Every finding is resolved on the current head.";
    const REVIEW_THREAD: &str = "PRRT_fixture_open";
    const RESOLVED_REVIEW_THREAD: &str = "PRRT_fixture_resolved";
    const PULL_NUMBERS: [u64; 1] = [PULL_NUMBER];
    const COMPLETED_CHECK_SUITE_IDS: [u64; 1] = [11];
    const QUEUED_CHECK_SUITE_ID: u64 = 12;
    const COMPLETED_CHECK_RUN_IDS: [u64; 1] = [21];
    const IN_PROGRESS_CHECK_RUN_ID: u64 = 22;
    const IN_PROGRESS_CHECK_RUN_NAME: &str = "lint";
    const RETAINED_REVIEW_IDS: [u64; 2] = [31, 32];
    const PENDING_REVIEW_ID: u64 = 33;
    // Distinct ordered identities preserve the provider order of the deferred
    // review fixtures.
    const DEFERRED_REVIEW_IDS: [u64; 3] = [34, 35, 36];
    const DEFERRED_USER_REVIEWER: &str = "watch-user";
    const DEFERRED_APPROVING_REVIEWER: &str = "review-agent-one[bot]";
    const DEFERRED_COMMENTING_REVIEWER: &str = "review-agent-two[bot]";
    const REVIEW_THREADS: [&str; 2] = [REVIEW_THREAD, RESOLVED_REVIEW_THREAD];
    const SIGNAL_REACTION_CONTENTS: [&str; 3] = ["+1", "rocket", "eyes"];
    const AMBIENT_REACTOR: &str = "ambient-user";
    const AMBIENT_REACTION_CONTENT: &str = "heart";
    const ISSUE_COMMENT_ID: u64 = 41;
    const REVIEW_COMMENT_ID: u64 = 51;
    const BRANCHES: [&str; 2] = [BASE_BRANCH, HEAD_BRANCH];
    const WORKFLOW_ID: u64 = 61;
    const WORKFLOW_RUN_IDS: [u64; 2] = [71, 72];
    const WORKFLOW_RUN_ATTEMPT: u64 = 1;
    const FOREIGN_WORKFLOW_RUN_ID: u64 = 73;
    const PROVIDER_HEAD_REPOSITORY: &str = "Fork/Repository";
    const PROVIDER_BASE_REPOSITORY: &str = "Namespace/Project";
    const PROVIDER_PULL_AUTHOR: &str = "Pull-Author";
    const PROVIDER_REVIEWER: &str = "Signal-Reviewer";
    const SCRIPTED_SERVER_TIMEOUT: Duration = Duration::from_secs(5);
    const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
    const DATABASE_NAME: &str = "signalbox_webhook_drain";
    const DATABASE_USER: &str = "signalbox";
    const DATABASE_PASSWORD: &str = "signalbox-test-only";
    const FIRST_WEBHOOK_DELIVERY: u128 = 0x7a01;
    const SECOND_WEBHOOK_DELIVERY: u128 = 0x7a02;
    const THIRD_WEBHOOK_DELIVERY: u128 = 0x7a03;
    const FIRST_WEBHOOK_REVIEW: u64 = 9_001;
    const SECOND_WEBHOOK_REVIEW: u64 = 9_002;
    const WEBHOOK_BODY_DIGEST_FILL: u8 = 0x71;
    const WEBHOOK_HOOK_ID: NonZeroU64 =
        NonZeroU64::new(7_001).expect("fixture webhook hook ID is positive");
    const WEBHOOK_PROJECTION_ADVISORY_LOCK: i64 = 70_001;

    async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
        let container = Postgres::default()
            .with_db_name(DATABASE_NAME)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_cmd(disposable_postgres_server_args())
            .with_mount(disposable_postgres_state_tmpfs_from_example()?)
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_labels(disposable_test_container_labels())
            .start()
            .await?;
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url =
            format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;
        Ok((container, pool))
    }

    fn webhook_delivery_key(value: u128) -> RepoWatchWebhookDeliveryKey {
        RepoWatchWebhookDeliveryKey::new(WEBHOOK_HOOK_ID, Uuid::from_u128(value))
    }

    fn submitted_review_admission(
        delivery: u128,
        review: u64,
    ) -> Result<RepoWatchWebhookAdmission, Box<dyn Error>> {
        let body = serde_json::json!({
            "action": "submitted",
            "number": PULL_NUMBER,
            "repository": {"full_name": WATCHED_REPOSITORY},
            "pull_request": {"head": {"sha": HEAD_SHA}},
            "review": {
                "id": review,
                "user": {"login": "reviewer"},
                "state": "approved",
                "commit_id": HEAD_SHA,
            },
        })
        .to_string()
        .into_bytes();
        Ok(RepoWatchWebhookAdmission::try_new(
            webhook_delivery_key(delivery),
            RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?,
            "pull_request_review".to_owned(),
            Some("submitted".to_owned()),
            [WEBHOOK_BODY_DIGEST_FILL; 32],
            body,
        )?)
    }

    const fn drain_failure() -> Result<(), RepositoryWatchAttemptError> {
        Err(RepositoryWatchAttemptError::Persistence)
    }

    /// A delivery outside the mapped set, which reaches terminal state without
    /// a provider request of its own.
    fn ignored_admission(delivery: u128) -> Result<RepoWatchWebhookAdmission, Box<dyn Error>> {
        let body = serde_json::json!({
            "action": "queued",
            "repository": {"full_name": WATCHED_REPOSITORY},
        })
        .to_string()
        .into_bytes();
        Ok(RepoWatchWebhookAdmission::try_new(
            webhook_delivery_key(delivery),
            RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?,
            "workflow_job".to_owned(),
            Some("queued".to_owned()),
            [WEBHOOK_BODY_DIGEST_FILL; 32],
            body,
        )?)
    }

    fn synchronize_admission(delivery: u128) -> Result<RepoWatchWebhookAdmission, Box<dyn Error>> {
        let body = serde_json::json!({
            "action": "synchronize",
            "number": PULL_NUMBER,
            "repository": {"full_name": WATCHED_REPOSITORY},
            "before": HEAD_SHA,
            "pull_request": {
                "title": PULL_TITLE,
                "body": PULL_BODY,
                "draft": false,
                "merged": false,
                "user": {"login": PROVIDER_PULL_AUTHOR},
                "labels": [],
                "base": {"ref": BASE_BRANCH},
                "head": {
                    "sha": CHANGED_LISTED_HEAD_SHA,
                    "ref": HEAD_BRANCH,
                    "repo": {"full_name": PROVIDER_HEAD_REPOSITORY}
                }
            }
        })
        .to_string()
        .into_bytes();
        Ok(RepoWatchWebhookAdmission::try_new(
            webhook_delivery_key(delivery),
            RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?,
            "pull_request".to_owned(),
            Some("synchronize".to_owned()),
            [WEBHOOK_BODY_DIGEST_FILL; 32],
            body,
        )?)
    }

    struct WebhookTaskFixture {
        task: RepositoryWatchTask,
        // The poller resolves its credential from a file in this directory on
        // every request, so a fixture that dropped it would fail every fetch
        // closed instead of exercising the scripted response.
        _credential_directory: TempDir,
    }

    async fn webhook_task(pool: &PgPool) -> Result<WebhookTaskFixture, Box<dyn Error>> {
        webhook_task_against(
            pool,
            Url::parse("http://127.0.0.1:9/").expect("fixture poller base URL is valid"),
        )
        .await
    }

    async fn webhook_task_against(
        pool: &PgPool,
        rest_base: Url,
    ) -> Result<WebhookTaskFixture, Box<dyn Error>> {
        let observation = complete_typed_observation().await;
        task_against(pool, rest_base, observation).await
    }

    /// The same task fixture over a caller-chosen committed cursor, for a test
    /// whose behavior depends on what that cursor observes.
    async fn task_against(
        pool: &PgPool,
        rest_base: Url,
        observation: RepoWatchObservation,
    ) -> Result<WebhookTaskFixture, Box<dyn Error>> {
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?;
        let store = PostgresRepoWatchStore::new(pool.clone());
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(observation),
                    Vec::new(),
                ),
            )
            .await?;
        let models = checked_in_example_configuration()?;
        let credential_pin = models.session_credential_pin();
        let (eligibility_nudge, _work_source) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
        let fixture = poller_fixture(rest_base)?;
        let poller = Arc::clone(&fixture.poller);
        Ok(WebhookTaskFixture {
            task: RepositoryWatchTask {
                webhook_nudge: None,
                webhook_primary: false,
                webhook_shadow: None,
                webhook_shadow_superseded: false,
                webhook_shadow_supersession_epoch: 0,
                webhook_projected_terminal_in_flight: None,
                webhook_dispatch_in_flight: false,
                webhook_targeted_completion: None,
                webhook_terminal_ambiguous: None,
                webhook_drain_first_failure: None,
                webhook_drain_projection_failure: None,
                webhook_drain_timed_out: false,
                webhook_attempt_phase: WebhookAttemptPhase::BeforeDrain,
                repository,
                interval: POLL_INTERVAL,
                poller,
                store,
                dispatch_store: PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin),
                rules: Vec::new(),
                templates: SessionTemplateConfiguration::default(),
                models,
                eligibility_nudge,
                webhook_store: PostgresRepoWatchWebhookStore::new(pool.clone()),
                webhook_work: None,
                startup_webhook_retry: None,
                reconciliation_quantum: None,
                webhook_drain_work_budget: None,
                payload_purge: WebhookPayloadPurgeSchedule::starting_now(),
                rules_activated: true,
            },
            _credential_directory: fixture._credential_directory,
        })
    }

    fn event_page_size() -> RepoWatchEventPageSize {
        RepoWatchEventPageSize::try_new(NonZeroU16::new(16).expect("a page size is positive"))
            .expect("sixteen is within the durable page ceiling")
    }

    async fn wait_for_webhook_projection_wedge(store: &PostgresRepoWatchWebhookStore) {
        let deadline = std::time::Instant::now() + SCRIPTED_SERVER_TIMEOUT;
        loop {
            if store
                .projection_wedge_is_reached()
                .await
                .expect("the fixture can inspect the wedge")
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the first webhook projection reaches its deliberate wedge"
            );
            tokio::task::yield_now().await;
        }
    }

    fn keep_paused_clock_runnable() -> JoinHandle<()> {
        tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        })
    }

    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured log locks").clone())
                .expect("captured webhook telemetry is UTF-8")
        }
    }

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("captured log locks")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLog {
        type Writer = CapturedLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    async fn webhook_disposition_exists(
        store: &PostgresRepoWatchWebhookStore,
        key: RepoWatchWebhookDeliveryKey,
    ) -> Result<bool, Box<dyn Error>> {
        Ok(store.load_disposition(key).await?.is_some())
    }

    async fn admit_submitted_review_burst(
        store: &PostgresRepoWatchWebhookStore,
        count: u16,
    ) -> Result<(), Box<dyn Error>> {
        const DELIVERY_BASE: u128 = 0x8a00;
        const REVIEW_BASE: u64 = 0x9a00;
        for offset in 0..count {
            let admission = submitted_review_admission(
                DELIVERY_BASE + u128::from(offset),
                REVIEW_BASE + u64::from(offset),
            )?;
            store.admit(&admission).await?;
        }
        Ok(())
    }

    fn pulls_with_one() -> String {
        serde_json::json!([{
            "number": PULL_NUMBERS[0],
            "updated_at": PULL_UPDATED_AT,
            "head": { "sha": HEAD_SHA }
        }])
        .to_string()
    }

    fn listed_pull_request(head_sha: &str) -> ListedPullRequest {
        ListedPullRequest {
            updated_at: PULL_UPDATED_AT.to_owned(),
            head_sha: CommitSha::try_new(head_sha.to_owned())
                .expect("fixture listed head is valid"),
        }
    }

    fn pull_detail() -> String {
        serde_json::json!({
            "number": PULL_NUMBER,
            "state": "open",
            "updated_at": PULL_UPDATED_AT,
            "merged_at": null,
            "mergeable": false,
            "head": {
                "sha": HEAD_SHA,
                "ref": HEAD_BRANCH,
                "repo": { "full_name": PROVIDER_HEAD_REPOSITORY }
            },
            "base": {
                "sha": BASE_SHA,
                "ref": BASE_BRANCH,
                "repo": { "full_name": PROVIDER_BASE_REPOSITORY }
            },
            "title": PULL_TITLE,
            "body": PULL_BODY,
            "labels": [{ "name": PULL_LABEL }],
            "draft": false,
            "user": { "login": PROVIDER_PULL_AUTHOR }
        })
        .to_string()
    }

    fn pull_detail_with_pending_mergeability() -> String {
        let mut detail = serde_json::from_str::<serde_json::Value>(&pull_detail())
            .expect("fixture pull detail is JSON");
        detail["mergeable"] = serde_json::Value::Null;
        detail.to_string()
    }

    /// The fixture pull request with mergeability decided in its favor, which
    /// a clearance revalidation needs: the shared fixture reports
    /// `CONFLICTING`, and that alone refuses every dismissal.
    fn mergeable_pull_detail() -> String {
        let mut detail = serde_json::from_str::<serde_json::Value>(&pull_detail())
            .expect("fixture pull detail is JSON");
        detail["mergeable"] = serde_json::Value::Bool(true);
        detail.to_string()
    }

    fn pull_detail_without_head_repository() -> String {
        let mut detail = serde_json::from_str::<serde_json::Value>(&pull_detail())
            .expect("fixture pull detail is JSON");
        detail["head"]["repo"] = serde_json::Value::Null;
        detail.to_string()
    }

    fn check_suites() -> String {
        serde_json::json!({
            "check_suites": [
                {
                    "id": COMPLETED_CHECK_SUITE_IDS[0],
                    "status": "completed",
                    "conclusion": "success",
                    "updated_at": CHECK_SUITE_COMPLETION_GENERATION
                },
                {
                    "id": QUEUED_CHECK_SUITE_ID,
                    "status": "queued",
                    "conclusion": null,
                    "updated_at": QUEUED_CHECK_SUITE_UPDATED_AT
                }
            ]
        })
        .to_string()
    }

    /// One completed and one unfinished check run, carrying the members the
    /// poller reads and no member the provider does not define. `updated_at` is
    /// absent because the provider defines it on a check suite and never on a
    /// check run; a fixture that supplied it would describe a page no provider
    /// can send. [`provider_defined_check_runs`] carries the provider's
    /// complete member set.
    fn check_runs() -> String {
        serde_json::json!({
            "check_runs": [
                {
                    "id": COMPLETED_CHECK_RUN_IDS[0],
                    "status": "completed",
                    "name": CHECK_RUN_NAME,
                    "conclusion": "failure",
                    "completed_at": CHECK_RUN_COMPLETION_GENERATION
                },
                {
                    "id": IN_PROGRESS_CHECK_RUN_ID,
                    "status": "in_progress",
                    "name": IN_PROGRESS_CHECK_RUN_NAME,
                    "conclusion": null,
                    "completed_at": null
                }
            ]
        })
        .to_string()
    }

    /// A completed check run carrying exactly the member set the provider's
    /// check-runs response defines for a run — the payload the decoder must
    /// survive. Only the members the decoder reads carry meaningful values,
    /// drawn from this module's constants; every other member exists so that
    /// the set is complete and its value is arbitrary, with the nested `app`,
    /// `check_suite`, `output`, and `pull_requests` bodies abridged, since it is
    /// the top-level member names that this fixture pins. `updated_at` is not
    /// among them, so a decoder requiring it rejects this page.
    fn provider_defined_check_runs() -> String {
        serde_json::json!({
            "check_runs": [
                {
                    "app": { "id": 15368, "slug": "github-actions" },
                    "check_suite": { "id": COMPLETED_CHECK_SUITE_IDS[0] },
                    "completed_at": CHECK_RUN_COMPLETION_GENERATION,
                    "conclusion": "failure",
                    "details_url": "https://provider.invalid/checks/21",
                    "external_id": "b1a5bc25-67cd-58b1-b7c0-449a03988c8c",
                    "head_sha": HEAD_SHA,
                    "html_url": "https://provider.invalid/checks/21",
                    "id": COMPLETED_CHECK_RUN_IDS[0],
                    "name": CHECK_RUN_NAME,
                    "node_id": "CR_provider_defined_check_run",
                    "output": {
                        "annotations_count": 0,
                        "annotations_url": "https://provider.invalid/checks/21/annotations",
                        "summary": null,
                        "text": null,
                        "title": null
                    },
                    "pull_requests": [],
                    "started_at": "2026-08-03T12:30:00Z",
                    "status": "completed",
                    "url": "https://provider.invalid/checks/21"
                }
            ]
        })
        .to_string()
    }

    /// The provider-defined page with the completion time the provider
    /// populates on every completed run removed. A run that reports `completed`
    /// without one carries no generation the differ could compare.
    fn completed_check_run_without_a_completion_time() -> String {
        let mut page = serde_json::from_str::<serde_json::Value>(&provider_defined_check_runs())
            .expect("provider-defined check-runs fixture is JSON");
        page["check_runs"][0]["completed_at"] = serde_json::Value::Null;
        page.to_string()
    }

    /// The check-suites page once every suite has settled: the completed suite
    /// from [`check_suites`] alone, stated directly so the settled payload is
    /// inspectable rather than recalculated by the completion filter the
    /// poller itself applies.
    fn settled_check_suites() -> String {
        serde_json::json!({
            "check_suites": [
                {
                    "id": COMPLETED_CHECK_SUITE_IDS[0],
                    "status": "completed",
                    "conclusion": "success",
                    "updated_at": CHECK_SUITE_COMPLETION_GENERATION
                }
            ]
        })
        .to_string()
    }

    /// The check-runs page once every run has settled: the completed run from
    /// [`check_runs`] alone, stated directly for the same reason as
    /// [`settled_check_suites`].
    fn settled_check_runs() -> String {
        serde_json::json!({
            "check_runs": [
                {
                    "id": COMPLETED_CHECK_RUN_IDS[0],
                    "status": "completed",
                    "name": CHECK_RUN_NAME,
                    "conclusion": "failure",
                    "completed_at": CHECK_RUN_COMPLETION_GENERATION
                }
            ]
        })
        .to_string()
    }

    fn reviews() -> String {
        serde_json::json!([
            {
                "id": RETAINED_REVIEW_IDS[0],
                "user": { "login": PROVIDER_REVIEWER },
                "state": "APPROVED",
                "commit_id": HEAD_SHA
            },
            {
                "id": RETAINED_REVIEW_IDS[1],
                "user": { "login": PROVIDER_REVIEWER },
                "state": "DISMISSED",
                "commit_id": HEAD_SHA
            },
            {
                "id": PENDING_REVIEW_ID,
                "user": { "login": PROVIDER_REVIEWER },
                "state": "PENDING",
                "commit_id": HEAD_SHA
            }
        ])
        .to_string()
    }

    fn reviews_with_deferred_wave() -> String {
        serde_json::json!([
            {
                "id": RETAINED_REVIEW_IDS[0],
                "user": { "login": PROVIDER_REVIEWER },
                "state": "APPROVED",
                "commit_id": HEAD_SHA
            },
            {
                "id": RETAINED_REVIEW_IDS[1],
                "user": { "login": PROVIDER_REVIEWER },
                "state": "DISMISSED",
                "commit_id": HEAD_SHA
            },
            {
                "id": PENDING_REVIEW_ID,
                "user": { "login": PROVIDER_REVIEWER },
                "state": "PENDING",
                "commit_id": HEAD_SHA
            },
            {
                "id": DEFERRED_REVIEW_IDS[0],
                "user": { "login": DEFERRED_USER_REVIEWER },
                "state": "COMMENTED",
                "commit_id": HEAD_SHA
            },
            {
                "id": DEFERRED_REVIEW_IDS[1],
                "user": { "login": DEFERRED_APPROVING_REVIEWER },
                "state": "APPROVED",
                "commit_id": HEAD_SHA
            },
            {
                "id": DEFERRED_REVIEW_IDS[2],
                "user": { "login": DEFERRED_COMMENTING_REVIEWER },
                "state": "COMMENTED",
                "commit_id": HEAD_SHA
            }
        ])
        .to_string()
    }

    fn identity_less_review(id: u64) -> String {
        serde_json::json!([{
            "id": id,
            "user": null,
            "state": "APPROVED",
            "commit_id": HEAD_SHA
        }])
        .to_string()
    }

    fn threads() -> String {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                { "id": REVIEW_THREADS[0], "isResolved": false },
                                { "id": REVIEW_THREADS[1], "isResolved": true }
                            ],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn convergence() -> String {
        convergence_with_mergeability("CONFLICTING")
    }

    /// Convergence evidence for a head whose only remaining blocker is the
    /// aggregate review decision: one complete, green, gating check and no
    /// other. This is the evidence a stale-review clearance is allowed to act
    /// on, so it is what a revalidation must be able to read back.
    fn review_only_blocked_convergence() -> String {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD_SHA,
                        "baseRefName": BASE_BRANCH,
                        "baseRefOid": BASE_SHA,
                        "mergeable": "MERGEABLE",
                        "reviewDecision": "CHANGES_REQUESTED",
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD_SHA,
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "nodes": [{
                                                "__typename": "CheckRun",
                                                "name": CHECK_RUN_NAME,
                                                "status": "COMPLETED",
                                                "conclusion": "SUCCESS"
                                            }],
                                            "pageInfo": {
                                                "hasNextPage": false,
                                                "endCursor": null
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn convergence_with_mergeability(mergeable: &str) -> String {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD_SHA,
                        "baseRefName": BASE_BRANCH,
                        "baseRefOid": BASE_SHA,
                        "mergeable": mergeable,
                        "reviewDecision": "APPROVED",
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD_SHA,
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "nodes": [
                                                {
                                                    "__typename": "CheckRun",
                                                    "name": CHECK_RUN_NAME,
                                                    "status": "COMPLETED",
                                                    "conclusion": "FAILURE"
                                                },
                                                {
                                                    "__typename": "CheckRun",
                                                    "name": "coverage (report only)",
                                                    "status": "IN_PROGRESS",
                                                    "conclusion": null
                                                },
                                                {
                                                    "__typename": "StatusContext",
                                                    "context": "CodeRabbit",
                                                    "state": "ERROR"
                                                }
                                            ],
                                            "pageInfo": {
                                                "hasNextPage": false,
                                                "endCursor": null
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn blocking_reviews(reviewed_head_sha: &str) -> String {
        blocking_reviews_by(REVIEWER, reviewed_head_sha)
    }

    fn blocking_reviews_by(reviewer: &str, reviewed_head_sha: &str) -> String {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD_SHA,
                        "baseRefOid": BASE_SHA,
                        "reviewDecision": "CHANGES_REQUESTED",
                        "latestOpinionatedReviews": {
                            "nodes": [{
                                "id": STALE_REVIEW_NODE_ID,
                                "state": "CHANGES_REQUESTED",
                                "author": { "login": reviewer },
                                "commit": { "oid": reviewed_head_sha }
                            }],
                            "pageInfo": {
                                "hasNextPage": false,
                                "endCursor": null
                            }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn dismissed_review(review_node_id: &str) -> String {
        serde_json::json!({
            "data": {
                "dismissPullRequestReview": {
                    "pullRequestReview": {
                        "id": review_node_id,
                        "state": "DISMISSED"
                    }
                }
            }
        })
        .to_string()
    }

    fn pull_reactions() -> String {
        serde_json::json!([
            {
                "user": { "login": PROVIDER_REVIEWER },
                "content": SIGNAL_REACTION_CONTENTS[0]
            },
            {
                "user": { "login": AMBIENT_REACTOR },
                "content": AMBIENT_REACTION_CONTENT
            }
        ])
        .to_string()
    }

    fn identity_less_reaction() -> String {
        serde_json::json!([{
            "user": null,
            "content": SIGNAL_REACTION_CONTENTS[0]
        }])
        .to_string()
    }

    fn issue_comments() -> String {
        serde_json::json!([{ "id": ISSUE_COMMENT_ID }]).to_string()
    }

    fn review_comments() -> String {
        serde_json::json!([{ "id": REVIEW_COMMENT_ID }]).to_string()
    }

    fn issue_comment_reactions() -> String {
        serde_json::json!([{
            "user": { "login": PROVIDER_REVIEWER },
            "content": SIGNAL_REACTION_CONTENTS[1]
        }])
        .to_string()
    }

    fn review_comment_reactions() -> String {
        serde_json::json!([{
            "user": { "login": PROVIDER_REVIEWER },
            "content": SIGNAL_REACTION_CONTENTS[2]
        }])
        .to_string()
    }

    fn branches() -> String {
        serde_json::json!([
            { "name": BRANCHES[0], "commit": { "sha": BASE_SHA } },
            { "name": BRANCHES[1], "commit": { "sha": HEAD_SHA } }
        ])
        .to_string()
    }

    fn workflows() -> String {
        serde_json::json!({
            "workflows": [{ "id": WORKFLOW_ID, "name": WORKFLOW_NAME }]
        })
        .to_string()
    }

    fn main_workflow_run() -> String {
        serde_json::json!({
            "workflow_runs": [
                {
                    "id": WORKFLOW_RUN_IDS[0],
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": BASE_BRANCH,
                    "status": "completed",
                    "conclusion": "success",
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                },
                {
                    "id": WORKFLOW_RUN_IDS[1],
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": HEAD_BRANCH,
                    "status": "completed",
                    "conclusion": "failure",
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                }
            ]
        })
        .to_string()
    }

    fn active_rerun_then_stale_workflow_run() -> String {
        serde_json::json!({
            "workflow_runs": [
                {
                    "id": FOREIGN_WORKFLOW_RUN_ID,
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": HEAD_BRANCH,
                    "status": "completed",
                    "conclusion": "failure",
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                },
                {
                    "id": FOREIGN_WORKFLOW_RUN_ID + 1,
                    "run_attempt": WORKFLOW_RUN_ATTEMPT + 1,
                    "head_branch": BASE_BRANCH,
                    "status": "in_progress",
                    "conclusion": null,
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                },
                {
                    "id": WORKFLOW_RUN_IDS[0],
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": BASE_BRANCH,
                    "status": "completed",
                    "conclusion": "success",
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                }
            ]
        })
        .to_string()
    }

    fn foreign_then_watched_workflow_runs() -> String {
        serde_json::json!({
            "workflow_runs": [
                {
                    "id": FOREIGN_WORKFLOW_RUN_ID,
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": BASE_BRANCH,
                    "status": "completed",
                    "conclusion": "failure",
                    "head_repository": { "full_name": PROVIDER_HEAD_REPOSITORY }
                },
                {
                    "id": WORKFLOW_RUN_IDS[0],
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": BASE_BRANCH,
                    "status": "completed",
                    "conclusion": "success",
                    "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
                }
            ]
        })
        .to_string()
    }

    fn full_foreign_workflow_run_page() -> String {
        let workflow_runs = (0..PAGE_SIZE)
            .map(|offset| {
                serde_json::json!({
                    "id": FOREIGN_WORKFLOW_RUN_ID + u64::try_from(offset)
                        .expect("fixture page offset fits u64"),
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "head_branch": BASE_BRANCH,
                    "status": "completed",
                    "conclusion": "failure",
                    "head_repository": { "full_name": PROVIDER_HEAD_REPOSITORY }
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "workflow_runs": workflow_runs }).to_string()
    }

    fn base_branch_head() -> RepoWatchBranchHead {
        RepoWatchBranchHead::new(
            BranchName::try_new(String::from(BASE_BRANCH)).expect("fixture branch is valid"),
            CommitSha::try_new(String::from(BASE_SHA)).expect("fixture commit is valid"),
        )
    }

    fn merged_baseline_for_number(
        source: &RepoWatchPullRequestState,
        number: PullRequestNumber,
        signal_reviewers: &[RepoWatchAuthorLogin],
    ) -> RepoWatchMergedPullRequestBaselineV1 {
        let context = source.context();
        let merged = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number,
                head_sha: context.head_sha().clone(),
                head_repository: context.head_repository().clone(),
                base_branch: context.base_branch().clone(),
                head_branch: context.head_branch().clone(),
                title: context.title().clone(),
                body: context.body().clone(),
                labels: context.labels().to_vec(),
                draft: context.draft(),
                author: context.author().cloned(),
            }),
            lifecycle: RepoWatchPullRequestLifecycle::Merged,
            mergeable_state: source.mergeable_state(),
            completed_check_suites: source.completed_check_suites().to_vec(),
            completed_check_runs: source.completed_check_runs().to_vec(),
            reviews: source.reviews().to_vec(),
            threads: source.threads().to_vec(),
            reactions: source.reactions().to_vec(),
        })
        .expect("fixture merged pull request is canonical");
        RepoWatchMergedPullRequestBaselineV1::from_merged_state(&merged, signal_reviewers)
            .expect("fixture compact baseline is canonical")
            .expect("fixture merged pull request produces a baseline")
    }

    fn submitted_review(id: u64) -> RepoWatchReviewObservation {
        RepoWatchReviewObservation::new(
            object_id(id).expect("fixture review identity is positive"),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
                .expect("fixture reviewer is valid"),
            EXPECTED_REVIEW_STATE,
            CommitSha::try_new(String::from(HEAD_SHA)).expect("fixture review commit is valid"),
        )
    }

    fn workflow_response() -> WorkflowResponse {
        WorkflowResponse {
            id: WORKFLOW_ID,
            name: String::from(WORKFLOW_NAME),
        }
    }

    fn previous_main_workflow_run() -> RepoWatchWorkflowRunObservation {
        RepoWatchWorkflowRunObservation::new(
            object_id(FOREIGN_WORKFLOW_RUN_ID + 1)
                .expect("fixture workflow-run identity is positive"),
            object_id(WORKFLOW_ID).expect("fixture workflow identity is positive"),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(WORKFLOW_RUN_ATTEMPT)
                    .expect("fixture workflow-run attempt is positive"),
            ),
            BranchName::try_new(String::from(BASE_BRANCH)).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from(WORKFLOW_NAME))
                .expect("fixture workflow name is valid"),
            EXPECTED_MAIN_WORKFLOW_CONCLUSION,
        )
    }

    fn older_main_workflow_run() -> RepoWatchWorkflowRunObservation {
        RepoWatchWorkflowRunObservation::new(
            object_id(WORKFLOW_RUN_IDS[0] - 1).expect("fixture workflow-run identity is positive"),
            object_id(WORKFLOW_ID).expect("fixture workflow identity is positive"),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(WORKFLOW_RUN_ATTEMPT)
                    .expect("fixture workflow-run attempt is positive"),
            ),
            BranchName::try_new(String::from(BASE_BRANCH)).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from(WORKFLOW_NAME))
                .expect("fixture workflow name is valid"),
            EXPECTED_MAIN_WORKFLOW_CONCLUSION,
        )
    }

    fn full_workflow_page() -> String {
        let workflows = (1..=PAGE_SIZE)
            .map(|identity| {
                serde_json::json!({
                    "id": identity,
                    "name": format!("workflow-{identity}")
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "workflows": workflows }).to_string()
    }

    fn issue_comment_subject() -> ReactionSubject {
        ReactionSubject::IssueComment {
            id: object_id(ISSUE_COMMENT_ID).expect("fixture issue-comment identity is positive"),
        }
    }

    fn review_comment_subject() -> ReactionSubject {
        ReactionSubject::ReviewComment {
            id: object_id(REVIEW_COMMENT_ID).expect("fixture review-comment identity is positive"),
        }
    }

    /// The request line a scripted response answers. Distinct from
    /// [`ResponseBody`] because the two travel through the same constructors:
    /// with both as plain strings, a transposed pair still compiles and is
    /// caught only by the server rejecting the request.
    struct RequestTarget(String);

    /// The payload a scripted response returns. See [`RequestTarget`].
    struct ResponseBody(String);

    struct ScriptedResponse {
        method: &'static str,
        target: String,
        request_body_marker: Option<String>,
        validator: Option<&'static str>,
        status: &'static str,
        entity_tag: Option<&'static str>,
        link: Option<&'static str>,
        retry_after: Option<&'static str>,
        declared_content_length: Option<usize>,
        body: String,
        delay: Duration,
    }

    impl ScriptedResponse {
        fn ok(target: RequestTarget, body: ResponseBody) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: None,
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: body.0,
                delay: Duration::ZERO,
            }
        }

        fn ok_with_next(target: RequestTarget, body: ResponseBody) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: None,
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                link: Some(NEXT_PAGE_LINK),
                retry_after: None,
                declared_content_length: None,
                body: body.0,
                delay: Duration::ZERO,
            }
        }

        fn conditional_ok(target: RequestTarget, body: ResponseBody) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: Some(ENTITY_TAG),
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: body.0,
                delay: Duration::ZERO,
            }
        }

        fn not_found(target: RequestTarget) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: None,
                status: "404 Not Found",
                entity_tag: None,
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: String::from("{}"),
                delay: Duration::ZERO,
            }
        }

        fn forbidden(target: RequestTarget) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: None,
                status: "403 Forbidden",
                entity_tag: None,
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: String::from("{}"),
                delay: Duration::ZERO,
            }
        }

        fn not_modified(target: RequestTarget) -> Self {
            Self {
                method: "GET",
                target: target.0,
                request_body_marker: None,
                validator: Some(ENTITY_TAG),
                status: "304 Not Modified",
                entity_tag: None,
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: String::new(),
                delay: Duration::ZERO,
            }
        }

        fn delayed(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        /// Marks a rejection as the provider's own rate limit.
        ///
        /// A secondary limit is what carries this header, and it is what
        /// separates a throttled `403` from the permission-scoped `403` an
        /// under-scoped credential receives on one endpoint.
        fn throttled(mut self) -> Self {
            self.retry_after = Some("60");
            self
        }

        /// Promises more body than the connection then delivers.
        ///
        /// The declared length outruns the bytes written and the socket closes,
        /// so the client's body stream fails part-way — the transport failure a
        /// rejection can hit while it is being read for classification.
        fn truncated(mut self) -> Self {
            self.declared_content_length = Some(self.body.len() + 1);
            self
        }

        fn post(target: RequestTarget, body: ResponseBody) -> Self {
            Self {
                method: "POST",
                target: target.0,
                request_body_marker: None,
                validator: None,
                status: "200 OK",
                entity_tag: None,
                link: None,
                retry_after: None,
                declared_content_length: None,
                body: body.0,
                delay: Duration::ZERO,
            }
        }

        fn matching_request_body(mut self, marker: String) -> Self {
            self.request_body_marker = Some(marker);
            self
        }
    }

    struct ConcurrentScriptedState {
        responses: Mutex<Vec<ScriptedResponse>>,
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        unmatched: AtomicUsize,
    }

    struct ConcurrentScriptedServer {
        base_url: Url,
        state: Arc<ConcurrentScriptedState>,
        task: JoinHandle<()>,
    }

    impl ConcurrentScriptedServer {
        async fn start(responses: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("loopback listener binds");
            let address = listener.local_addr().expect("listener has an address");
            let base_url =
                Url::parse(&format!("http://{address}/")).expect("loopback address forms a URL");
            let state = Arc::new(ConcurrentScriptedState {
                responses: Mutex::new(responses),
                in_flight: AtomicUsize::new(0),
                peak_in_flight: AtomicUsize::new(0),
                unmatched: AtomicUsize::new(0),
            });
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move { serve_matched_response(stream, &state).await });
                    }
                }
            });
            Self {
                base_url,
                state,
                task,
            }
        }

        /// Waits until the server holds a request in flight, so a caller can
        /// cancel a fetch that is demonstrably mid-request. Bounded, so a
        /// regression that keeps the fetch from ever reaching the listener
        /// fails the test locally instead of hanging it until the job times
        /// out.
        async fn request_in_flight(&self) {
            let arrival = async {
                while self.state.in_flight.load(Ordering::SeqCst) == 0 {
                    sleep(Duration::from_millis(1)).await;
                }
            };
            tokio::time::timeout(SCRIPTED_SERVER_TIMEOUT, arrival)
                .await
                .expect("a scripted request goes in flight before the deadline");
        }

        async fn finish(self) -> usize {
            self.task.abort();
            let remaining = self
                .state
                .responses
                .lock()
                .expect("scripted responses are readable")
                .len();
            assert_eq!(remaining, 0, "every scripted response is consumed");
            assert_eq!(
                self.state.unmatched.load(Ordering::SeqCst),
                0,
                "every request matches a scripted response"
            );
            self.state.peak_in_flight.load(Ordering::SeqCst)
        }
    }

    async fn serve_matched_response(mut stream: TcpStream, state: &ConcurrentScriptedState) {
        let in_flight = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        let request = read_request(&mut stream).await;
        let start_line = request
            .lines()
            .next()
            .expect("request has a start line")
            .to_owned();
        let matched = {
            let mut responses = state
                .responses
                .lock()
                .expect("scripted responses are readable");
            responses
                .iter()
                .position(|response| {
                    start_line == format!("{} {} HTTP/1.1", response.method, response.target)
                        && response
                            .request_body_marker
                            .as_ref()
                            .is_none_or(|marker| request.contains(marker))
                })
                .map(|position| responses.remove(position))
        };
        match matched {
            Some(response) => write_response(&mut stream, &response).await,
            None => {
                state.unmatched.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("unmatched refusal can be written");
            }
        }
        state.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    struct ScriptedServer {
        base_url: Url,
        task: JoinHandle<()>,
    }

    impl ScriptedServer {
        async fn start(responses: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("loopback listener binds");
            let address = listener.local_addr().expect("listener has an address");
            let base_url =
                Url::parse(&format!("http://{address}/")).expect("loopback address forms a URL");
            let task = tokio::spawn(async move {
                for response in responses {
                    serve_response(&listener, response).await;
                }
            });
            Self { base_url, task }
        }

        async fn finish(self) {
            tokio::time::timeout(SCRIPTED_SERVER_TIMEOUT, self.task)
                .await
                .expect("scripted server consumes every expected request")
                .expect("scripted server completes");
        }
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1_024];
            let read = stream
                .read(&mut chunk)
                .await
                .expect("scripted request can be read");
            request.extend_from_slice(&chunk[..read]);
            if scripted_request_is_complete(&request) {
                break;
            }
            assert_ne!(read, 0, "request body must be complete");
        }
        String::from_utf8(request).expect("request headers are UTF-8")
    }

    fn scripted_request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let header_end = header_end + 4;
        let headers = std::str::from_utf8(&request[..header_end])
            .expect("scripted request headers are UTF-8");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| {
                value
                    .trim()
                    .parse::<usize>()
                    .expect("scripted request content length is valid")
            });
        request.len() >= header_end + content_length
    }

    async fn write_response(stream: &mut TcpStream, response: &ScriptedResponse) {
        sleep(response.delay).await;
        let entity_tag = response
            .entity_tag
            .map(|value| format!("ETag: {value}\r\n"))
            .unwrap_or_default();
        let link = response
            .link
            .map(|value| format!("Link: {value}\r\n"))
            .unwrap_or_default();
        let retry_after = response
            .retry_after
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        let encoded = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{}{}{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            entity_tag,
            link,
            retry_after,
            response
                .declared_content_length
                .unwrap_or(response.body.len()),
            response.body,
        );
        stream
            .write_all(encoded.as_bytes())
            .await
            .expect("scripted response can be written");
    }

    async fn serve_response(listener: &TcpListener, response: ScriptedResponse) {
        let (mut stream, _) = listener.accept().await.expect("scripted request arrives");
        let request = read_request(&mut stream).await;
        let start_line = request.lines().next().expect("request has a start line");
        assert_eq!(
            start_line,
            format!("{} {} HTTP/1.1", response.method, response.target)
        );
        let lowercase_request = request.to_ascii_lowercase();
        assert!(lowercase_request.contains(&format!("authorization: bearer {}", CREDENTIAL_VALUE)));
        match response.validator {
            Some(validator) => assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case(&format!("if-none-match: {validator}")))
            ),
            None => assert!(!lowercase_request.contains("if-none-match:")),
        }
        write_response(&mut stream, &response).await;
    }

    struct PollerFixture {
        poller: Arc<GitHubRepositoryPoller>,
        _credential_directory: TempDir,
    }

    fn poller_fixture(
        rest_base: Url,
    ) -> Result<PollerFixture, RepositoryWatchRuntimeConstructionError> {
        let reviewer = RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
            .expect("reviewer fixture is valid");
        poller_fixture_with_signal_reviewers(rest_base, vec![reviewer])
    }

    fn poller_fixture_with_signal_reviewers(
        rest_base: Url,
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
    ) -> Result<PollerFixture, RepositoryWatchRuntimeConstructionError> {
        let credential_directory = TempDir::new().expect("credential directory is created");
        let credential_file: PathBuf = credential_directory.path().join(CREDENTIAL_FILE_NAME);
        fs::write(&credential_file, format!("{CREDENTIAL_VALUE}\n"))
            .expect("credential fixture is written");
        let credential_reference = CredentialReference::new(CREDENTIAL_REFERENCE);
        let credentials = FileCredentialAccess::new_bounded(
            credential_file,
            credential_reference.clone(),
            super::MAX_CREDENTIAL_BYTES,
        );
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())
            .expect("repository fixture is valid");
        let poller = GitHubRepositoryPoller::try_new_with_rest_base(
            repository,
            signal_reviewers,
            credentials,
            credential_reference,
            rest_base,
        )?;
        Ok(PollerFixture {
            poller: Arc::new(poller),
            _credential_directory: credential_directory,
        })
    }

    fn complete_poll_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(EMPTY_WORKFLOW_LIST.to_owned()),
            ),
        ]
    }

    fn conditional_poll_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::not_modified(RequestTarget(PULLS_TARGET.to_owned())),
            ScriptedResponse::not_modified(RequestTarget(BRANCHES_TARGET.to_owned())),
            ScriptedResponse::not_modified(RequestTarget(WORKFLOWS_TARGET.to_owned())),
        ]
    }

    fn complete_typed_observation_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(pulls_with_one()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_DETAIL_TARGET.to_owned()),
                ResponseBody(pull_detail()),
            ),
            ScriptedResponse::ok(
                RequestTarget(CHECK_SUITES_TARGET.to_owned()),
                ResponseBody(check_suites()),
            ),
            ScriptedResponse::ok(
                RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
                ResponseBody(check_runs()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEWS_TARGET.to_owned()),
                ResponseBody(reviews()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(convergence()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(threads()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(pull_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(issue_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(issue_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(review_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(review_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(workflows()),
            ),
            ScriptedResponse::ok(
                RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
                ResponseBody(main_workflow_run()),
            ),
        ]
    }

    /// The same complete sweep over a pull request whose only remaining
    /// convergence blocker is its aggregate review decision: GitHub reports it
    /// mergeable and every review thread is resolved. This is the state a stale
    /// blocking review may be dismissed against, so it is the cursor a clearance
    /// is planned and dismissed from.
    fn review_only_blocked_observation_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(pulls_with_one()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_DETAIL_TARGET.to_owned()),
                ResponseBody(mergeable_pull_detail()),
            ),
            ScriptedResponse::ok(
                RequestTarget(CHECK_SUITES_TARGET.to_owned()),
                ResponseBody(check_suites()),
            ),
            ScriptedResponse::ok(
                RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
                ResponseBody(check_runs()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEWS_TARGET.to_owned()),
                ResponseBody(reviews()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(review_only_blocked_convergence()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(empty_threads()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(pull_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(issue_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(issue_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(review_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(review_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(workflows()),
            ),
            ScriptedResponse::ok(
                RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
                ResponseBody(main_workflow_run()),
            ),
        ]
    }

    fn complete_pull_request_responses() -> Vec<ScriptedResponse> {
        complete_typed_observation_responses()
            .into_iter()
            // The per-pull-request slice of the complete sweep: everything from
            // the pull detail through the review-comment reactions, with the
            // repository listing and branch page ahead of it and the workflow
            // queries behind it excluded.
            .skip(2)
            .take(11)
            .collect()
    }

    /// Descends as the pull number ascends, so an implementation that
    /// accidentally orders fetched pull requests by head identity or head
    /// branch reverses the expected number order instead of matching it.
    fn minimal_pull_head_seed(number: u64) -> u64 {
        u64::MAX - number
    }

    fn minimal_pull_head_sha(number: u64) -> String {
        format!("{:040x}", minimal_pull_head_seed(number))
    }

    fn minimal_pull_detail(number: u64) -> String {
        serde_json::json!({
            "number": number,
            "state": "open",
            "updated_at": PULL_UPDATED_AT,
            "merged_at": null,
            "mergeable": true,
            "head": {
                "sha": minimal_pull_head_sha(number),
                "ref": format!("{HEAD_BRANCH}-{}", minimal_pull_head_seed(number)),
                "repo": { "full_name": PROVIDER_HEAD_REPOSITORY }
            },
            "base": {
                "sha": BASE_SHA,
                "ref": BASE_BRANCH,
                "repo": { "full_name": PROVIDER_BASE_REPOSITORY }
            },
            "title": PULL_TITLE,
            "body": PULL_BODY,
            "labels": [],
            "draft": false,
            "user": { "login": PROVIDER_PULL_AUTHOR }
        })
        .to_string()
    }

    fn empty_threads() -> String {
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn minimal_convergence(number: u64) -> String {
        let head_sha = minimal_pull_head_sha(number);
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": head_sha,
                        "baseRefName": BASE_BRANCH,
                        "baseRefOid": BASE_SHA,
                        "mergeable": "MERGEABLE",
                        "reviewDecision": null,
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": minimal_pull_head_sha(number),
                                    "statusCheckRollup": null
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string()
    }

    fn minimal_pull_responses(number: u64) -> Vec<ScriptedResponse> {
        let head_sha = minimal_pull_head_sha(number);
        vec![
            ScriptedResponse::ok(
                RequestTarget(format!("/repos/{WATCHED_REPOSITORY}/pulls/{number}")),
                ResponseBody(minimal_pull_detail(number)),
            )
            .delayed(CONCURRENT_FETCH_DELAY),
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/commits/{head_sha}/check-suites?filter=all&per_page=100&page=1"
                )),
                ResponseBody(EMPTY_CHECK_SUITE_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/pulls/{number}/reviews?per_page=100&page=1"
                )),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(minimal_convergence(number)),
            )
            .matching_request_body(format!("\"number\":{number}")),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(empty_threads()),
            )
            .matching_request_body(format!("\"number\":{number}")),
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/issues/{number}/reactions?per_page=100&page=1"
                )),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/issues/{number}/comments?per_page=100&page=1"
                )),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/pulls/{number}/comments?per_page=100&page=1"
                )),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
        ]
    }

    fn settled_typed_observation_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(pulls_with_one()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_DETAIL_TARGET.to_owned()),
                ResponseBody(pull_detail()),
            ),
            ScriptedResponse::ok(
                RequestTarget(CHECK_SUITES_TARGET.to_owned()),
                ResponseBody(settled_check_suites()),
            ),
            ScriptedResponse::ok(
                RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
                ResponseBody(settled_check_runs()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEWS_TARGET.to_owned()),
                ResponseBody(reviews()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(convergence()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(threads()),
            ),
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(pull_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(issue_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(issue_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(review_comments()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(review_comment_reactions()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(workflows()),
            ),
            ScriptedResponse::ok(
                RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
                ResponseBody(main_workflow_run()),
            ),
        ]
    }

    fn skipped_pull_request_responses() -> Vec<ScriptedResponse> {
        skipped_pull_request_responses_with_reviews(reviews())
    }

    fn skipped_pull_request_responses_with_deferred_reviews() -> Vec<ScriptedResponse> {
        skipped_pull_request_responses_with_reviews(reviews_with_deferred_wave())
    }

    fn skipped_pull_request_responses_with_reviews(reviews: String) -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::conditional_ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(pulls_with_one()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(REVIEWS_TARGET.to_owned()),
                ResponseBody(reviews),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(convergence()),
            ),
            ScriptedResponse::post(
                RequestTarget(THREADS_TARGET.to_owned()),
                ResponseBody(threads()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(pull_reactions()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(issue_comments()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(ISSUE_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(issue_comment_reactions()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(review_comments()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(REVIEW_COMMENT_REACTIONS_TARGET.to_owned()),
                ResponseBody(review_comment_reactions()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(workflows()),
            ),
            ScriptedResponse::conditional_ok(
                RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
                ResponseBody(main_workflow_run()),
            ),
        ]
    }

    fn settled_responses_with_pending_mergeability() -> Vec<ScriptedResponse> {
        with_pending_mergeability(settled_typed_observation_responses())
    }

    fn responses_with_only_an_unsettled_check_suite() -> Vec<ScriptedResponse> {
        complete_typed_observation_responses()
            .into_iter()
            .map(|response| {
                if response.target == COMMIT_CHECK_RUNS_TARGET {
                    ScriptedResponse::ok(
                        RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
                        ResponseBody(settled_check_runs()),
                    )
                } else {
                    response
                }
            })
            .collect()
    }

    fn responses_with_only_an_unsettled_check_run() -> Vec<ScriptedResponse> {
        complete_typed_observation_responses()
            .into_iter()
            .map(|response| {
                if response.target == CHECK_SUITES_TARGET {
                    ScriptedResponse::ok(
                        RequestTarget(CHECK_SUITES_TARGET.to_owned()),
                        ResponseBody(settled_check_suites()),
                    )
                } else {
                    response
                }
            })
            .collect()
    }

    fn with_pending_mergeability(responses: Vec<ScriptedResponse>) -> Vec<ScriptedResponse> {
        responses
            .into_iter()
            .map(|response| {
                if response.target == PULL_DETAIL_TARGET {
                    ScriptedResponse::ok(
                        RequestTarget(PULL_DETAIL_TARGET.to_owned()),
                        ResponseBody(pull_detail_with_pending_mergeability()),
                    )
                } else if response.target == THREADS_TARGET && response.body == convergence() {
                    ScriptedResponse::post(
                        RequestTarget(THREADS_TARGET.to_owned()),
                        ResponseBody(convergence_with_mergeability("UNKNOWN")),
                    )
                } else {
                    response
                }
            })
            .collect()
    }

    #[test]
    fn with_pending_mergeability_rewrites_the_pull_detail_response() {
        let responses = with_pending_mergeability(vec![ScriptedResponse::conditional_ok(
            RequestTarget(PULL_DETAIL_TARGET.to_owned()),
            ResponseBody(pull_detail()),
        )]);

        let [response] = responses.as_slice() else {
            panic!("one scripted response stays one response");
        };
        assert_eq!(response.method, "GET");
        assert_eq!(response.target, PULL_DETAIL_TARGET);
        assert_eq!(response.validator, None);
        assert_eq!(response.status, "200 OK");
        assert_eq!(response.entity_tag, Some(ENTITY_TAG));
        assert_eq!(response.link, None);
        assert_eq!(response.body, pull_detail_with_pending_mergeability());
        assert_eq!(response.delay, Duration::ZERO);
    }

    #[test]
    fn with_pending_mergeability_leaves_another_target_unchanged() {
        let responses = with_pending_mergeability(vec![ScriptedResponse::conditional_ok(
            RequestTarget(REVIEWS_TARGET.to_owned()),
            ResponseBody(reviews()),
        )]);

        let [response] = responses.as_slice() else {
            panic!("one scripted response stays one response");
        };
        assert_eq!(response.method, "GET");
        assert_eq!(response.target, REVIEWS_TARGET);
        assert_eq!(response.validator, Some(ENTITY_TAG));
        assert_eq!(response.status, "200 OK");
        assert_eq!(response.entity_tag, Some(ENTITY_TAG));
        assert_eq!(response.link, None);
        assert_eq!(response.body, reviews());
        assert_eq!(response.delay, Duration::ZERO);
    }

    fn revalidated(responses: Vec<ScriptedResponse>) -> Vec<ScriptedResponse> {
        responses
            .into_iter()
            .map(|response| match response.entity_tag {
                Some(_) => ScriptedResponse::conditional_ok(
                    RequestTarget(response.target),
                    ResponseBody(response.body),
                ),
                None => response,
            })
            .collect()
    }

    #[test]
    fn revalidated_rewrites_a_tagged_response_into_a_conditional_expectation() {
        let revalidated_responses = revalidated(vec![ScriptedResponse::ok(
            RequestTarget(PULLS_TARGET.to_owned()),
            ResponseBody(EMPTY_LIST.to_owned()),
        )]);

        let [response] = revalidated_responses.as_slice() else {
            panic!("one scripted response stays one response");
        };
        assert_eq!(response.method, "GET");
        assert_eq!(response.target, PULLS_TARGET);
        assert_eq!(response.validator, Some(ENTITY_TAG));
        assert_eq!(response.status, "200 OK");
        assert_eq!(response.entity_tag, Some(ENTITY_TAG));
        assert_eq!(response.link, None);
        assert_eq!(response.body, EMPTY_LIST);
        assert_eq!(response.delay, Duration::ZERO);
    }

    #[test]
    fn revalidated_leaves_an_untagged_response_unchanged() {
        let revalidated_responses = revalidated(vec![ScriptedResponse::post(
            RequestTarget(THREADS_TARGET.to_owned()),
            ResponseBody(empty_threads()),
        )]);

        let [response] = revalidated_responses.as_slice() else {
            panic!("one scripted response stays one response");
        };
        assert_eq!(response.method, "POST");
        assert_eq!(response.target, THREADS_TARGET);
        assert_eq!(response.validator, None);
        assert_eq!(response.status, "200 OK");
        assert_eq!(response.entity_tag, None);
        assert_eq!(response.link, None);
        assert_eq!(response.body, empty_threads());
        assert_eq!(response.delay, Duration::ZERO);
    }

    async fn complete_typed_observation() -> RepoWatchObservation {
        let server = ScriptedServer::start(complete_typed_observation_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;
        observation
    }

    async fn observation_with_pull_lifecycle(
        lifecycle: RepoWatchPullRequestLifecycle,
    ) -> RepoWatchObservation {
        let observation = complete_typed_observation().await;
        let state = observation.state();
        let original = &state.pull_requests()[0];
        let pull_request = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: original.context().clone(),
            lifecycle,
            mergeable_state: original.mergeable_state(),
            completed_check_suites: original.completed_check_suites().to_vec(),
            completed_check_runs: original.completed_check_runs().to_vec(),
            reviews: original.reviews().to_vec(),
            threads: original.threads().to_vec(),
            reactions: original.reactions().to_vec(),
        })
        .expect("fixture pull request remains canonical under another lifecycle");
        let rebuilt = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull_request],
            workflow_runs: state.workflow_runs().to_vec(),
            branch_heads: state.branch_heads().to_vec(),
        })
        .expect("fixture repository state remains canonical");
        RepoWatchObservation::new(observation.signal_reviewers().to_vec(), rebuilt)
    }

    /// The observation [`review_only_blocked_assessment`] describes. A first
    /// poll publishes no freshness, so its own candidate lookup finds the head
    /// unsettled and short-circuits before any blocking-review request.
    async fn review_only_blocked_observation() -> RepoWatchObservation {
        let server = ScriptedServer::start(review_only_blocked_observation_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;
        observation
    }

    fn review_only_blocked_assessment() -> RepoWatchConvergenceAssessment {
        RepoWatchConvergenceAssessment::try_new(RepoWatchConvergenceAssessmentInput {
            number: PullRequestNumber::new(
                NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
            ),
            head_sha: CommitSha::try_new(String::from(HEAD_SHA))
                .expect("fixture head is canonical"),
            base_branch: BranchName::try_new(String::from(BASE_BRANCH))
                .expect("fixture base branch is canonical"),
            base_revision: CommitSha::try_new(String::from(BASE_SHA))
                .expect("fixture base revision is canonical"),
            mergeable_state: MergeableState::Mergeable,
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })
        .expect("review decision is the fixture's only convergence blocker")
    }

    #[tokio::test]
    async fn targeted_refresh_reuses_the_repository_poller_and_preserves_untouched_state() {
        let previous = complete_typed_observation().await;
        let server = ScriptedServer::start(complete_pull_request_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        install_late_freshness_survivor(&fixture.poller).await;
        let target = TargetedPullRequest {
            number: PullRequestNumber::new(
                NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
            ),
            expected_head: Some(
                CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head SHA is canonical"),
            ),
        };

        let refreshed = fixture
            .poller
            .poll_targeted_pull_requests_against_cursor(&previous, &[target])
            .await
            .expect("targeted refresh succeeds");

        server.finish().await;
        assert!(
            fixture.poller.fetches.lock().await.is_empty(),
            "targeted refresh drains complete-poll survivors before fetching"
        );
        assert!(
            !fixture
                .poller
                .freshness()
                .contains_key(&CANCELLED_FETCH_PULL_NUMBER),
            "targeted refresh invalidates freshness recorded by a late survivor"
        );
        assert_eq!(
            refreshed,
            TargetedPollOutcome::Observation {
                observation: previous,
                superseded_targets: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn targeted_refresh_without_survivors_preserves_untouched_freshness() {
        const UNTOUCHED_PULL_NUMBER: u64 = 8;
        let previous = complete_typed_observation().await;
        let server = ScriptedServer::start(complete_pull_request_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        fixture.poller.record_fetched_pull_request(
            UNTOUCHED_PULL_NUMBER,
            &listed_pull_request(&minimal_pull_head_sha(UNTOUCHED_PULL_NUMBER)),
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        let target = TargetedPullRequest {
            number: PullRequestNumber::new(
                NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
            ),
            expected_head: Some(
                CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head SHA is canonical"),
            ),
        };

        fixture
            .poller
            .poll_targeted_pull_requests_against_cursor(&previous, &[target])
            .await
            .expect("targeted refresh succeeds");

        server.finish().await;
        assert!(
            fixture
                .poller
                .freshness()
                .contains_key(&UNTOUCHED_PULL_NUMBER),
            "targeted refresh without survivors preserves untouched freshness"
        );
    }

    #[tokio::test]
    async fn targeted_refresh_merge_preserves_accumulated_shadow_state_and_frontiers()
    -> Result<(), Box<dyn Error>> {
        let observation = complete_typed_observation().await;
        let target = observation.state().pull_requests()[0].context().number();
        let unrelated = PullRequestNumber::new(
            NonZeroU64::new(PULL_NUMBER + 1).expect("fixture pull-request number is positive"),
        );
        let unrelated_baseline = merged_baseline_for_number(
            &observation.state().pull_requests()[0],
            unrelated,
            observation.signal_reviewers(),
        );
        let advanced_branch = RepoWatchBranchHead::new(
            observation.state().branch_heads()[0].branch().clone(),
            CommitSha::try_new(String::from(OWED_EVENT_HEAD_SHA))?,
        );
        let shadow_state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: observation.state().pull_requests().to_vec(),
            workflow_runs: observation.state().workflow_runs().to_vec(),
            branch_heads: vec![advanced_branch.clone()],
        })?;
        let shadow_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                [1; 32],
                NonZeroU64::new(2).expect("fixture sequence is positive"),
                target,
            ),
            RepoWatchEventIdentityFrontierEntryV1::new([2; 32], NonZeroU64::MIN),
        ])?;
        let merged = observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Merged).await;
        let compacted = compact_cursor_observation(&merged, None, &[])
            .expect("fixture merged observation compacts");
        let candidate_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                [1; 32],
                NonZeroU64::new(3).expect("fixture sequence is positive"),
                target,
            ),
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                [3; 32],
                NonZeroU64::MIN,
                target,
            ),
        ])?;
        let candidate =
            RepoWatchCursorCandidate::try_with_event_identity_frontier_and_merged_baselines(
                compacted.observation,
                candidate_frontier,
                compacted.merged_pull_request_baselines,
            )?;
        let expected_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                [1; 32],
                NonZeroU64::new(3).expect("fixture sequence is positive"),
                target,
            ),
            RepoWatchEventIdentityFrontierEntryV1::new([2; 32], NonZeroU64::MIN),
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                [3; 32],
                NonZeroU64::MIN,
                target,
            ),
        ])?;

        let refreshed = merge_targeted_refresh_into_webhook_shadow(
            WebhookShadowBaseline {
                observation: RepoWatchObservation::new(
                    observation.signal_reviewers().to_vec(),
                    shadow_state,
                ),
                identity_frontier: shadow_frontier,
                merged_pull_request_baselines: vec![unrelated_baseline],
            },
            &candidate,
            &[target],
        )
        .expect("targeted refresh merges into the accumulated shadow");

        assert!(refreshed.observation.state().pull_requests().is_empty());
        assert_eq!(
            refreshed.observation.state().branch_heads(),
            [advanced_branch]
        );
        assert_eq!(refreshed.identity_frontier, expected_frontier);
        assert_eq!(refreshed.merged_pull_request_baselines.len(), 2);
        assert_eq!(refreshed.merged_pull_request_baselines[0].number(), target);
        assert_eq!(
            refreshed.merged_pull_request_baselines[1].number(),
            unrelated
        );
        Ok(())
    }

    struct FreshnessOnDrop {
        poller: Arc<GitHubRepositoryPoller>,
    }

    impl Drop for FreshnessOnDrop {
        fn drop(&mut self) {
            self.poller.record_fetched_pull_request(
                CANCELLED_FETCH_PULL_NUMBER,
                &listed_pull_request(&minimal_pull_head_sha(CANCELLED_FETCH_PULL_NUMBER)),
                PullRequestSettlement::Settled,
                Vec::new(),
            );
        }
    }

    async fn install_late_freshness_survivor(poller: &Arc<GitHubRepositoryPoller>) {
        let (started, ready) = tokio::sync::oneshot::channel();
        let survivor_poller = Arc::clone(poller);
        poller.fetches.lock().await.spawn(async move {
            let _freshness_on_cancellation = FreshnessOnDrop {
                poller: survivor_poller,
            };
            started
                .send(())
                .expect("targeted-refresh fixture still waits for its survivor");
            std::future::pending::<Result<super::FetchedPullRequest, RepositoryWatchAttemptError>>()
                .await
        });
        ready
            .await
            .expect("the cancelled-fetch survivor starts before targeted refresh");
    }

    #[tokio::test]
    async fn targeted_refresh_reports_a_moved_head_as_superseded() {
        let previous = complete_typed_observation().await;
        let server = ScriptedServer::start(complete_pull_request_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let target = TargetedPullRequest {
            number: PullRequestNumber::new(
                NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
            ),
            expected_head: Some(
                CommitSha::try_new(String::from("beefbeefbeefbeefbeefbeefbeefbeefbeefbeef"))
                    .expect("fixture head SHA is canonical"),
            ),
        };

        let refreshed = fixture
            .poller
            .poll_targeted_pull_requests_against_cursor(&previous, &[target])
            .await
            .expect("targeted refresh succeeds");

        server.finish().await;
        assert_eq!(refreshed, TargetedPollOutcome::SupersededTarget);
    }

    #[tokio::test]
    async fn targeted_query_coalescing_retains_the_stricter_head_guard() {
        let previous = complete_typed_observation().await;
        let pull_request = PullRequestNumber::new(
            NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
        );
        let expected_head =
            CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head SHA is canonical");
        let refreshes = [
            RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request },
            RepoWatchTargetedRefreshV1::Mergeability {
                pull_request,
                expected_head: expected_head.clone(),
            },
        ];

        let targets = targeted_pull_requests(&previous, &[], &refreshes)
            .expect("compatible targeted queries coalesce");

        assert_eq!(
            targets,
            vec![TargetedPullRequest {
                number: pull_request,
                expected_head: Some(expected_head),
            }]
        );
    }

    #[tokio::test]
    async fn conflicting_targeted_head_guards_fail_closed() {
        let previous = complete_typed_observation().await;
        let pull_request = PullRequestNumber::new(
            NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
        );
        let first_head =
            CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head SHA is canonical");
        let second_head = CommitSha::try_new(CHANGED_LISTED_HEAD_SHA.to_owned())
            .expect("changed fixture head SHA is canonical");
        let refreshes = [
            RepoWatchTargetedRefreshV1::Mergeability {
                pull_request,
                expected_head: first_head,
            },
            RepoWatchTargetedRefreshV1::CheckRollup {
                pull_request,
                expected_head: second_head,
            },
        ];

        let result = targeted_pull_requests(&previous, &[], &refreshes);

        assert_eq!(result, Err(RepositoryWatchAttemptError::Normalization));
    }

    #[tokio::test]
    async fn a_compact_merged_baseline_is_a_target_for_its_commit_rollup() {
        let merged = observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Merged).await;
        let compacted = compact_cursor_observation(&merged, None, &[])
            .expect("fixture observation compacts canonically");
        let baseline = &compacted.merged_pull_request_baselines[0];
        let refreshes = [RepoWatchTargetedRefreshV1::CheckRollupForCommit {
            head: baseline.head_sha().clone(),
        }];

        let targets = targeted_pull_requests(
            &compacted.observation,
            &compacted.merged_pull_request_baselines,
            &refreshes,
        )
        .expect("a compact subject remains targetable");

        assert_eq!(
            targets,
            vec![TargetedPullRequest {
                number: baseline.number(),
                expected_head: Some(baseline.head_sha().clone()),
            }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_webhook_drain_schedules_an_independent_retry() {
        let scheduled_at = Instant::now();
        let mut retry = WebhookDrainRetry::default();

        retry.update_after(&Err(RepositoryWatchAttemptError::Persistence));

        assert_eq!(
            retry.deadline,
            Some(scheduled_at + WEBHOOK_DRAIN_RETRY_DELAY)
        );
        tokio::time::advance(WEBHOOK_DRAIN_RETRY_DELAY).await;
        retry.due().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_consecutive_drain_failure_doubles_the_retry_delay() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());

        retry.update_after(&drain_failure());

        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_DELAY * 2);
    }

    /// The seventh consecutive failure is the first whose doubling reaches the
    /// ceiling, and the eighth stays there rather than growing past it.
    #[tokio::test(start_paused = true)]
    async fn consecutive_drain_failures_stop_doubling_at_the_ceiling() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_DELAY * 32);

        retry.update_after(&drain_failure());
        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_MAX_DELAY);

        retry.update_after(&drain_failure());

        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_MAX_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_drained_webhook_page_clears_the_retry_backoff() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());

        retry.update_after(&Ok(()));

        assert_eq!(retry.deadline, None);
        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_DELAY);
    }

    /// A failure that is not the drain's must not push an already-earned retry
    /// further out: with a poll interval shorter than the current delay, every
    /// failure would reschedule the deadline and the retry would never come
    /// due.
    #[tokio::test(start_paused = true)]
    async fn a_failure_outside_the_retry_never_defers_a_retry_that_is_already_owed() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        let owed = retry.deadline;

        retry.arm_if_unowed(Instant::now());

        assert_eq!(retry.deadline, owed);
    }

    /// A poll that began before an owed retry and outlived it did not take that
    /// retry, so its own failure must leave the deadline expired for the next
    /// pass. Moving it forward would let a poll interval shorter than the delay
    /// starve the drain on every slow failure.
    #[tokio::test(start_paused = true)]
    async fn a_failure_outside_the_retry_preserves_a_deadline_it_outlived() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        let owed = retry.deadline;
        tokio::time::advance(WEBHOOK_DRAIN_RETRY_DELAY * 2).await;

        retry.arm_if_unowed(Instant::now());

        assert_eq!(retry.deadline, owed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_outside_the_retry_arms_one_when_none_is_owed() {
        let armed_at = Instant::now();
        let mut retry = WebhookDrainRetry::default();

        retry.arm_if_unowed(armed_at);

        assert_eq!(retry.deadline, Some(armed_at + WEBHOOK_DRAIN_RETRY_DELAY));
    }

    /// The dispatch work that failed runs only from a later attempt, and the
    /// delivery that would have woken one is already terminal, so the drain
    /// succeeding must still leave a retry owed — at the base delay, because
    /// the drain is not what failed.
    #[tokio::test(start_paused = true)]
    async fn a_drain_whose_dispatch_failed_arms_a_follow_up_at_the_base_delay() {
        let armed_at = Instant::now();
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_DELAY * 2);

        retry.arm_follow_up(armed_at);

        assert_eq!(retry.deadline, Some(armed_at + WEBHOOK_DRAIN_RETRY_DELAY));
        assert_eq!(retry.delay(), WEBHOOK_DRAIN_RETRY_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dispatch_follow_up_keeps_poll_drains_enabled() {
        let mut retry = WebhookDrainRetry::default();
        retry.arm_follow_up(Instant::now());

        let drain = retry.poll_drain();

        assert_eq!(drain, WebhookDrain::Run);
    }

    #[test]
    fn only_projection_timeouts_block_a_complete_poll() {
        assert!(
            WebhookDrainOutcome::ProjectionFailed(
                RepositoryWatchAttemptError::WebhookDrainTimedOut
            )
            .blocks_complete_poll_after_timeout()
        );
        assert!(
            !WebhookDrainOutcome::DispatchFailedAfterTerminal(
                RepositoryWatchAttemptError::WebhookDrainTimedOut
            )
            .blocks_complete_poll_after_timeout()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dispatch_follow_up_does_not_suppress_an_admission_wake() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let mut retry = WebhookDrainRetry::default();
        retry.arm_follow_up(Instant::now());
        admissions
            .send(())
            .expect("the fixture task still holds the wake receiver");

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            Instant::now() + POLL_INTERVAL,
            &retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::WebhookWork);
    }

    #[tokio::test(start_paused = true)]
    async fn a_trailing_poll_dispatch_failure_arms_a_follow_up() {
        let armed_at = Instant::now();
        let mut retry = WebhookDrainRetry::default();

        retry.update_after_poll_drain(
            WebhookDrainOutcome::Drained,
            Some(RepositoryWatchAttemptError::Dispatch),
            armed_at,
        );

        assert_eq!(retry.deadline, Some(armed_at + WEBHOOK_DRAIN_RETRY_DELAY));
        assert_eq!(retry.poll_drain(), WebhookDrain::Run);
    }

    /// The work a follow-up owes runs before the drain, so failing it again is
    /// the same trailing failure rather than a projection one. Rearming as
    /// backoff would start suppressing admission wakes and poll drains on the
    /// strength of a failure that never reached projection.
    #[tokio::test(start_paused = true)]
    async fn a_follow_up_that_failed_again_rearms_as_a_follow_up() {
        let mut retry = WebhookDrainRetry::default();
        retry.arm_follow_up(Instant::now());
        retry.consume();
        let rearmed_at = Instant::now();

        retry.arm_if_unowed(rearmed_at);

        assert_eq!(retry.deadline, Some(rearmed_at + WEBHOOK_DRAIN_RETRY_DELAY));
        assert!(!retry.is_backing_off());
        assert_eq!(retry.poll_drain(), WebhookDrain::Run);
    }

    /// The other side of the same rearm: a spent backoff whose attempt failed
    /// before reaching the drain is still owed that drain, so the poll must go
    /// on leaving it to the retry.
    #[tokio::test(start_paused = true)]
    async fn a_backoff_that_failed_before_its_drain_rearms_as_backoff() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.consume();

        retry.arm_if_unowed(Instant::now());

        assert!(retry.is_backing_off());
        assert_eq!(retry.poll_drain(), WebhookDrain::Deferred);
    }

    /// The retry arm precedes the poll arm, so a deadline left owed once its
    /// retry has been taken selects the same attempt again immediately.
    #[tokio::test(start_paused = true)]
    async fn a_taken_retry_spends_its_deadline_and_keeps_its_delay() {
        let mut retry = WebhookDrainRetry::default();
        retry.update_after(&drain_failure());
        retry.update_after(&drain_failure());
        let earned = retry.delay();

        retry.consume();

        assert_eq!(retry.deadline, None);
        assert_eq!(retry.delay(), earned);
    }

    #[tokio::test(start_paused = true)]
    async fn an_overdue_webhook_retry_precedes_an_overdue_poll() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (_admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let mut webhook_retry = WebhookDrainRetry::default();
        webhook_retry.update_after(&Err(RepositoryWatchAttemptError::Persistence));
        tokio::time::advance(WEBHOOK_DRAIN_RETRY_DELAY).await;
        let overdue_poll = Instant::now() - Duration::from_secs(1);

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            overdue_poll,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::WebhookRetry);
    }

    #[tokio::test(start_paused = true)]
    async fn an_overdue_poll_runs_when_no_webhook_retry_is_due() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (_admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let webhook_retry = WebhookDrainRetry::default();
        let overdue_poll = Instant::now() - Duration::from_secs(1);

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            overdue_poll,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::Poll);
    }

    #[tokio::test(start_paused = true)]
    async fn an_admitted_wake_reaches_the_task_before_its_next_poll() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let webhook_retry = WebhookDrainRetry::default();
        let next_poll = Instant::now() + POLL_INTERVAL;
        admissions
            .send(())
            .expect("the fixture task still holds the wake receiver");

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            next_poll,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::WebhookWork);
    }

    /// An authenticated sender can replay an admitted delivery at the intake
    /// rate, so a wake must not start a drain the backoff has deferred.
    #[tokio::test(start_paused = true)]
    async fn an_admission_wake_defers_to_an_owed_retry() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let mut webhook_retry = WebhookDrainRetry::default();
        webhook_retry.update_after(&drain_failure());
        admissions
            .send(())
            .expect("the fixture task still holds the wake receiver");

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            Instant::now() + POLL_INTERVAL,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::WebhookRetry);
    }

    /// The wake coalesces, so one deferred by the backoff is still there for
    /// the attempt that follows the retry rather than being lost.
    #[tokio::test(start_paused = true)]
    async fn a_deferred_admission_wake_survives_the_backoff() {
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let (admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let mut webhook_retry = WebhookDrainRetry::default();
        webhook_retry.update_after(&drain_failure());
        admissions
            .send(())
            .expect("the fixture task still holds the wake receiver");
        webhook_retry.update_after(&Ok(()));

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            Instant::now() + POLL_INTERVAL,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::WebhookWork);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_precedes_every_overdue_repository_deadline() {
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let (_admissions, admitted) = watch::channel(());
        let mut webhook_work = Some(admitted);
        let mut webhook_retry = WebhookDrainRetry::default();
        webhook_retry.update_after(&Err(RepositoryWatchAttemptError::Persistence));
        tokio::time::advance(WEBHOOK_DRAIN_RETRY_DELAY).await;
        let overdue_poll = Instant::now() - Duration::from_secs(1);
        shutdown
            .send(true)
            .expect("the fixture task still holds the shutdown receiver");

        let wake = next_repository_wake(
            &mut shutdown_receiver,
            overdue_poll,
            &webhook_retry,
            &mut webhook_work,
        )
        .await;

        assert_eq!(wake, RepositoryWatchWake::Stop);
    }

    /// The supervisor joins every child before aborting the set, so a drain
    /// inspection that never returns — an unresponsive database, a connection
    /// that never becomes available — has to be cancellable or the daemon
    /// cannot terminate.
    #[tokio::test]
    async fn shutdown_cancels_supervised_work_that_never_returns() {
        // The sender stays with the fixture throughout: a dropped sender also
        // ends the wait, so a test that let it go could pass without ever
        // observing shutdown itself.
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        shutdown
            .send(true)
            .expect("the fixture still holds the shutdown receiver");

        let outcome =
            run_until_shutdown(&mut shutdown_receiver, std::future::pending::<()>()).await;

        assert_eq!(outcome, None);
        drop(shutdown);
    }

    #[tokio::test]
    async fn a_notification_that_is_not_shutdown_resumes_supervised_work() {
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        const COMPLETION: u8 = 7;
        let (completion, completed) = tokio::sync::oneshot::channel::<u8>();
        // Pending before the wait begins, so the notification is delivered
        // while the work is still outstanding.
        shutdown
            .send(false)
            .expect("the fixture still holds the shutdown receiver");
        let signal = tokio::spawn(async move {
            tokio::task::yield_now().await;
            completion
                .send(COMPLETION)
                .expect("the fixture still awaits the supervised work");
        });

        let outcome = run_until_shutdown(&mut shutdown_receiver, async move {
            completed.await.expect("the fixture completes the work")
        })
        .await;

        signal.await.expect("the fixture signal completes");
        assert_eq!(outcome, Some(COMPLETION));
        drop(shutdown);
    }

    #[tokio::test]
    async fn a_webhook_wake_preempts_an_in_flight_complete_poll() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        let webhook_retry = WebhookDrainRetry::default();
        webhook_sender.send_replace(());

        let outcome = await_poll_or_interrupt(
            std::future::pending::<()>(),
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Webhook));
    }

    #[tokio::test]
    async fn each_fresh_poll_can_be_preempted_while_the_drain_rearms() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        let webhook_retry = WebhookDrainRetry::default();
        webhook_sender.send_replace(());

        let first = await_poll_or_interrupt(
            std::future::pending::<()>(),
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;
        webhook_sender.send_replace(());
        let second = await_poll_or_interrupt(
            std::future::pending::<()>(),
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;

        assert!(matches!(first, PollAttemptWait::Webhook));
        assert!(matches!(second, PollAttemptWait::Webhook));
    }

    #[tokio::test]
    async fn a_complete_poll_wins_without_an_interrupt() {
        let (_webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        let webhook_retry = WebhookDrainRetry::default();
        const COMPLETION: u8 = 7;

        let outcome = await_poll_or_interrupt(
            async { COMPLETION },
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Completed(COMPLETION)));
    }

    #[tokio::test]
    async fn a_suppressed_admission_wake_does_not_preempt_a_complete_poll() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        let webhook_retry = WebhookDrainRetry::default();
        const COMPLETION: u8 = 11;
        webhook_sender.send_replace(());

        let outcome = await_poll_or_interrupt(
            async { COMPLETION },
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Suppressed,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Completed(COMPLETION)));
    }

    #[tokio::test(start_paused = true)]
    async fn an_owed_retry_preempts_a_wedged_complete_poll_when_its_deadline_arrives() {
        let (_webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        let mut webhook_retry = WebhookDrainRetry::default();
        webhook_retry.update_after(&drain_failure());

        let outcome = await_poll_or_interrupt(
            std::future::pending::<()>(),
            &mut shutdown,
            &webhook_retry,
            &mut webhook_work,
            WebhookPollInterrupt::Suppressed,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::WebhookRetry));
    }

    #[test]
    fn a_pending_webhook_wake_is_observed_before_its_drain() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        webhook_sender.send_replace(());

        observe_webhook_work_before_drain(&mut webhook_work);

        assert!(
            !webhook_work
                .as_ref()
                .expect("the fixture keeps its webhook sender")
                .has_changed()
                .expect("the fixture keeps its webhook sender")
        );
    }

    #[test]
    fn a_webhook_wake_after_the_observation_remains_pending() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        webhook_sender.send_replace(());
        observe_webhook_work_before_drain(&mut webhook_work);

        webhook_sender.send_replace(());

        assert!(
            webhook_work
                .as_ref()
                .expect("the fixture keeps its webhook sender")
                .has_changed()
                .expect("the fixture keeps its webhook sender")
        );
    }

    /// Primary mode's whole point: an authenticated delivery writes the durable
    /// cursor itself, and the ordinary event rows it produces are attributed to
    /// the transport that observed them rather than to a poll that never ran.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_primary_webhook_delivery_advances_the_durable_cursor() -> Result<(), Box<dyn Error>>
    {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_primary = true;
        let repository = fixture.task.repository.clone();
        let before = fixture
            .task
            .store
            .load_cursor(&repository)
            .await?
            .expect("the fixture commits a baseline cursor")
            .generation();

        let attempt = fixture.task.process_webhook_deliveries().await;

        assert_eq!(attempt, WebhookDrainOutcome::Drained);
        let disposition = webhook_store
            .load_disposition(admission.key())
            .await?
            .expect("a primary delivery reaches a terminal disposition");
        assert_eq!(
            disposition.disposition(),
            RepoWatchWebhookDisposition::Committed
        );
        let after = fixture
            .task
            .store
            .load_cursor(&repository)
            .await?
            .expect("the committed cursor is readable")
            .generation();
        assert!(
            after > before,
            "a primary delivery advances the durable cursor"
        );
        // The fixture's baseline commit carries no events, so this page holds
        // exactly what the delivery wrote.
        let page = fixture
            .task
            .store
            .load_event_page(&repository, None, event_page_size())
            .await?;
        let recorded = page
            .events()
            .iter()
            .map(|event| (event.event().kind().name(), event.producer()))
            .collect::<Vec<_>>();

        assert_eq!(
            recorded,
            vec![(
                RepoWatchEventKindNameV1::ReviewSubmitted,
                RepoWatchEventProducer::Webhook
            )]
        );
        // The committed row is the durable record, so a parity projection of
        // the same occurrence would stand permanently unmatched.
        assert_eq!(
            webhook_store
                .recorded_event_projection_count(admission.key())
                .await?,
            0
        );
        Ok(())
    }

    /// Shadow mode is unchanged by primary mode's arrival: no ordinary event row
    /// and no cursor advance from a payload-derived patch.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_shadow_webhook_delivery_leaves_the_durable_cursor_alone()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let mut fixture = webhook_task(&pool).await?;
        let repository = fixture.task.repository.clone();
        let before = fixture
            .task
            .store
            .load_cursor(&repository)
            .await?
            .expect("the fixture commits a baseline cursor")
            .generation();

        let attempt = fixture.task.process_webhook_deliveries().await;

        assert_eq!(attempt, WebhookDrainOutcome::Drained);
        let after = fixture
            .task
            .store
            .load_cursor(&repository)
            .await?
            .expect("the committed cursor is readable")
            .generation();
        assert_eq!(after, before);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn one_projection_error_does_not_halt_its_page() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let first = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        let second = submitted_review_admission(SECOND_WEBHOOK_DELIVERY, SECOND_WEBHOOK_REVIEW)?;
        webhook_store.admit(&first).await?;
        webhook_store.admit(&second).await?;
        webhook_store
            .inject_projection_rejection(first.key())
            .await?;
        let mut fixture = webhook_task(&pool).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Persistence)
        );
        assert!(!webhook_disposition_exists(&webhook_store, first.key()).await?);
        let peer = webhook_store
            .load_disposition(second.key())
            .await?
            .expect("the page peer reached a terminal disposition");
        assert_eq!(peer.disposition(), RepoWatchWebhookDisposition::Projected);
        assert_eq!(peer.outcome_code(), None);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn startup_preparation_drains_pending_webhook_work_before_runtime_admission()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admitted = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admitted).await?;
        let (_sender, receiver) = watch::channel(());
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_work = Some(receiver);

        let must_stop = fixture.task.prepare_startup().await;

        assert!(!must_stop);
        assert!(webhook_disposition_exists(&webhook_store, admitted.key()).await?);
        assert!(fixture.task.startup_webhook_retry.is_some());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_retry_drains_the_delivery_a_projection_error_retained() -> Result<(), Box<dyn Error>>
    {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let retained = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&retained).await?;
        let rejection = webhook_store
            .inject_projection_rejection(retained.key())
            .await?;
        let mut fixture = webhook_task(&pool).await?;
        let failed = fixture.task.process_webhook_deliveries().await;
        assert_eq!(
            failed,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Persistence)
        );
        rejection.restore().await?;

        let retried = fixture.task.process_webhook_deliveries().await;

        assert_eq!(retried, WebhookDrainOutcome::Drained);
        assert!(webhook_disposition_exists(&webhook_store, retained.key()).await?);
        Ok(())
    }

    #[test]
    fn scripted_request_waits_for_its_declared_body() {
        const HEADERS: &[u8] = b"POST /graphql HTTP/1.1\r\nContent-Length: 4\r\n\r\n";
        const COMPLETE: &[u8] = b"POST /graphql HTTP/1.1\r\nContent-Length: 4\r\n\r\ntest";

        assert!(!scripted_request_is_complete(HEADERS));
        assert!(scripted_request_is_complete(COMPLETE));
    }

    #[test]
    fn scripted_request_without_a_body_completes_at_headers() {
        const REQUEST: &[u8] = b"GET /resource HTTP/1.1\r\nHost: localhost\r\n\r\n";

        assert!(scripted_request_is_complete(REQUEST));
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_progressing_drain_yields_before_its_outer_deadline_and_rearms()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let first = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        let second = submitted_review_admission(SECOND_WEBHOOK_DELIVERY, SECOND_WEBHOOK_REVIEW)?;
        webhook_store.admit(&first).await?;
        webhook_store.admit(&second).await?;
        let (sender, receiver) = watch::channel(());
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_nudge = Some(Arc::new(sender));

        let outcome = fixture
            .task
            .process_webhook_deliveries_with_budget(Some(Duration::ZERO))
            .await;

        assert_eq!(outcome, WebhookDrainOutcome::Drained);
        assert!(webhook_disposition_exists(&webhook_store, first.key()).await?);
        assert!(!webhook_disposition_exists(&webhook_store, second.key()).await?);
        assert!(receiver.has_changed()?);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_backlogged_drain_yields_after_one_page_and_rearms_its_wake()
    -> Result<(), Box<dyn Error>> {
        const BURST_SIZE: u16 = WEBHOOK_PENDING_PAGE_SIZE.get() + 1;
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        admit_submitted_review_burst(&webhook_store, BURST_SIZE).await?;
        let (sender, receiver) = watch::channel(());
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_nudge = Some(Arc::new(sender));

        let outcome = fixture.task.process_webhook_deliveries().await;

        let disposition_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_disposition")
                .fetch_one(&pool)
                .await?;
        assert_eq!(outcome, WebhookDrainOutcome::Drained);
        assert_eq!(
            disposition_count,
            i64::from(WEBHOOK_PENDING_PAGE_SIZE.get())
        );
        assert!(receiver.has_changed()?);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn admission_during_a_concurrent_drain_rearms_the_next_bounded_page()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let first = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        let second = submitted_review_admission(SECOND_WEBHOOK_DELIVERY, SECOND_WEBHOOK_REVIEW)?;
        webhook_store.admit(&first).await?;
        webhook_store
            .inject_projection_wedge(first.key(), WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .await?;
        let mut blocker = pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .execute(&mut *blocker)
            .await?;
        let (sender, receiver) = watch::channel(());
        let _nudge_keepalive = sender.clone();
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_nudge = Some(Arc::new(sender));
        let mut task = fixture.task;
        let drain = tokio::spawn(async move { task.process_webhook_deliveries().await });
        wait_for_webhook_projection_wedge(&webhook_store).await;

        webhook_store.admit(&second).await?;
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .fetch_one(&mut *blocker)
            .await?;
        assert_eq!(drain.await?, WebhookDrainOutcome::Drained);

        assert!(unlocked, "the fixture releases its deliberate drain wedge");
        assert!(webhook_disposition_exists(&webhook_store, first.key()).await?);
        assert!(!webhook_disposition_exists(&webhook_store, second.key()).await?);
        assert!(receiver.has_changed()?);

        let mut continuation = webhook_task(&pool).await?.task;
        let continued = continuation.process_webhook_deliveries().await;

        assert_eq!(continued, WebhookDrainOutcome::Drained);
        assert!(webhook_disposition_exists(&webhook_store, second.key()).await?);
        Ok(())
    }

    /// deadline cancellation preserves durable webhook work for retry.
    #[tokio::test(start_paused = true)]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_webhook_drain_deadline_cancels_and_retries_durable_work()
    -> Result<(), Box<dyn Error>> {
        // A paused clock auto-advances whenever the runtime goes idle, and
        // container startup spends nearly all of its time waiting on the
        // container daemon. Without this the daemon client's own request
        // deadline expires in virtual time before any of the work below runs,
        // so keep the clock runnable across setup and not just the wedge.
        let clock_guard = keep_paused_clock_runnable();
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        webhook_store
            .inject_projection_wedge(admission.key(), WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .await?;
        let mut blocker = pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .execute(&mut *blocker)
            .await?;
        let mut fixture = webhook_task(&pool).await?;
        {
            // The guard above still holds, so Tokio cannot auto-advance the
            // production deadline before the database operation reaches the
            // injected wedge.
            let drain = fixture.task.process_webhook_deliveries_with_timeout();
            tokio::pin!(drain);
            tokio::select! {
                () = wait_for_webhook_projection_wedge(&webhook_store) => {}
                outcome = &mut drain => {
                    panic!("the deliberate projection wedge completed early: {outcome:?}");
                }
            }

            clock_guard.abort();
            clock_guard.await.ok();
            tokio::time::advance(WEBHOOK_DRAIN_ATTEMPT_TIMEOUT).await;
            assert_eq!(
                drain.await,
                WebhookDrainOutcome::ProjectionFailed(
                    RepositoryWatchAttemptError::WebhookDrainTimedOut
                )
            );
        }
        // The deadline has fired, so virtual time is free to jump again. The
        // durable work below still talks to PostgreSQL, and its connection
        // pool's own acquire deadline would expire instantly in that jumped
        // time, so keep the clock runnable for the retry as well.
        let clock_guard = keep_paused_clock_runnable();
        assert_eq!(
            fixture.task.webhook_terminal_ambiguous,
            Some(admission.key()),
            "deadline cancellation retains the exact unsettled terminal write"
        );
        let ambiguous = fixture.task.webhook_terminal_ambiguous.take();
        let mut deferred_drain = None;
        let mut deferred_dispatch_failure = None;
        assert_eq!(
            fixture
                .task
                .run_attempt_prelude(
                    WebhookDrain::Deferred,
                    &mut deferred_drain,
                    &mut deferred_dispatch_failure,
                )
                .await,
            Err(RepositoryWatchAttemptError::WebhookDrainTimedOut),
            "the general timeout fence blocks deferred cursor-advancing polls even before delivery-specific state is installed"
        );
        fixture.task.webhook_terminal_ambiguous = ambiguous;
        assert!(!webhook_disposition_exists(&webhook_store, admission.key()).await?);
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .fetch_one(&mut *blocker)
            .await?;

        let retried = fixture.task.process_webhook_deliveries().await;

        assert!(unlocked, "the fixture releases its deliberate drain wedge");
        assert_eq!(retried, WebhookDrainOutcome::Drained);
        assert!(webhook_disposition_exists(&webhook_store, admission.key()).await?);
        assert_eq!(
            fixture.task.webhook_terminal_ambiguous, None,
            "settling that exact delivery releases complete polling"
        );
        clock_guard.abort();
        clock_guard.await.ok();
        Ok(())
    }

    /// Only a cancelled drain has earned the growing projection backoff, so an
    /// attempt deadline reached outside the drain reports the step it
    /// interrupted rather than a drain failure.
    #[test]
    fn a_cancelled_attempt_reports_the_step_the_deadline_interrupted() {
        let error = RepositoryWatchAttemptError::WebhookAttemptTimedOut;

        assert_eq!(
            WebhookAttemptPhase::BeforeDrain.cancelled_outcome(error, false),
            WebhookAttemptOutcome::FailedBeforeDrain(error)
        );
        assert_eq!(
            WebhookAttemptPhase::Drain.cancelled_outcome(error, false),
            WebhookAttemptOutcome::DrainFailed(error)
        );
        assert_eq!(
            WebhookAttemptPhase::AfterDrain.cancelled_outcome(error, false),
            WebhookAttemptOutcome::DrainedThenFailed(error)
        );
    }

    /// The drain's own post-terminal dispatch window sits inside the drain
    /// phase, and its delivery is already terminal. Reporting a cancellation
    /// there as a drain failure would grow the projection backoff — suppressing
    /// admission wakes and poll drains to its cap — for a projection that had
    /// already committed.
    #[test]
    fn a_cancelled_post_terminal_dispatch_arms_the_follow_up_instead_of_the_backoff() {
        let error = RepositoryWatchAttemptError::WebhookAttemptTimedOut;

        assert_eq!(
            WebhookAttemptPhase::Drain.cancelled_outcome(error, true),
            WebhookAttemptOutcome::DrainedThenFailed(error)
        );
    }

    /// Only the drain owns pending deliveries, so only a cancellation there can
    /// leave one unprojected behind a cursor a later poll would advance.
    #[test]
    fn only_a_cancelled_drain_fences_the_next_cursor_advancing_poll() {
        assert!(!WebhookAttemptPhase::BeforeDrain.cancellation_fences_complete_poll());
        assert!(WebhookAttemptPhase::Drain.cancellation_fences_complete_poll());
        assert!(!WebhookAttemptPhase::AfterDrain.cancellation_fences_complete_poll());
    }

    #[test]
    fn webhook_deadlines_scale_with_the_durable_cursor_payload_and_remain_bounded() {
        let ordinary = WebhookAttemptDeadlines::for_cursor_payload(
            WEBHOOK_DRAIN_TIMEOUT_PAYLOAD_QUANTUM_BYTES,
        );
        let larger = WebhookAttemptDeadlines::for_cursor_payload(
            WEBHOOK_DRAIN_TIMEOUT_PAYLOAD_QUANTUM_BYTES * 3,
        );
        let maximum = WebhookAttemptDeadlines::for_cursor_payload(u64::MAX);

        assert_eq!(ordinary.drain, WEBHOOK_DRAIN_ATTEMPT_TIMEOUT);
        assert_eq!(
            larger.drain,
            WEBHOOK_DRAIN_TIMEOUT_PER_PAYLOAD_QUANTUM.saturating_mul(3)
        );
        assert_eq!(maximum.drain, WEBHOOK_DRAIN_MAX_ATTEMPT_TIMEOUT);
        assert_eq!(ordinary.stall_threshold(), ordinary.drain);
        assert_eq!(larger.stall_threshold(), larger.drain);
        assert_eq!(maximum.stall_threshold(), maximum.drain);
        assert!(ordinary.attempt > ordinary.drain);
        assert!(larger.attempt > larger.drain);
        assert!(maximum.attempt > maximum.drain);
    }

    /// The read that derives the deadlines cannot be covered by them, so its
    /// own bound has to expire sooner than the shortest attempt it could have
    /// produced. Otherwise a stalled sizing read would hold the serialized
    /// repository task longer than the attempt it was sizing ever could.
    #[test]
    fn cursor_sizing_is_bounded_below_the_shortest_deadline_it_derives() {
        let floor = WebhookAttemptDeadlines::for_cursor_payload(0);

        assert!(WEBHOOK_CURSOR_SIZING_TIMEOUT < floor.drain);
        assert!(WEBHOOK_CURSOR_SIZING_TIMEOUT < floor.attempt);
    }

    /// The attempt deadline is the drain deadline plus a margin the leading
    /// reconciliation spends, so any leading work that outlasts that margin
    /// makes the outer deadline — not the drain's own — cancel an in-flight
    /// drain. That cancellation leaves the same pending delivery behind, so it
    /// owes the same fence: without it the next complete poll advances the
    /// durable cursor past a delivery nothing has projected.
    #[tokio::test(start_paused = true)]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn an_attempt_deadline_reached_inside_the_drain_fences_the_next_complete_poll()
    -> Result<(), Box<dyn Error>> {
        // A paused clock auto-advances whenever the runtime goes idle, and
        // container startup spends nearly all of its time waiting on the
        // container daemon, so keep the clock runnable across setup.
        let clock_guard = keep_paused_clock_runnable();
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        webhook_store
            .inject_projection_wedge(admission.key(), WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .await?;
        let mut blocker = pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .execute(&mut *blocker)
            .await?;
        let mut fixture = webhook_task(&pool).await?;
        {
            // The drain deadline is deliberately far beyond the attempt's, so
            // the cancellation observed below can only be the outer one.
            let attempt =
                fixture
                    .task
                    .run_webhook_attempt_with_deadlines(WebhookAttemptDeadlines {
                        drain: Duration::from_secs(600),
                        attempt: Duration::from_secs(60),
                        cursor_payload_bytes: 0,
                    });
            tokio::pin!(attempt);
            tokio::select! {
                () = wait_for_webhook_projection_wedge(&webhook_store) => {}
                outcome = &mut attempt => {
                    panic!("the deliberate projection wedge completed early: {outcome:?}");
                }
            }

            clock_guard.abort();
            clock_guard.await.ok();
            tokio::time::advance(Duration::from_secs(60)).await;
            assert_eq!(
                attempt.await,
                WebhookAttemptOutcome::DrainFailed(
                    RepositoryWatchAttemptError::WebhookAttemptTimedOut
                ),
                "the drain was what the outer deadline interrupted"
            );
        }
        let clock_guard = keep_paused_clock_runnable();
        assert!(
            fixture.task.webhook_drain_timed_out,
            "an outer deadline reached inside the drain records the same timeout the drain's own deadline does"
        );
        // Taken and restored so the assertion below isolates the general
        // timeout fence from the delivery-specific ambiguity fence beside it.
        let ambiguous = fixture.task.webhook_terminal_ambiguous.take();
        let mut deferred_drain = None;
        let mut deferred_dispatch_failure = None;
        assert_eq!(
            fixture
                .task
                .run_attempt_prelude(
                    WebhookDrain::Deferred,
                    &mut deferred_drain,
                    &mut deferred_dispatch_failure,
                )
                .await,
            Err(RepositoryWatchAttemptError::WebhookDrainTimedOut),
            "the cancelled drain's pending delivery blocks the cursor-advancing poll"
        );
        fixture.task.webhook_terminal_ambiguous = ambiguous;
        assert!(!webhook_disposition_exists(&webhook_store, admission.key()).await?);
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .fetch_one(&mut *blocker)
            .await?;

        let retried = fixture.task.process_webhook_deliveries().await;

        assert!(unlocked, "the fixture releases its deliberate drain wedge");
        assert_eq!(retried, WebhookDrainOutcome::Drained);
        assert!(webhook_disposition_exists(&webhook_store, admission.key()).await?);
        clock_guard.abort();
        clock_guard.await.ok();
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn cursor_sizing_failure_clears_an_earlier_drain_timeout_marker()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.webhook_drain_timed_out = true;
        pool.close().await;

        let outcome = fixture.task.process_webhook_deliveries_with_timeout().await;

        assert_eq!(
            outcome,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Persistence)
        );
        assert!(!fixture.task.webhook_drain_timed_out);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn cursor_sizing_settles_a_retained_completion_before_reading_the_cursor()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let mut fixture = webhook_task(&pool).await?;
        let key = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?.key();
        fixture.task.webhook_targeted_completion =
            Some(super::RetainedTargetedWebhookCompletion::new(tokio::spawn(
                async move { Ok(super::TargetedWebhookCompletion::CursorSuperseded { key }) },
            )));

        let deadlines = fixture
            .task
            .webhook_attempt_deadlines()
            .await
            .expect("retained completion settles before cursor sizing");

        assert!(fixture.task.webhook_targeted_completion.is_none());
        assert_eq!(
            deadlines,
            super::load_webhook_attempt_deadlines(&fixture.task.store, &fixture.task.repository)
                .await
                .expect("settled cursor can be sized independently")
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn failed_pre_sizing_settlement_invalidates_unpublished_freshness()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let mut fixture = webhook_task(&pool).await?;
        fixture.task.poller.record_fetched_pull_request(
            PULL_NUMBER,
            &listed_pull_request(HEAD_SHA),
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture.task.webhook_targeted_completion =
            Some(super::RetainedTargetedWebhookCompletion::new(tokio::spawn(
                async move { Err(super::TargetedWebhookCompletionError::Cursor) },
            )));

        let result = fixture.task.webhook_attempt_deadlines().await;

        assert!(result.is_err());
        assert!(fixture.task.poller.freshness().is_empty());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_webhook_attempt_deadline_cancels_any_wedged_phase_and_retries()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        webhook_store
            .inject_projection_wedge(admission.key(), WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .await?;
        let mut blocker = pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .execute(&mut *blocker)
            .await?;
        let mut fixture = webhook_task(&pool).await?;

        let timed_out = fixture
            .task
            .run_webhook_attempt_with_deadlines(WebhookAttemptDeadlines {
                drain: Duration::from_secs(5),
                attempt: Duration::from_millis(50),
                cursor_payload_bytes: 0,
            })
            .await;

        // The projection wedge is reached by the cutoff and dispatch
        // reconciliation that now precedes the drain, so the deadline cancels
        // the attempt while it is still `BeforeDrain`. The phase matters to the
        // retry accounting rather than to the recovery: a cancellation before
        // the drain began neither grows nor clears the projection backoff.
        assert_eq!(
            timed_out,
            WebhookAttemptOutcome::FailedBeforeDrain(
                RepositoryWatchAttemptError::WebhookAttemptTimedOut
            )
        );
        assert!(!webhook_disposition_exists(&webhook_store, admission.key()).await?);
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(WEBHOOK_PROJECTION_ADVISORY_LOCK)
            .fetch_one(&mut *blocker)
            .await?;

        let retried = fixture
            .task
            .run_webhook_attempt_with_deadlines(WebhookAttemptDeadlines {
                drain: Duration::from_secs(5),
                attempt: Duration::from_secs(5),
                cursor_payload_bytes: 0,
            })
            .await;

        assert!(
            unlocked,
            "the fixture releases its deliberate attempt wedge"
        );
        assert_eq!(retried, WebhookAttemptOutcome::Completed);
        assert!(webhook_disposition_exists(&webhook_store, admission.key()).await?);
        Ok(())
    }

    /// A full poll drains durable webhook work as part of its own sequence, so
    /// leaving that step in while a retry is owed would repeat at the poll
    /// cadence exactly the work the backoff exists to space out.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_poll_leaves_the_drain_to_a_retry_that_is_already_owed() -> Result<(), Box<dyn Error>>
    {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let deferring = ScriptedServer::start(complete_typed_observation_responses()).await;
        let mut fixture = webhook_task_against(&pool, deferring.base_url.clone()).await?;

        let mut deferred_drain = None;
        let mut deferred_dispatch_failure = None;
        fixture
            .task
            .run_attempt(
                WebhookDrain::Deferred,
                &mut deferred_drain,
                &mut deferred_dispatch_failure,
            )
            .await
            .expect("the polling attempt itself succeeds");

        deferring.finish().await;
        assert_eq!(
            deferred_drain, None,
            "a deferred poll reports no drain for the backoff to read"
        );
        assert!(
            !webhook_disposition_exists(&webhook_store, admission.key()).await?,
            "a poll must leave a pending delivery to the retry that owes it"
        );

        let draining = ScriptedServer::start(complete_typed_observation_responses()).await;
        let mut owed_nothing = webhook_task_against(&pool, draining.base_url.clone()).await?;

        let mut performed_drain = None;
        let mut performed_dispatch_failure = None;
        owed_nothing
            .task
            .run_attempt(
                WebhookDrain::Run,
                &mut performed_drain,
                &mut performed_dispatch_failure,
            )
            .await
            .expect("the polling attempt drains when no retry is owed");

        draining.finish().await;
        assert_eq!(performed_drain, Some(WebhookDrainOutcome::Drained));
        assert!(webhook_disposition_exists(&webhook_store, admission.key()).await?);
        Ok(())
    }

    /// Cutoff work after the drain belongs to the same follow-up classification
    /// as the dispatch work beside it: the deliveries it follows are terminal
    /// and will wake no later attempt, so an attempt that let this failure
    /// escape would report a bare `Drained`, clear the deadline the committed
    /// work needs, and leave that work to an unrelated admission or the next
    /// poll. The fixture fault arms itself from the drain's terminal write, so
    /// the cutoff pass this attempt ran before its drain is left succeeding.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_trailing_poll_cutoff_failure_arms_a_follow_up() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let server = ScriptedServer::start(complete_typed_observation_responses()).await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;
        fixture
            .task
            .dispatch_store
            .inject_post_drain_lifecycle_cutoff_fault()
            .await?;

        let mut drained = None;
        let mut trailing_failure = None;
        let attempt = fixture
            .task
            .run_attempt(WebhookDrain::Run, &mut drained, &mut trailing_failure)
            .await;

        server.finish().await;
        assert_eq!(attempt, Err(RepositoryWatchAttemptError::Persistence));
        assert_eq!(
            drained,
            Some(WebhookDrainOutcome::Drained),
            "every delivery the drain visited reached terminal state"
        );
        assert_eq!(
            trailing_failure,
            Some(RepositoryWatchAttemptError::Persistence),
            "the cutoff that failed after the drain committed is reported, not swallowed"
        );
        assert!(
            webhook_disposition_exists(&webhook_store, admission.key()).await?,
            "the drain committed the work whose follow-up this failure owes"
        );
        let armed_at = Instant::now();
        let mut retry = WebhookDrainRetry::default();
        retry.update_after_poll_drain(
            drained.expect("the attempt reported the drain it performed"),
            trailing_failure,
            armed_at,
        );
        assert_eq!(retry.deadline, Some(armed_at + WEBHOOK_DRAIN_RETRY_DELAY));
        Ok(())
    }

    /// A refresh the provider will not serve leaves its delivery pending rather
    /// than terminal, because the query runs before anything is recorded and a
    /// transient fetch failure must stay retryable. What keeps an admitted
    /// burst of unservable targets from denying webhook projection is that the
    /// drain defers such a delivery for the rest of the page: every newer
    /// receipt behind it still reaches terminal state.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn an_unservable_refresh_target_defers_rather_than_pinning_its_page()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let unservable = synchronize_admission(THIRD_WEBHOOK_DELIVERY)?;
        let behind_it = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&unservable).await?;
        webhook_store.admit(&behind_it).await?;
        let server = ScriptedServer::start(vec![ScriptedResponse::not_found(RequestTarget(
            PULL_DETAIL_TARGET.to_owned(),
        ))])
        .await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        server.finish().await;
        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Rejected)
        );
        assert!(!webhook_disposition_exists(&webhook_store, unservable.key()).await?);
        assert!(webhook_disposition_exists(&webhook_store, behind_it.key()).await?);
        Ok(())
    }

    #[test]
    fn non_enqueued_repo_watch_nudges_are_recorded() -> Result<(), Box<dyn Error>> {
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?;
        let session = signalbox_domain::SessionId::from_uuid(Uuid::from_u128(0x69));
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(captured.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            record_dispatch_start_nudge_outcome(
                &repository,
                session,
                EligibilityNudgeOutcome::Coalesced,
            );
            record_dispatch_start_nudge_outcome(
                &repository,
                session,
                EligibilityNudgeOutcome::DroppedAtCapacity,
            );
            record_dispatch_start_nudge_outcome(
                &repository,
                session,
                EligibilityNudgeOutcome::WorkSourceClosed,
            );
        });
        let telemetry = captured.text();

        assert!(telemetry.contains("repository_watch_dispatch_start_nudge_coalesced"));
        assert!(telemetry.contains("repository_watch_dispatch_start_nudge_capacity"));
        assert!(telemetry.contains("repository_watch_dispatch_start_nudge_closed"));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_provider_wide_rejection_stops_the_page_and_preserves_its_durable_tail()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let throttled = synchronize_admission(THIRD_WEBHOOK_DELIVERY)?;
        let tail = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&throttled).await?;
        webhook_store.admit(&tail).await?;
        let server = ScriptedServer::start(vec![
            ScriptedResponse::forbidden(RequestTarget(PULL_DETAIL_TARGET.to_owned())).throttled(),
        ])
        .await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        server.finish().await;
        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::ProviderUnavailable)
        );
        assert!(!webhook_disposition_exists(&webhook_store, throttled.key()).await?);
        assert!(!webhook_disposition_exists(&webhook_store, tail.key()).await?);
        Ok(())
    }

    /// A credential that lacks permission on one endpoint answers `403` without
    /// the provider's rate-limit headers. Stopping the page there would break
    /// the drain at that same oldest receipt on every retry, so every later
    /// delivery behind it is never attempted — a permanent per-repository stall
    /// reachable with an ordinary under-scoped token.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_permission_scoped_rejection_leaves_the_rest_of_the_page_attempted()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let unscoped = synchronize_admission(THIRD_WEBHOOK_DELIVERY)?;
        let tail = ignored_admission(FIRST_WEBHOOK_DELIVERY)?;
        webhook_store.admit(&unscoped).await?;
        webhook_store.admit(&tail).await?;
        let server = ScriptedServer::start(vec![ScriptedResponse::forbidden(RequestTarget(
            PULL_DETAIL_TARGET.to_owned(),
        ))])
        .await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        server.finish().await;
        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Rejected)
        );
        assert!(!webhook_disposition_exists(&webhook_store, unscoped.key()).await?);
        assert!(
            webhook_disposition_exists(&webhook_store, tail.key()).await?,
            "the receipt behind the unscoped one is still attempted"
        );
        Ok(())
    }

    /// A rejection the headers already classify is never read, which is what
    /// keeps a stalled or broken body from holding the serialized repository
    /// task to the request timeout during the outage that produced it. The
    /// scripted response promises a byte it never sends: a drain that read it
    /// would report that transport failure instead of the throttle the headers
    /// had already named.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_header_signalled_rejection_is_classified_without_reading_its_body()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let throttled = synchronize_admission(THIRD_WEBHOOK_DELIVERY)?;
        webhook_store.admit(&throttled).await?;
        let server = ScriptedServer::start(vec![
            ScriptedResponse::forbidden(RequestTarget(PULL_DETAIL_TARGET.to_owned()))
                .throttled()
                .truncated(),
        ])
        .await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        server.finish().await;
        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::ProviderUnavailable)
        );
        assert!(!webhook_disposition_exists(&webhook_store, throttled.key()).await?);
        Ok(())
    }

    /// A headerless `403` is the one rejection whose body has to be read before
    /// it can be classified, so it is also the one whose read can fail. That
    /// failure is a transport failure and stops the page: reading it as the
    /// empty-bodied permission rejection instead would keep hydrating later
    /// deliveries over a transport that had just broken.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_rejection_whose_body_fails_mid_read_stops_the_whole_page()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let unreadable = synchronize_admission(THIRD_WEBHOOK_DELIVERY)?;
        let behind_it = ignored_admission(FIRST_WEBHOOK_DELIVERY)?;
        webhook_store.admit(&unreadable).await?;
        webhook_store.admit(&behind_it).await?;
        let server = ScriptedServer::start(vec![
            ScriptedResponse::forbidden(RequestTarget(PULL_DETAIL_TARGET.to_owned())).truncated(),
        ])
        .await;
        let mut fixture = webhook_task_against(&pool, server.base_url.clone()).await?;

        let attempt = fixture.task.process_webhook_deliveries().await;

        server.finish().await;
        assert_eq!(
            attempt,
            WebhookDrainOutcome::ProjectionFailed(RepositoryWatchAttemptError::Request)
        );
        assert!(!webhook_disposition_exists(&webhook_store, unreadable.key()).await?);
        assert!(
            !webhook_disposition_exists(&webhook_store, behind_it.key()).await?,
            "the page stops rather than hydrating over the failed transport"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn wedged_webhook_drain_emits_an_error_with_its_cause() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let store = PostgresRepoWatchWebhookStore::new(pool);
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        store.admit(&admission).await?;
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?;
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .with_writer(captured.clone())
            .finish();

        let mut progress = WebhookDrainProgress::default();
        inspect_webhook_drain(&repository, &store, Duration::ZERO, &mut progress).await;
        inspect_webhook_drain(&repository, &store, Duration::ZERO, &mut progress)
            .with_subscriber(subscriber)
            .await;

        let telemetry = captured.text();
        assert!(telemetry.contains("ERROR"));
        assert!(telemetry.contains("cause_code=\"webhook_projection_drain_stalled\""));
        assert!(telemetry.contains(&admission.key().delivery_id().to_string()));
        Ok(())
    }

    #[test]
    fn advancing_webhook_head_resets_the_stall_clock() {
        let first_sequence = NonZeroU64::new(41).expect("fixture sequence is positive");
        let next_sequence = NonZeroU64::new(42).expect("fixture sequence is positive");
        let started_at = Instant::now();
        let threshold = Duration::from_secs(60);
        let mut progress = WebhookDrainProgress::default();

        assert_eq!(
            progress.observe(first_sequence, started_at, threshold),
            None
        );
        assert_eq!(
            progress.observe(
                first_sequence,
                started_at + Duration::from_secs(31),
                threshold,
            ),
            Some((Duration::from_secs(31), threshold))
        );
        assert_eq!(
            progress.observe(
                next_sequence,
                started_at + Duration::from_secs(32),
                threshold,
            ),
            None
        );
        assert_eq!(
            progress.observe(
                next_sequence,
                started_at + Duration::from_secs(90),
                threshold,
            ),
            Some((Duration::from_secs(58), threshold))
        );
    }

    #[test]
    fn an_unchanged_webhook_head_never_reduces_its_stall_threshold() {
        let sequence = NonZeroU64::new(41).expect("fixture sequence is positive");
        let started_at = Instant::now();
        let larger = Duration::from_secs(300);
        let smaller = Duration::from_secs(60);
        let mut progress = WebhookDrainProgress::default();

        assert_eq!(progress.observe(sequence, started_at, larger), None);
        assert_eq!(
            progress.observe(sequence, started_at + smaller, smaller),
            Some((smaller, larger))
        );
    }

    #[tokio::test]
    async fn convergence_matches_the_exact_head_gate() {
        let server = ScriptedServer::start(complete_typed_observation_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let polled = fixture
            .poller
            .poll_against_cursor(None, Some(RepoWatchCursorGeneration::INITIAL))
            .await
            .expect("full poll and convergence assessment succeed");
        server.finish().await;
        let assessment = &polled.convergence[0];

        assert_eq!(assessment.gating_check_count(), 1);
        assert_eq!(
            assessment.non_green_gating_checks()[0].as_str(),
            CHECK_RUN_NAME
        );
        assert_eq!(assessment.unresolved_threads()[0].as_str(), REVIEW_THREAD);
        assert_eq!(
            assessment.verdict(),
            signalbox_application::RepoWatchConvergenceVerdict::NotConverged
        );
    }

    #[tokio::test]
    async fn older_head_review_becomes_a_clearance_candidate() {
        let response = ScriptedResponse::post(
            RequestTarget(String::from(THREADS_TARGET)),
            ResponseBody(blocking_reviews(STALE_REVIEW_HEAD_SHA)),
        )
        .matching_request_body(String::from("RepositoryWatchBlockingReviews"));
        let server = ScriptedServer::start(vec![response]).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let candidates = fixture
            .poller
            .fetch_stale_review_clearances(&review_only_blocked_assessment())
            .await
            .expect("blocking review evidence is valid");
        server.finish().await;

        assert_eq!(candidates[0].review_node_id(), STALE_REVIEW_NODE_ID);
        assert_eq!(candidates[0].reviewer().as_str(), REVIEWER);
        assert_eq!(
            candidates[0].reviewed_head_sha().as_str(),
            STALE_REVIEW_HEAD_SHA
        );
    }

    #[tokio::test]
    async fn current_head_review_is_not_a_clearance_candidate() {
        let response = ScriptedResponse::post(
            RequestTarget(String::from(THREADS_TARGET)),
            ResponseBody(blocking_reviews(HEAD_SHA)),
        )
        .matching_request_body(String::from("RepositoryWatchBlockingReviews"));
        let server = ScriptedServer::start(vec![response]).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let candidates = fixture
            .poller
            .fetch_stale_review_clearances(&review_only_blocked_assessment())
            .await
            .expect("current-head blocker fails closed without an error");
        server.finish().await;

        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn dismissal_mutation_requires_the_expected_review_identity() {
        const MISMATCHING_REVIEW_NODE_ID: &str = "PRR_mismatching_review_node";
        let response = ScriptedResponse::post(
            RequestTarget(String::from(THREADS_TARGET)),
            ResponseBody(dismissed_review(MISMATCHING_REVIEW_NODE_ID)),
        )
        .matching_request_body(String::from("RepositoryWatchDismissReview"));
        let server = ScriptedServer::start(vec![response]).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let error = fixture
            .poller
            .dismiss_review_node(super::DismissReviewInput {
                review_node_id: STALE_REVIEW_NODE_ID,
                dismissal_message: DISMISSAL_MESSAGE,
            })
            .await
            .expect_err("a response naming another review must fail closed");
        server.finish().await;

        assert_eq!(error, RepositoryWatchAttemptError::InvalidResponse);
    }

    fn planned_stale_review_clearance() -> super::RepoWatchPlannedStaleReviewClearance {
        super::RepoWatchPlannedStaleReviewClearance::from_fixture(
            RepoWatchPlannedStaleReviewClearanceFixture {
                clearance_id: RepoWatchStaleReviewClearanceId::new(Uuid::from_u128(0x_c1ea_0001)),
                claim_token: RepoWatchStaleReviewClearanceClaimToken::new(Uuid::from_u128(
                    0x_c1a1_0001,
                )),
                number: PullRequestNumber::new(
                    NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
                ),
                current_head_sha: CommitSha::try_new(String::from(HEAD_SHA))
                    .expect("fixture head is canonical"),
                base_branch: BranchName::try_new(String::from(BASE_BRANCH))
                    .expect("fixture base branch is canonical"),
                base_revision: CommitSha::try_new(String::from(BASE_SHA))
                    .expect("fixture base revision is canonical"),
                review_node_id: String::from(STALE_REVIEW_NODE_ID),
                reviewer: RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
                    .expect("fixture reviewer is valid"),
                reviewed_head_sha: CommitSha::try_new(String::from(STALE_REVIEW_HEAD_SHA))
                    .expect("fixture reviewed head is canonical"),
                dismissal_message: String::from(DISMISSAL_MESSAGE),
            },
        )
    }

    /// The in-memory candidate the committed poll raises for the review
    /// [`blocking_reviews`] reports, against the evidence
    /// [`review_only_blocked_assessment`] records.
    fn stale_review_clearance_candidate() -> RepoWatchStaleReviewClearanceCandidate {
        RepoWatchStaleReviewClearanceCandidate::try_new(
            &review_only_blocked_assessment(),
            String::from(STALE_REVIEW_NODE_ID),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
                .expect("fixture reviewer is valid"),
            CommitSha::try_new(String::from(STALE_REVIEW_HEAD_SHA))
                .expect("fixture reviewed head is canonical"),
        )
        .expect("the review is the fixture head's only convergence blocker")
    }

    /// Revalidation is the gate the dismissal mutation sits behind, and it
    /// reports the clearance still holds only for a settled head. Settlement in
    /// turn requires a quiesced gating-check inventory, so evidence that never
    /// carries quiescence makes the whole feature a no-op: the candidate lookup
    /// short-circuits, the revalidation refuses, and no review is ever
    /// dismissed. This proves the re-read backed by the committed poll's
    /// freshness passes that gate;
    /// [`a_planned_clearance_reaches_its_dismissal_mutation`] proves the
    /// orchestration then issues the mutation.
    #[tokio::test]
    async fn a_quiesced_inventory_revalidates_a_planned_clearance() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(String::from(PULL_DETAIL_TARGET)),
                ResponseBody(mergeable_pull_detail()),
            ),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(review_only_blocked_convergence()),
            )
            .matching_request_body(String::from("RepositoryWatchConvergence")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(empty_threads()),
            )
            .matching_request_body(String::from("RepositoryWatchReviewThreads")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(blocking_reviews(STALE_REVIEW_HEAD_SHA)),
            )
            .matching_request_body(String::from("RepositoryWatchBlockingReviews")),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let generation = RepoWatchCursorGeneration::INITIAL;
        fixture.poller.record_fetched_pull_request(
            PULL_NUMBER,
            &listed_pull_request(HEAD_SHA),
            PullRequestSettlement::Settled,
            vec![String::from(CHECK_RUN_NAME)],
        );
        fixture.poller.publish_freshness(generation);

        let holds = fixture
            .poller
            .revalidate_stale_review_clearance(&planned_stale_review_clearance(), generation)
            .await
            .expect("clearance revalidation reads valid evidence");
        server.finish().await;

        assert!(
            holds,
            "a settled head whose only blocker is a superseded review must pass revalidation"
        );
    }

    /// The whole live path, from the poll that commits the candidate's evidence
    /// to the provider mutation: the completed poll records its assessment,
    /// plans the intent durably, revalidates it against a re-read, and sends the
    /// dismissal. Scripting the mutation as a matched response is what makes
    /// this end-to-end rather than a revalidation test — a build that stops
    /// short of `dismiss_review_node` leaves that response unconsumed and no
    /// terminal outcome recorded.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_planned_clearance_reaches_its_dismissal_mutation() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let server = ConcurrentScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(String::from(PULL_DETAIL_TARGET)),
                ResponseBody(mergeable_pull_detail()),
            ),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(review_only_blocked_convergence()),
            )
            .matching_request_body(String::from("RepositoryWatchConvergence")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(empty_threads()),
            )
            .matching_request_body(String::from("RepositoryWatchReviewThreads")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(blocking_reviews(STALE_REVIEW_HEAD_SHA)),
            )
            .matching_request_body(String::from("RepositoryWatchBlockingReviews")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(dismissed_review(STALE_REVIEW_NODE_ID)),
            )
            .matching_request_body(String::from("RepositoryWatchDismissReview")),
        ])
        .await;
        let observation = review_only_blocked_observation().await;
        let mut fixture = task_against(&pool, server.base_url.clone(), observation.clone()).await?;
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?;
        let generation = PostgresRepoWatchStore::new(pool.clone())
            .load_cursor(&repository)
            .await?
            .expect("the fixture commits its cursor")
            .generation();
        fixture.task.poller.record_fetched_pull_request(
            PULL_NUMBER,
            &listed_pull_request(HEAD_SHA),
            PullRequestSettlement::Settled,
            vec![String::from(CHECK_RUN_NAME)],
        );

        fixture
            .task
            .commit_complete_poll(super::PreparedCompletePoll {
                cursor_generation: Some(generation),
                candidate: RepoWatchCursorCandidate::new(observation),
                events: Vec::new(),
                convergence: vec![review_only_blocked_assessment()],
                stale_review_clearances: vec![stale_review_clearance_candidate()],
            })
            .await
            .expect("the completed poll commits its evidence and sweeps its clearances");
        // Asserts that every scripted response was consumed and that every
        // request matched one, so the dismissal mutation reached the provider
        // as the mutation it claims to be rather than as some other body.
        server.finish().await;

        let outcome: String =
            sqlx::query_scalar("SELECT outcome_kind FROM repo_watch_stale_review_clearance_result")
                .fetch_one(&pool)
                .await?;
        assert_eq!(outcome, "dismissed");
        Ok(())
    }

    /// The mirror of the revalidation test: a gating check that appeared since
    /// the committed poll leaves the inventory unquiesced, the head unsettled,
    /// and the review undismissed. The candidate lookup short-circuits before
    /// its provider request, so only three calls are scripted.
    #[tokio::test]
    async fn a_gating_check_added_since_the_committed_poll_refuses_the_clearance() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(String::from(PULL_DETAIL_TARGET)),
                ResponseBody(mergeable_pull_detail()),
            ),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(review_only_blocked_convergence()),
            )
            .matching_request_body(String::from("RepositoryWatchConvergence")),
            ScriptedResponse::post(
                RequestTarget(String::from(THREADS_TARGET)),
                ResponseBody(empty_threads()),
            )
            .matching_request_body(String::from("RepositoryWatchReviewThreads")),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let generation = RepoWatchCursorGeneration::INITIAL;
        fixture.poller.record_fetched_pull_request(
            PULL_NUMBER,
            &listed_pull_request(HEAD_SHA),
            PullRequestSettlement::Settled,
            vec![String::from(CHECK_RUN_NAME), String::from("later gate")],
        );
        fixture.poller.publish_freshness(generation);

        let holds = fixture
            .poller
            .revalidate_stale_review_clearance(&planned_stale_review_clearance(), generation)
            .await
            .expect("clearance revalidation reads valid evidence");
        server.finish().await;

        assert!(
            !holds,
            "an inventory that has not stood still since the committed poll must refuse dismissal"
        );
    }

    #[test]
    fn recovery_settles_review_states_that_no_longer_block() {
        use signalbox_persistence::repo_watch::{
            RepoWatchObservedReviewState, RepoWatchStaleReviewClearanceOutcome,
        };

        assert_eq!(
            super::terminal_clearance_outcome(RepoWatchObservedReviewState::Dismissed),
            Some(RepoWatchStaleReviewClearanceOutcome::AlreadyDismissed)
        );
        assert_eq!(
            super::terminal_clearance_outcome(RepoWatchObservedReviewState::Approved),
            Some(RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere)
        );
        assert_eq!(
            super::terminal_clearance_outcome(RepoWatchObservedReviewState::Commented),
            Some(RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere)
        );
        assert_eq!(
            super::terminal_clearance_outcome(RepoWatchObservedReviewState::Pending),
            Some(RepoWatchStaleReviewClearanceOutcome::ClearedElsewhere)
        );
        assert_eq!(
            super::terminal_clearance_outcome(RepoWatchObservedReviewState::ChangesRequested),
            None
        );
    }

    #[tokio::test]
    async fn convergence_rejects_evidence_from_a_different_base_revision() {
        let observation = complete_typed_observation().await;
        let pull_request = &observation.state().pull_requests()[0];
        let evidence = super::FetchedConvergenceEvidence {
            base_revision: CommitSha::try_new(CHANGED_LISTED_HEAD_SHA.to_owned())
                .expect("fixture provider base revision is valid"),
            gating_checks_settled: true,
            gating_check_inventory_quiesced: true,
            gating_check_inventory: vec![String::from(CHECK_RUN_NAME)],
            review_decision: super::RepoWatchReviewDecision::Approved,
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        };
        let snapshot_base_revision = CommitSha::try_new(BASE_SHA.to_owned())
            .expect("fixture snapshot base revision is valid");

        let assessment = evidence.assess(pull_request, snapshot_base_revision);

        assert!(matches!(
            assessment,
            Err(RepositoryWatchAttemptError::InvalidResponse)
        ));
    }

    #[test]
    fn codecov_project_status_is_report_only() {
        let check = ConvergenceCheck::StatusContext {
            context: String::from("codecov/project"),
            state: String::from("PENDING"),
        };

        assert!(check.is_report_only());
    }

    #[test]
    fn codecov_patch_status_is_report_only_case_insensitively() {
        let check = ConvergenceCheck::StatusContext {
            context: String::from("Codecov/Patch"),
            state: String::from("PENDING"),
        };

        assert!(check.is_report_only());
    }

    #[tokio::test]
    async fn shutdown_wins_when_a_repository_task_exits_cleanly_at_the_same_time() {
        let (sender, receiver) = watch::channel(false);
        let exit = Arc::new(Notify::new());
        let mut tasks = JoinSet::new();
        tasks.spawn({
            let exit = Arc::clone(&exit);
            async move {
                exit.notified().await;
                RepositoryWatchChildExit::Repository
            }
        });
        let trigger = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sender
                .send(true)
                .expect("fixture supervisor still holds the shutdown receiver");
            exit.notify_one();
        });

        let (task_shutdown, _task_shutdown_receiver) = watch::channel(false);
        let result = supervise_repository_tasks(tasks, Vec::new(), receiver, task_shutdown).await;
        trigger.await.expect("fixture race trigger completes");

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn supervisor_failure_drains_sibling_fetches() {
        let server = ConcurrentScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/pulls/{CANCELLED_FETCH_PULL_NUMBER}"
                )),
                ResponseBody(minimal_pull_detail(CANCELLED_FETCH_PULL_NUMBER)),
            )
            .delayed(CANCELLED_FETCH_DELAY),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let listed = BTreeMap::from([(
            CANCELLED_FETCH_PULL_NUMBER,
            listed_pull_request(&minimal_pull_head_sha(CANCELLED_FETCH_PULL_NUMBER)),
        )]);
        let mut tasks = JoinSet::new();
        tasks.spawn({
            let poller = Arc::clone(&fixture.poller);
            async move {
                let _ = poller
                    .fetch_pull_requests(
                        BTreeSet::from([CANCELLED_FETCH_PULL_NUMBER]),
                        &listed,
                        None,
                        Some(RepoWatchCursorGeneration::INITIAL),
                        &[base_branch_head()],
                    )
                    .await;
                RepositoryWatchChildExit::Repository
            }
        });
        server.request_in_flight().await;
        tasks.spawn(async { panic!("fixture repository task panics") });
        let (_sender, receiver) = watch::channel(false);
        let (task_shutdown, _task_shutdown_receiver) = watch::channel(false);

        let result = supervise_repository_tasks(
            tasks,
            vec![Arc::clone(&fixture.poller)],
            receiver,
            task_shutdown,
        )
        .await;

        assert_eq!(
            result,
            Err(RepositoryWatchRuntimeError::RepositoryTaskPanicked)
        );
        assert_eq!(
            Arc::strong_count(&fixture.poller),
            1,
            "a failed supervisor leaves no child fetch holding the sibling poller"
        );
    }

    #[tokio::test]
    async fn dropping_a_retained_targeted_completion_aborts_its_writer() {
        let ownership = Arc::new(());
        let child_ownership = Arc::clone(&ownership);
        let retained = super::RetainedTargetedWebhookCompletion::new(tokio::spawn(async move {
            let _child_ownership = child_ownership;
            std::future::pending::<
                Result<super::TargetedWebhookCompletion, super::TargetedWebhookCompletionError>,
            >()
            .await
        }));

        drop(retained);
        tokio::task::yield_now().await;

        assert_eq!(
            Arc::strong_count(&ownership),
            1,
            "dropping the repository task cannot detach its retained writer"
        );
    }

    #[tokio::test]
    async fn repository_task_panic_during_shutdown_drain_drains_sibling_fetches() {
        let server = ConcurrentScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/pulls/{CANCELLED_FETCH_PULL_NUMBER}"
                )),
                ResponseBody(minimal_pull_detail(CANCELLED_FETCH_PULL_NUMBER)),
            )
            .delayed(CANCELLED_FETCH_DELAY),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let listed = BTreeMap::from([(
            CANCELLED_FETCH_PULL_NUMBER,
            listed_pull_request(&minimal_pull_head_sha(CANCELLED_FETCH_PULL_NUMBER)),
        )]);
        let mut tasks = JoinSet::new();
        tasks.spawn({
            let poller = Arc::clone(&fixture.poller);
            async move {
                let _ = poller
                    .fetch_pull_requests(
                        BTreeSet::from([CANCELLED_FETCH_PULL_NUMBER]),
                        &listed,
                        None,
                        Some(RepoWatchCursorGeneration::INITIAL),
                        &[base_branch_head()],
                    )
                    .await;
                RepositoryWatchChildExit::Repository
            }
        });
        server.request_in_flight().await;
        tasks.spawn(async { panic!("fixture repository task panics during shutdown") });
        let (_sender, receiver) = watch::channel(true);
        let (task_shutdown, _task_shutdown_receiver) = watch::channel(true);

        let result = supervise_repository_tasks(
            tasks,
            vec![Arc::clone(&fixture.poller)],
            receiver,
            task_shutdown,
        )
        .await;

        assert_eq!(
            result,
            Err(RepositoryWatchRuntimeError::RepositoryTaskPanicked)
        );
        assert_eq!(
            Arc::strong_count(&fixture.poller),
            1,
            "a shutdown-drain panic leaves no child fetch holding the sibling poller"
        );
    }

    #[test]
    fn retired_rule_identity_terminates_repository_attempts() {
        let rule_id = RepoWatchRuleId::try_new(String::from("retired-rule"))
            .expect("fixture rule ID is valid");
        let error = rule_activation_error(RepoWatchDispatchRepositoryError::ReusedRuleIdentity {
            rule_id,
            rule_version: RepoWatchRuleVersion::V1,
        });

        assert_eq!(error, RepositoryWatchAttemptError::RetiredRuleIdentity);
        assert!(error.is_permanent());
    }

    #[test]
    fn changed_rule_identity_terminates_repository_attempts() {
        let rule_id = RepoWatchRuleId::try_new(String::from("changed-rule"))
            .expect("fixture rule ID is valid");
        let error = rule_activation_error(RepoWatchDispatchRepositoryError::ChangedRuleIdentity {
            rule_id,
            rule_version: RepoWatchRuleVersion::V1,
            field: RepoWatchRuleIdentityField::MatcherMergeableStateAnyOf,
        });

        assert_eq!(error, RepositoryWatchAttemptError::ChangedRuleIdentity);
        assert!(error.is_permanent());
    }

    #[test]
    fn regressed_rule_version_terminates_repository_attempts() {
        let rule_id = RepoWatchRuleId::try_new(String::from("regressed-rule"))
            .expect("fixture rule ID is valid");
        let latest_version = RepoWatchRuleVersion::new(
            NonZeroU64::new(2).expect("recorded fixture version is positive"),
        )
        .expect("recorded fixture version is within the durable range");
        let error = rule_activation_error(RepoWatchDispatchRepositoryError::RegressedRuleVersion {
            rule_id,
            rule_version: RepoWatchRuleVersion::V1,
            latest_version,
        });

        assert_eq!(error, RepositoryWatchAttemptError::RegressedRuleVersion);
        assert!(error.is_permanent());
    }

    #[tokio::test]
    async fn resource_level_not_modified_reuses_only_its_typed_accepted_state() {
        let server = ScriptedServer::start(
            complete_poll_responses()
                .into_iter()
                .chain(conditional_poll_responses())
                .collect(),
        )
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("conditional poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn a_restarted_poller_has_no_process_local_validators() {
        let server = ScriptedServer::start(
            complete_poll_responses()
                .into_iter()
                .chain(complete_poll_responses())
                .collect(),
        )
        .await;
        let first_poller =
            poller_fixture(server.base_url.clone()).expect("first poller is constructed");
        let first = first_poller
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        let restarted = poller_fixture(server.base_url.clone()).expect("poller restarts");
        let after_restart = restarted
            .poller
            .poll(Some(&first))
            .await
            .expect("restart performs a full poll");
        server.finish().await;

        assert_eq!(after_restart, first);
    }

    #[tokio::test]
    async fn workflow_listing_follows_the_link_after_a_full_page() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok_with_next(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(full_workflow_page()),
            ),
            ScriptedResponse::ok(
                RequestTarget(SECOND_WORKFLOWS_PAGE_TARGET.to_owned()),
                ResponseBody(EMPTY_WORKFLOW_LIST.to_owned()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;

        assert!(observation.state().workflow_runs().is_empty());
    }

    #[tokio::test]
    async fn workflow_listing_accepts_a_full_terminal_page_without_a_link() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(full_workflow_page()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;

        assert!(observation.state().workflow_runs().is_empty());
    }

    #[tokio::test]
    async fn cached_full_terminal_page_probes_one_bounded_successor() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(WORKFLOWS_TARGET.to_owned()),
                ResponseBody(full_workflow_page()),
            ),
            ScriptedResponse::not_modified(RequestTarget(PULLS_TARGET.to_owned())),
            ScriptedResponse::not_modified(RequestTarget(BRANCHES_TARGET.to_owned())),
            ScriptedResponse::not_modified(RequestTarget(WORKFLOWS_TARGET.to_owned())),
            ScriptedResponse::ok(
                RequestTarget(SECOND_WORKFLOWS_PAGE_TARGET.to_owned()),
                ResponseBody(EMPTY_WORKFLOW_LIST.to_owned()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");

        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("cached boundary poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn branch_projection_skips_a_fork_run_with_the_same_branch_name() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
            ResponseBody(foreign_then_watched_workflow_runs()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();

        let run = fixture
            .poller
            .fetch_workflow_runs(std::slice::from_ref(&branch), &workflow, &[])
            .await
            .expect("workflow-run response is valid")
            .into_iter()
            .next()
            .expect("watched-repository run remains in the response");
        server.finish().await;

        assert_eq!(run.id().get(), WORKFLOW_RUN_IDS[0]);
    }

    #[tokio::test]
    async fn active_rerun_retains_the_previous_completed_workflow_baseline() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
            ResponseBody(active_rerun_then_stale_workflow_run()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();
        let previous = previous_main_workflow_run();

        let run = fixture
            .poller
            .fetch_workflow_runs(
                std::slice::from_ref(&branch),
                &workflow,
                std::slice::from_ref(&previous),
            )
            .await
            .expect("unfiltered workflow-run response is valid")
            .into_iter()
            .next()
            .expect("previous completed run for the watched branch remains visible");
        server.finish().await;

        assert_eq!(run, previous);
    }

    #[tokio::test]
    async fn active_run_does_not_hide_a_newer_completed_workflow_baseline() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
            ResponseBody(active_rerun_then_stale_workflow_run()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();
        let previous = older_main_workflow_run();

        let run = fixture
            .poller
            .fetch_workflow_runs(
                std::slice::from_ref(&branch),
                &workflow,
                std::slice::from_ref(&previous),
            )
            .await
            .expect("unfiltered workflow-run response is valid")
            .into_iter()
            .next()
            .expect("newer completed run for the watched branch becomes visible");
        server.finish().await;

        assert_eq!(run.id().get(), WORKFLOW_RUN_IDS[0]);
    }

    #[tokio::test]
    async fn branch_projection_follows_a_full_page_of_fork_runs() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok_with_next(
                RequestTarget(MAIN_WORKFLOW_TARGET.to_owned()),
                ResponseBody(full_foreign_workflow_run_page()),
            ),
            ScriptedResponse::ok(
                RequestTarget(SECOND_MAIN_WORKFLOW_PAGE_TARGET.to_owned()),
                ResponseBody(main_workflow_run()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();

        let run = fixture
            .poller
            .fetch_workflow_runs(std::slice::from_ref(&branch), &workflow, &[])
            .await
            .expect("workflow-run pages are valid")
            .into_iter()
            .next()
            .expect("watched-repository run is found on the next page");
        server.finish().await;

        assert_eq!(run.id().get(), WORKFLOW_RUN_IDS[0]);
    }

    #[tokio::test]
    async fn an_invalid_changed_response_invalidates_its_cached_resource_pair() {
        let responses = complete_poll_responses()
            .into_iter()
            .chain([ScriptedResponse::conditional_ok(
                RequestTarget(PULLS_TARGET.to_owned()),
                ResponseBody(MALFORMED_JSON.to_owned()),
            )])
            .chain([
                ScriptedResponse::ok(
                    RequestTarget(PULLS_TARGET.to_owned()),
                    ResponseBody(EMPTY_LIST.to_owned()),
                ),
                ScriptedResponse::not_modified(RequestTarget(BRANCHES_TARGET.to_owned())),
                ScriptedResponse::not_modified(RequestTarget(WORKFLOWS_TARGET.to_owned())),
            ])
            .collect();
        let server = ScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        let rejected = fixture.poller.poll(Some(&first)).await.err();
        let recovered = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("the next poll refetches the invalidated resource");
        server.finish().await;

        assert_eq!(rejected, Some(RepositoryWatchAttemptError::InvalidResponse));
        assert_eq!(recovered, first);
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_pull_request_context() {
        let observation = complete_typed_observation().await;
        let state = observation.state();
        let pull = &state.pull_requests()[0];
        let context = pull.context();

        assert_eq!(observation.signal_reviewers()[0].as_str(), REVIEWER);
        assert_eq!(state.pull_requests().len(), PULL_NUMBERS.len());
        assert_eq!(context.number().get(), PULL_NUMBER);
        assert_eq!(context.head_sha().as_str(), HEAD_SHA);
        assert_eq!(context.head_repository().as_str(), HEAD_REPOSITORY);
        assert_eq!(context.base_branch().as_str(), BASE_BRANCH);
        assert_eq!(context.head_branch().as_str(), HEAD_BRANCH);
        assert_eq!(context.title().as_str(), PULL_TITLE);
        assert_eq!(context.body().as_str(), PULL_BODY);
        assert_eq!(context.labels()[0].as_str(), PULL_LABEL);
        assert_eq!(
            context.author().expect("fixture has an author").as_str(),
            PULL_AUTHOR
        );
    }

    #[test]
    fn deleted_head_fork_reuses_the_previous_canonical_repository() {
        let previous_response = serde_json::from_str::<PullResponse>(&pull_detail())
            .expect("fixture pull detail is valid");
        let previous = normalize_pull_request_context(
            &previous_response,
            CommitSha::try_new(String::from(HEAD_SHA)).expect("fixture commit is valid"),
            None,
        )
        .expect("fixture prior context is valid");
        let current_response =
            serde_json::from_str::<PullResponse>(&pull_detail_without_head_repository())
                .expect("deleted-fork pull detail is valid");

        let current = normalize_pull_request_context(
            &current_response,
            CommitSha::try_new(String::from(HEAD_SHA)).expect("fixture commit is valid"),
            Some(&previous),
        )
        .expect("prior repository supplies the deleted-fork identity");

        assert_eq!(current.head_repository(), previous.head_repository());
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_pull_request_lifecycle() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.lifecycle(), EXPECTED_LIFECYCLE);
    }

    #[tokio::test]
    async fn a_durable_cursor_evicts_merged_pull_request_details() {
        let observation =
            observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Merged).await;

        let compacted = compact_cursor_observation(&observation, None, &[])
            .expect("fixture observation compacts canonically");

        assert!(compacted.observation.state().pull_requests().is_empty());
        assert_eq!(compacted.merged_pull_request_baselines.len(), 1);
        assert_eq!(
            compacted.merged_pull_request_baselines[0].number(),
            observation.state().pull_requests()[0].context().number()
        );
        assert_eq!(
            compacted.observation.state().workflow_runs(),
            observation.state().workflow_runs()
        );
        assert_eq!(
            compacted.observation.state().branch_heads(),
            observation.state().branch_heads()
        );
        assert_eq!(
            compacted.observation.signal_reviewers(),
            observation.signal_reviewers()
        );
    }

    /// A storage-version-three cursor migrated to version four holds its merged
    /// pull requests in full and no baselines. A complete poll fetches only
    /// listed open pull requests and previously open ones, so that merged entry
    /// is absent from the polled observation. Compacting the polled observation
    /// alone would drop it without leaving the baseline the migration promises,
    /// and the next post-merge hydration would then have nothing to compare.
    #[tokio::test]
    async fn a_durable_cursor_seeds_baselines_from_migrated_merged_pull_requests() {
        let previous = observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Merged).await;
        let polled = compact_cursor_observation(&previous, None, &[])
            .expect("fixture observation compacts canonically")
            .observation;
        assert!(polled.state().pull_requests().is_empty());

        let compacted = compact_cursor_observation(&polled, Some(&previous), &[])
            .expect("fixture observation compacts canonically");

        assert_eq!(compacted.merged_pull_request_baselines.len(), 1);
        assert_eq!(
            compacted.merged_pull_request_baselines[0].number(),
            previous.state().pull_requests()[0].context().number()
        );
        assert!(compacted.observation.state().pull_requests().is_empty());
    }

    #[tokio::test]
    async fn a_durable_cursor_retains_closed_unmerged_pull_request_details() {
        let observation =
            observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Closed).await;

        let compacted = compact_cursor_observation(&observation, None, &[])
            .expect("fixture observation compacts canonically");

        assert_eq!(
            compacted.observation.state().pull_requests(),
            observation.state().pull_requests()
        );
        assert!(compacted.merged_pull_request_baselines.is_empty());
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_mergeability() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.mergeable_state(), EXPECTED_MERGEABLE_STATE);
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_completed_check_suites() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(
            pull.completed_check_suites().len(),
            COMPLETED_CHECK_SUITE_IDS.len()
        );
        assert_eq!(
            pull.completed_check_suites()[0].outcome(),
            EXPECTED_CHECKS_OUTCOME
        );
    }

    #[test]
    fn neutral_check_suite_conclusion_is_success_like() {
        assert_eq!(
            normalize_checks_outcome(Some("neutral")),
            Ok(ChecksOutcome::Success)
        );
    }

    #[test]
    fn skipped_check_suite_conclusion_is_success_like() {
        assert_eq!(
            normalize_checks_outcome(Some("skipped")),
            Ok(ChecksOutcome::Success)
        );
    }

    #[tokio::test]
    async fn a_complete_poll_preserves_check_suite_completion_generation() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(
            pull.completed_check_suites()[0]
                .completion_generation()
                .as_str(),
            CHECK_SUITE_COMPLETION_GENERATION
        );
    }

    #[tokio::test]
    async fn a_complete_poll_enumerates_completed_check_runs_from_the_commit() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(
            pull.completed_check_runs().len(),
            COMPLETED_CHECK_RUN_IDS.len()
        );
        assert_eq!(
            pull.completed_check_runs()[0].name().as_str(),
            CHECK_RUN_NAME
        );
        assert_eq!(
            pull.completed_check_runs()[0].conclusion(),
            EXPECTED_CHECK_RUN_CONCLUSION
        );
    }

    #[tokio::test]
    async fn a_complete_poll_preserves_check_run_completion_generation() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(
            pull.completed_check_runs()[0]
                .completion_generation()
                .as_str(),
            CHECK_RUN_COMPLETION_GENERATION
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cycle_shorter_than_the_interval_waits_out_the_remainder() {
        let cycle_started = Instant::now();
        let completed = cycle_started + SHORT_CYCLE;

        let next_poll = next_cadence_deadline(cycle_started, POLL_INTERVAL, completed);

        assert_eq!(next_poll, cycle_started + POLL_INTERVAL);
        assert_eq!(next_poll - completed, SHORT_CYCLE_REMAINDER);
    }

    /// Following such a cycle immediately would leave the poll deadline elapsed
    /// on entry to every pass, so the task would never sleep and never reach
    /// the arms that drain durable webhook work.
    #[tokio::test(start_paused = true)]
    async fn a_cycle_that_reaches_the_interval_starts_a_fresh_interval() {
        let cycle_started = Instant::now();
        let completed = cycle_started + POLL_INTERVAL;

        assert_eq!(
            next_cadence_deadline(cycle_started, POLL_INTERVAL, completed),
            completed + POLL_INTERVAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cycle_that_overruns_the_interval_starts_a_fresh_interval() {
        let cycle_started = Instant::now();
        let completed = cycle_started + OVERRUNNING_CYCLE;

        let next_poll = next_cadence_deadline(cycle_started, POLL_INTERVAL, completed);

        assert_eq!(next_poll, completed + POLL_INTERVAL);
        assert!(next_poll > completed, "an elapsed deadline never sleeps");
    }

    #[test]
    fn reconciliation_yields_at_the_audited_quantum_only_with_a_continuation_wake() {
        let quantum = checked_in_example_configuration()
            .expect("checked-in example parses")
            .numeric_bounds()
            .integer("repository_reconciliation_quantum")
            .flatten()
            .and_then(|value| usize::try_from(value).ok())
            .expect("example reconciliation quantum fits usize");
        assert!(!repository_reconciliation_should_yield(
            quantum - 1,
            Some(quantum),
            true,
        ));
        assert!(repository_reconciliation_should_yield(
            quantum,
            Some(quantum),
            true,
        ));
        assert!(!repository_reconciliation_should_yield(
            quantum,
            Some(quantum),
            false,
        ));
    }

    #[test]
    fn a_first_repository_baseline_polls_immediately() {
        let now = Instant::now();

        assert_eq!(initial_poll_deadline(now, POLL_INTERVAL, None), now);
    }

    #[test]
    fn a_warm_restart_waits_for_the_repository_poll_cadence() {
        let now = Instant::now();

        assert_eq!(
            initial_poll_deadline(now, POLL_INTERVAL, Some(Duration::ZERO)),
            now + POLL_INTERVAL
        );
    }

    /// A restart resumes what the durable record says is left of the cadence.
    /// Anchoring on this process's start instead would hand every restart a
    /// fresh full interval.
    #[test]
    fn a_warm_restart_waits_only_for_the_remainder_of_the_cadence() {
        let now = Instant::now();
        let elapsed = POLL_INTERVAL / 4;

        assert_eq!(
            initial_poll_deadline(now, POLL_INTERVAL, Some(elapsed)),
            now + (POLL_INTERVAL - elapsed)
        );
    }

    /// Restarts more frequent than the interval must not postpone the
    /// authoritative sweep: once the durable record is older than the interval,
    /// the sweep is due no matter how recently this process started.
    #[test]
    fn a_restart_polls_immediately_once_the_recorded_sweep_is_older_than_the_interval() {
        let now = Instant::now();

        assert_eq!(
            initial_poll_deadline(now, POLL_INTERVAL, Some(POLL_INTERVAL)),
            now
        );
        assert_eq!(
            initial_poll_deadline(now, POLL_INTERVAL, Some(POLL_INTERVAL * 9)),
            now
        );
    }

    /// A bounded drain page re-arms its wake while durable remainder exists, so
    /// nothing in the preemption cycle itself ends it: sustained ingress would
    /// preempt every fresh pass and the complete poll would never commit.
    #[test]
    fn consecutive_webhook_preemptions_stop_starving_the_complete_poll() {
        assert_eq!(
            poll_webhook_interrupt(DrainRetryBackoff::Clear, 0),
            WebhookPollInterrupt::Enabled
        );
        assert_eq!(
            poll_webhook_interrupt(
                DrainRetryBackoff::Clear,
                MAX_CONSECUTIVE_POLL_PREEMPTIONS - 1
            ),
            WebhookPollInterrupt::Enabled
        );
        assert_eq!(
            poll_webhook_interrupt(DrainRetryBackoff::Clear, MAX_CONSECUTIVE_POLL_PREEMPTIONS),
            WebhookPollInterrupt::Suppressed
        );
        assert_eq!(
            poll_webhook_interrupt(
                DrainRetryBackoff::Clear,
                MAX_CONSECUTIVE_POLL_PREEMPTIONS + 1
            ),
            WebhookPollInterrupt::Suppressed
        );
    }

    /// An owed drain retry keeps its own deadline authoritative regardless of
    /// how many preemptions the still-due poll has left.
    #[test]
    fn a_backing_off_drain_retry_still_suppresses_admission_preemption() {
        assert_eq!(
            poll_webhook_interrupt(DrainRetryBackoff::InForce, 0),
            WebhookPollInterrupt::Suppressed
        );
    }

    /// The labelled axis is only worth its label if it reads the retry
    /// faithfully — including the case the axis exists to separate, a
    /// follow-up deadline, which is owed but is not backoff.
    #[tokio::test(start_paused = true)]
    async fn the_backoff_axis_reads_the_retry_it_is_taken_from() {
        let mut retry = WebhookDrainRetry::default();
        assert_eq!(DrainRetryBackoff::of(&retry), DrainRetryBackoff::Clear);

        retry.update_after(&drain_failure());
        assert_eq!(DrainRetryBackoff::of(&retry), DrainRetryBackoff::InForce);

        let mut following_up = WebhookDrainRetry::default();
        following_up.arm_follow_up(Instant::now());
        assert_eq!(
            DrainRetryBackoff::of(&following_up),
            DrainRetryBackoff::Clear
        );
    }

    /// The budget is counted in preemptions but chosen against the pages they
    /// cost, and the two are not the same number: a preempted pass drains the
    /// poll's own pre-poll page and then the page the admission wake's attempt
    /// runs, and the suppressed pass that ends the cycle drains one more. Keeping
    /// that conversion in a test is what stops the constant and the ceiling it
    /// was chosen for from drifting apart.
    #[test]
    fn the_preemption_budget_holds_the_sweep_within_its_page_ceiling() {
        const DRAINS_PER_PREEMPTED_PASS: u32 = 2;
        const SUPPRESSED_PASS_DRAINS: u32 = 1;
        const MAX_DELIVERIES_BEFORE_THE_SWEEP: u32 = 225;

        let pages =
            MAX_CONSECUTIVE_POLL_PREEMPTIONS * DRAINS_PER_PREEMPTED_PASS + SUPPRESSED_PASS_DRAINS;

        assert_eq!(pages, 9);
        assert_eq!(
            pages * u32::from(WEBHOOK_PENDING_PAGE_SIZE.get()),
            MAX_DELIVERIES_BEFORE_THE_SWEEP
        );
    }

    /// What the provider returns when a valid credential lacks permission on one
    /// endpoint: an error envelope naming no rate limit.
    const PERMISSION_REJECTION_BODY: &[u8] = br#"{"message":"Resource not accessible by personal access token","documentation_url":"https://docs.github.com/rest"}"#;
    /// What it returns for the secondary limit that carries neither rate-limit
    /// header, where the message is the only signal.
    const SECONDARY_RATE_LIMIT_BODY: &[u8] = br#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#;

    /// A credential that lacks permission on one endpoint answers `403` with no
    /// rate-limit signal of any kind. Treating that as a provider outage stops
    /// the drain page at the same oldest receipt on every retry, so every later
    /// delivery behind it — payload-only and ignored ones included — is never
    /// attempted.
    #[test]
    fn a_permission_scoped_rejection_defers_only_its_own_receipt() {
        let error = rejected_response_error(
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            PERMISSION_REJECTION_BODY,
        );

        assert_eq!(error, RepositoryWatchAttemptError::Rejected);
        assert!(!error.stops_webhook_page());
    }

    /// The provider answers a `403` with its quota intact when the rejection is
    /// about permission rather than rate, so an unexhausted counter is no more a
    /// throttle than an absent one.
    #[test]
    fn a_rejection_with_quota_remaining_defers_only_its_own_receipt() {
        let mut remaining = HeaderMap::new();
        remaining.insert("x-ratelimit-remaining", HeaderValue::from_static("4999"));

        assert_eq!(
            rejected_response_error(StatusCode::FORBIDDEN, &remaining, PERMISSION_REJECTION_BODY),
            RepositoryWatchAttemptError::Rejected
        );
    }

    /// The provider documents three ordered signals for its own rate limits, and
    /// each one has to stop the page: a secondary limit's `Retry-After`, a
    /// primary limit's exhausted counter, and — the case neither header covers —
    /// a secondary limit named only by the rejection's own message.
    #[test]
    fn a_throttled_rejection_stops_the_whole_page() {
        let mut retry_after = HeaderMap::new();
        retry_after.insert(RETRY_AFTER, HeaderValue::from_static("60"));
        let mut exhausted = HeaderMap::new();
        exhausted.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        let mut unexhausted = HeaderMap::new();
        unexhausted.insert("x-ratelimit-remaining", HeaderValue::from_static("4999"));

        let signalled_by_header = rejected_response_error(
            StatusCode::FORBIDDEN,
            &retry_after,
            PERMISSION_REJECTION_BODY,
        );
        let signalled_by_quota =
            rejected_response_error(StatusCode::FORBIDDEN, &exhausted, PERMISSION_REJECTION_BODY);
        let signalled_by_message = rejected_response_error(
            StatusCode::FORBIDDEN,
            &unexhausted,
            SECONDARY_RATE_LIMIT_BODY,
        );

        assert_eq!(
            signalled_by_header,
            RepositoryWatchAttemptError::ProviderUnavailable
        );
        assert_eq!(
            signalled_by_quota,
            RepositoryWatchAttemptError::ProviderUnavailable
        );
        assert_eq!(
            signalled_by_message,
            RepositoryWatchAttemptError::ProviderUnavailable
        );
        assert!(signalled_by_message.stops_webhook_page());
    }

    /// Only the `403` carrying neither header leaves anything for the message to
    /// decide. Reading the others would hold the serialized repository task to
    /// the request timeout on a stalled rejection — during the very outage that
    /// produces them — for a classification the status already fixed.
    #[test]
    fn only_a_headerless_forbidden_rejection_is_read_before_it_is_classified() {
        let mut retry_after = HeaderMap::new();
        retry_after.insert(RETRY_AFTER, HeaderValue::from_static("60"));
        let mut exhausted = HeaderMap::new();
        exhausted.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        let mut unexhausted = HeaderMap::new();
        unexhausted.insert("x-ratelimit-remaining", HeaderValue::from_static("4999"));

        assert!(rejection_needs_its_message(
            StatusCode::FORBIDDEN,
            &HeaderMap::new()
        ));
        assert!(rejection_needs_its_message(
            StatusCode::FORBIDDEN,
            &unexhausted
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::FORBIDDEN,
            &retry_after
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::FORBIDDEN,
            &exhausted
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new()
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::SERVICE_UNAVAILABLE,
            &HeaderMap::new()
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::UNAUTHORIZED,
            &HeaderMap::new()
        ));
        assert!(!rejection_needs_its_message(
            StatusCode::NOT_FOUND,
            &HeaderMap::new()
        ));
    }

    /// The provider's older spelling for the same secondary limit still reaches
    /// live responses, so it stops the page on the same terms.
    #[test]
    fn the_legacy_secondary_limit_message_stops_the_whole_page() {
        assert_eq!(
            rejected_response_error(
                StatusCode::FORBIDDEN,
                &HeaderMap::new(),
                br#"{"message":"You have triggered an abuse detection mechanism."}"#,
            ),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
    }

    /// A rejection whose body is absent or unparseable is not evidence of
    /// throttling, so it classifies from the status and headers alone.
    #[test]
    fn an_unreadable_rejection_body_is_not_read_as_a_throttle() {
        assert_eq!(
            rejected_response_error(StatusCode::FORBIDDEN, &HeaderMap::new(), b""),
            RepositoryWatchAttemptError::Rejected
        );
        assert_eq!(
            rejected_response_error(
                StatusCode::FORBIDDEN,
                &HeaderMap::new(),
                b"<html>429</html>"
            ),
            RepositoryWatchAttemptError::Rejected
        );
    }

    #[test]
    fn a_too_many_requests_response_stops_the_whole_page() {
        assert_eq!(
            rejected_response_error(
                StatusCode::TOO_MANY_REQUESTS,
                &HeaderMap::new(),
                PERMISSION_REJECTION_BODY
            ),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
    }

    #[test]
    fn a_provider_server_error_stops_the_whole_page() {
        assert_eq!(
            rejected_response_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &HeaderMap::new(),
                PERMISSION_REJECTION_BODY
            ),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
    }

    #[test]
    fn an_unauthorized_response_reports_a_credential_failure() {
        assert_eq!(
            rejected_response_error(
                StatusCode::UNAUTHORIZED,
                &HeaderMap::new(),
                PERMISSION_REJECTION_BODY
            ),
            RepositoryWatchAttemptError::Credential
        );
    }

    #[test]
    fn an_ordinary_rejection_defers_only_its_own_receipt() {
        let error = rejected_response_error(
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            PERMISSION_REJECTION_BODY,
        );

        assert_eq!(error, RepositoryWatchAttemptError::Rejected);
        assert!(!error.stops_webhook_page());
    }

    /// The request count and the wire budget are the attempt's, and the drain
    /// page runs inside one attempt, so a spent budget refuses every later
    /// hydration on the page. Continuing spends transfer the ledger can no
    /// longer account for to learn the same failure once per receipt.
    #[test]
    fn an_exhausted_attempt_budget_stops_the_whole_page() {
        assert!(RepositoryWatchAttemptError::ResourceLimit.stops_webhook_page());
    }

    /// The size ceiling beside it is one response's, not the attempt's: a peer's
    /// hydration can still fit under it, so page isolation holds.
    #[test]
    fn an_oversized_response_defers_only_its_own_receipt() {
        assert!(!RepositoryWatchAttemptError::ResponseTooLarge.stops_webhook_page());
    }

    /// Throttling and provider outage arrive as an `HTTP 200` GraphQL error
    /// envelope. Classified as a target-specific rejection, thread hydration
    /// re-issues the identical doomed request for every later delivery on the
    /// page — the amplification the page-stopping predicate exists to prevent.
    #[test]
    fn a_repository_wide_graphql_error_stops_the_whole_page() {
        let error = graphql_envelope_error(&[graphql_error("RATE_LIMITED")]);

        assert_eq!(error, RepositoryWatchAttemptError::ProviderUnavailable);
        assert!(error.stops_webhook_page());
        assert_eq!(
            graphql_envelope_error(&[graphql_error("SERVICE_UNAVAILABLE")]),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
        assert_eq!(
            graphql_envelope_error(&[graphql_error("NOT_FOUND"), graphql_error("RATE_LIMITED")]),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
    }

    /// The provider spells one taxonomy across two carriers, choosing by which
    /// layer rejected the query: reading only `type` leaves an `extensions.code`
    /// throttle classified as target-specific, and thread hydration re-issues it
    /// for every later delivery on the page.
    #[test]
    fn a_repository_wide_graphql_error_is_read_from_either_carrier() {
        let error = graphql_envelope_error(&[graphql_error_extension("RATE_LIMITED")]);

        assert_eq!(error, RepositoryWatchAttemptError::ProviderUnavailable);
        assert!(error.stops_webhook_page());
        assert_eq!(
            graphql_envelope_error(&[graphql_error_extension("undefinedField")]),
            RepositoryWatchAttemptError::Rejected
        );
    }

    /// The classifier reads the provider's envelope, not a hand-built value, so
    /// the carrier has to survive deserialization to be read at all.
    #[test]
    fn the_extension_carrier_survives_the_wire() {
        const THROTTLED_ENVELOPE: &str = r#"{"data":null,"errors":[{"message":"API rate limit exceeded","extensions":{"code":"RATE_LIMITED"}}]}"#;

        let envelope: GraphQlEnvelope<serde_json::Value> =
            serde_json::from_str(THROTTLED_ENVELOPE).expect("the throttled envelope parses");

        assert_eq!(
            graphql_envelope_error(&envelope.errors),
            RepositoryWatchAttemptError::ProviderUnavailable
        );
    }

    /// A query-scoped failure is not evidence that a peer's request cannot make
    /// independent progress, so it defers only its own receipt.
    #[test]
    fn a_query_scoped_graphql_error_defers_only_its_own_receipt() {
        let error = graphql_envelope_error(&[graphql_error("NOT_FOUND"), untyped_graphql_error()]);

        assert_eq!(error, RepositoryWatchAttemptError::Rejected);
        assert!(!error.stops_webhook_page());
        assert_eq!(
            graphql_envelope_error(&[untyped_graphql_error()]),
            RepositoryWatchAttemptError::Rejected
        );
        assert_eq!(
            graphql_envelope_error(&[graphql_error("INTERNAL")]),
            RepositoryWatchAttemptError::Rejected
        );
    }

    fn graphql_error(error_type: &str) -> GraphQlError {
        GraphQlError {
            error_type: Some(error_type.to_owned()),
            extensions: None,
        }
    }

    fn graphql_error_extension(code: &str) -> GraphQlError {
        GraphQlError {
            error_type: None,
            extensions: Some(GraphQlErrorExtensions {
                code: Some(code.to_owned()),
            }),
        }
    }

    fn untyped_graphql_error() -> GraphQlError {
        GraphQlError {
            error_type: None,
            extensions: None,
        }
    }

    #[tokio::test]
    async fn an_unchanged_pull_request_refetches_dispatch_signals() {
        let responses = settled_typed_observation_responses()
            .into_iter()
            .chain(skipped_pull_request_responses())
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("second poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn a_reused_pull_request_emits_every_deferred_review_once() {
        let responses = settled_typed_observation_responses()
            .into_iter()
            .chain(skipped_pull_request_responses_with_deferred_reviews())
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let previous = fixture
            .poller
            .poll(None)
            .await
            .expect("the prior cursor observation is fetched");
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        let current = fixture
            .poller
            .poll(Some(&previous))
            .await
            .expect("the deferred remote state is fetched");
        server.finish().await;
        let mut event_identity_frontier = RepoWatchEventIdentityFrontierV1::default();
        let events = derive_repo_watch_events(
            &fixture.poller.repository,
            Some(&previous),
            &current,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .expect("the deferred review wave forms events");
        let deferred_reviews =
            &current.state().pull_requests()[0].reviews()[RETAINED_REVIEW_IDS.len()..];

        assert_eq!(events.len(), deferred_reviews.len());
        assert_eq!(
            events[0].event().kind(),
            &RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: deferred_reviews[0].reviewer().clone(),
                state: deferred_reviews[0]
                    .state()
                    .expect("fixture review is submitted"),
                commit: deferred_reviews[0].commit().clone(),
            }
        );
        assert_eq!(
            events[1].event().kind(),
            &RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: deferred_reviews[1].reviewer().clone(),
                state: deferred_reviews[1]
                    .state()
                    .expect("fixture review is submitted"),
                commit: deferred_reviews[1].commit().clone(),
            }
        );
        assert_eq!(
            events[2].event().kind(),
            &RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: deferred_reviews[2].reviewer().clone(),
                state: deferred_reviews[2]
                    .state()
                    .expect("fixture review is submitted"),
                commit: deferred_reviews[2].commit().clone(),
            }
        );
    }

    #[tokio::test]
    async fn a_pull_request_with_pending_mergeability_is_refetched() {
        let responses = settled_responses_with_pending_mergeability()
            .into_iter()
            .chain(revalidated(settled_responses_with_pending_mergeability()))
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("second poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn a_pull_request_fetch_that_never_committed_is_refetched() {
        let responses = settled_typed_observation_responses()
            .into_iter()
            .chain(revalidated(settled_typed_observation_responses()))
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("second poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn an_unchanged_pull_request_with_an_unsettled_check_suite_is_refetched() {
        let responses = responses_with_only_an_unsettled_check_suite()
            .into_iter()
            .chain(revalidated(responses_with_only_an_unsettled_check_suite()))
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("second poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn an_unchanged_pull_request_with_an_unsettled_check_run_is_refetched() {
        let responses = responses_with_only_an_unsettled_check_run()
            .into_iter()
            .chain(revalidated(responses_with_only_an_unsettled_check_run()))
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let first = fixture
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        let second = fixture
            .poller
            .poll(Some(&first))
            .await
            .expect("second poll succeeds");
        server.finish().await;

        assert_eq!(second, first);
    }

    async fn concurrently_fetched_pull_requests() -> (Vec<u64>, usize) {
        let numbers: BTreeSet<u64> = CONCURRENT_FETCH_PULL_NUMBERS.collect();
        let responses = numbers
            .iter()
            .flat_map(|number| minimal_pull_responses(*number))
            .collect();
        let server = ConcurrentScriptedServer::start(responses).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let listed: BTreeMap<u64, ListedPullRequest> = numbers
            .iter()
            .map(|number| {
                (
                    *number,
                    ListedPullRequest {
                        updated_at: PULL_UPDATED_AT.to_owned(),
                        head_sha: CommitSha::try_new(minimal_pull_head_sha(*number))
                            .expect("fixture listed head is valid"),
                    },
                )
            })
            .collect();

        let pull_requests = fixture
            .poller
            .fetch_pull_requests(
                numbers,
                &listed,
                None,
                Some(RepoWatchCursorGeneration::INITIAL),
                &[base_branch_head()],
            )
            .await
            .expect("every open pull request is fetched");
        let peak_in_flight = server.finish().await;

        (
            pull_requests
                .states
                .iter()
                .map(|pull_request| pull_request.context().number().get())
                .collect(),
            peak_in_flight,
        )
    }

    #[tokio::test]
    async fn concurrent_pull_request_fetches_stay_within_their_bound() {
        let (_, peak_in_flight) = concurrently_fetched_pull_requests().await;

        assert!(peak_in_flight > 1);
        assert!(peak_in_flight <= MAX_CONCURRENT_PULL_REQUEST_FETCHES);
    }

    #[tokio::test]
    async fn concurrently_fetched_pull_requests_keep_ascending_number_order() {
        let (fetched, _) = concurrently_fetched_pull_requests().await;

        assert_eq!(fetched, CONCURRENT_FETCH_PULL_NUMBERS.collect::<Vec<u64>>());
    }

    /// Cancelling an attempt drops the fetch future, which aborts the spawned
    /// children without joining them. Whether a child is still running is read
    /// from the poller's reference count: the child parked in the delayed
    /// response owns a clone, so the count stays raised until a join actually
    /// retires it.
    #[tokio::test]
    async fn draining_after_a_cancelled_fetch_joins_every_child() {
        let server = ConcurrentScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(format!(
                    "/repos/{WATCHED_REPOSITORY}/pulls/{CANCELLED_FETCH_PULL_NUMBER}"
                )),
                ResponseBody(minimal_pull_detail(CANCELLED_FETCH_PULL_NUMBER)),
            )
            .delayed(CANCELLED_FETCH_DELAY),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let poller = Arc::clone(&fixture.poller);
        let listed = BTreeMap::from([(
            CANCELLED_FETCH_PULL_NUMBER,
            ListedPullRequest {
                updated_at: PULL_UPDATED_AT.to_owned(),
                head_sha: CommitSha::try_new(minimal_pull_head_sha(CANCELLED_FETCH_PULL_NUMBER))
                    .expect("fixture listed head is valid"),
            },
        )]);
        let fetch = tokio::spawn(async move {
            poller
                .fetch_pull_requests(
                    BTreeSet::from([CANCELLED_FETCH_PULL_NUMBER]),
                    &listed,
                    None,
                    Some(RepoWatchCursorGeneration::INITIAL),
                    &[base_branch_head()],
                )
                .await
        });
        server.request_in_flight().await;

        fetch.abort();
        let cancelled = fetch.await;

        assert!(
            cancelled
                .expect_err("the mid-request fetch must not have completed")
                .is_cancelled()
        );
        assert_eq!(
            Arc::strong_count(&fixture.poller),
            2,
            "the aborted attempt leaves its child running"
        );
        fixture.poller.drain_fetches().await;
        assert_eq!(
            Arc::strong_count(&fixture.poller),
            1,
            "a drained poller has no child still holding it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_fetch_cleanup_returns_at_its_deadline() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        {
            let mut fetches = fixture.poller.fetches.lock().await;
            fetches.spawn_blocking(move || {
                started_sender
                    .send(())
                    .expect("the fixture awaits the blocking child");
                release_receiver
                    .blocking_recv()
                    .expect("the fixture releases the blocking child");
                Err(RepositoryWatchAttemptError::Persistence)
            });
        }
        started_receiver
            .await
            .expect("the blocking child reports readiness");

        let timed_out = fixture
            .poller
            .drain_fetches_within(Duration::from_millis(10))
            .await;

        assert!(!timed_out);
        release_sender
            .send(())
            .expect("the blocking child still awaits release");
        assert!(
            fixture
                .poller
                .drain_fetches_within(Duration::from_secs(1))
                .await
        );
    }

    /// After a commit conflict the durable baseline belongs to a competing
    /// watcher, so entries recorded and published against the superseded
    /// baseline must authorize no further reuse.
    #[tokio::test]
    async fn invalidated_freshness_authorizes_no_reuse() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let observation = complete_typed_observation().await;
        let previous = &observation.state().pull_requests()[0];
        let listed = listed_pull_request(HEAD_SHA);
        let number = previous.context().number().get();
        fixture.poller.record_fetched_pull_request(
            number,
            &listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        assert!(
            fixture.poller.pull_request_detail_is_reusable(
                number,
                &listed,
                previous,
                Some(RepoWatchCursorGeneration::INITIAL),
            ),
            "a published settled entry authorizes reuse against its cursor generation"
        );
        fixture.poller.invalidate_freshness();

        assert!(
            !fixture.poller.pull_request_detail_is_reusable(
                number,
                &listed,
                previous,
                Some(RepoWatchCursorGeneration::INITIAL),
            ),
            "an invalidated record authorizes nothing"
        );
    }

    /// A targeted refresh whose cursor commit loses its generation race never
    /// became cursor state, so it must leave nothing behind that a later commit
    /// could vouch for. Its fetch already recorded unpublished freshness, and
    /// `publish_freshness` stamps every entry it finds: keeping those would let
    /// the next targeted commit relabel this fetch as belonging to a cursor it
    /// never reached, and a following poll would reuse detail that cursor does
    /// not carry.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_superseded_targeted_commit_clears_the_freshness_it_recorded()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let mut fixture = webhook_task(&pool).await?;

        // What a completed targeted fetch leaves behind: recorded detail that
        // no cursor has published yet.
        let observation = complete_typed_observation().await;
        let listed = listed_pull_request(HEAD_SHA);
        let number = observation.state().pull_requests()[0]
            .context()
            .number()
            .get();
        fixture.task.poller.record_fetched_pull_request(
            number,
            &listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );

        // A generation the durable cursor has not reached, so this commit loses
        // its race exactly as a competing watcher's advance would make it.
        let unreached = RepoWatchCursorGeneration::INITIAL
            .next()
            .expect("fixture cursor generation has a successor");
        let pull_request = PullRequestNumber::new(
            NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
        );
        let prepared = PreparedTargetedRefresh {
            generation: unreached,
            candidate: RepoWatchCursorCandidate::new(review_only_blocked_observation().await),
            events: Vec::new(),
            queried: vec![RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request }],
            targeted_pull_requests: vec![pull_request],
        };

        let settlement = fixture
            .task
            .complete_targeted_webhook_projection(
                prepared,
                webhook_delivery_key(FIRST_WEBHOOK_DELIVERY),
                Vec::new(),
                WebhookShadowBaseline {
                    observation,
                    identity_frontier: RepoWatchEventIdentityFrontierV1::default(),
                    merged_pull_request_baselines: Vec::new(),
                },
            )
            .await
            .expect("a lost generation race settles rather than failing");

        assert_eq!(
            settlement,
            TargetedRefreshSettlement::Superseded,
            "a commit that lost its generation race never reached the cursor"
        );
        assert!(
            fixture.task.poller.freshness().is_empty(),
            "a fetch that never reached the cursor authorizes no later reuse"
        );
        Ok(())
    }

    /// A shadow-mode targeted refresh shares the primary path's completion
    /// helper but is not the repository's promotion: it reconciles through the
    /// poller's own credential, so the rows it writes are poll-produced and its
    /// projections are exactly what parity compares against them. Recording it
    /// as committed would set the repository's `primary_start` and permanently
    /// drop every later poll event from the measurement it belongs to, in a
    /// deployment that never entered primary mode.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_shadow_targeted_refresh_records_a_projected_disposition()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
        let admission = submitted_review_admission(FIRST_WEBHOOK_DELIVERY, FIRST_WEBHOOK_REVIEW)?;
        webhook_store.admit(&admission).await?;
        let mut fixture = webhook_task(&pool).await?;
        assert!(
            !fixture.task.webhook_primary,
            "the fixture stays in shadow mode"
        );

        let observation = complete_typed_observation().await;
        let pull_request = PullRequestNumber::new(
            NonZeroU64::new(PULL_NUMBER).expect("fixture pull-request number is positive"),
        );
        let refreshed = compact_cursor_observation(
            &observation_with_pull_lifecycle(RepoWatchPullRequestLifecycle::Merged).await,
            None,
            &[],
        )
        .expect("fixture observation compacts canonically");
        let expected_shadow_observation = refreshed.observation.clone();
        let expected_shadow_baselines = refreshed.merged_pull_request_baselines.clone();
        let prepared = PreparedTargetedRefresh {
            generation: RepoWatchCursorGeneration::INITIAL,
            candidate:
                RepoWatchCursorCandidate::try_with_event_identity_frontier_and_merged_baselines(
                    refreshed.observation,
                    RepoWatchEventIdentityFrontierV1::default(),
                    refreshed.merged_pull_request_baselines,
                )?,
            events: Vec::new(),
            queried: vec![RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request }],
            targeted_pull_requests: vec![pull_request],
        };

        let settlement = fixture
            .task
            .complete_targeted_webhook_projection(
                prepared,
                admission.key(),
                Vec::new(),
                WebhookShadowBaseline {
                    observation,
                    identity_frontier: RepoWatchEventIdentityFrontierV1::default(),
                    merged_pull_request_baselines: Vec::new(),
                },
            )
            .await
            .expect("the targeted commit settles");

        assert_eq!(
            settlement,
            TargetedRefreshSettlement::Landed,
            "the fixture cursor is at the generation this commit expects"
        );
        let disposition = webhook_store
            .load_disposition(admission.key())
            .await?
            .expect("a targeted refresh reaches a terminal disposition");
        assert_eq!(
            disposition.disposition(),
            RepoWatchWebhookDisposition::Projected,
            "a shadow targeted refresh is not the repository's first primary commit"
        );
        let carried_shadow = fixture
            .task
            .webhook_shadow
            .as_ref()
            .expect("a landed targeted refresh advances the shadow");
        assert_eq!(carried_shadow.observation, expected_shadow_observation);
        assert_eq!(
            carried_shadow.merged_pull_request_baselines,
            expected_shadow_baselines
        );
        Ok(())
    }

    #[tokio::test]
    async fn freshness_published_against_another_cursor_authorizes_no_reuse() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let observation = complete_typed_observation().await;
        let previous = &observation.state().pull_requests()[0];
        let listed = listed_pull_request(HEAD_SHA);
        let number = previous.context().number().get();
        let published_generation = RepoWatchCursorGeneration::INITIAL;
        let loaded_generation = published_generation
            .next()
            .expect("fixture cursor generation has a successor");
        fixture.poller.record_fetched_pull_request(
            number,
            &listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture.poller.publish_freshness(published_generation);

        assert!(
            !fixture.poller.pull_request_detail_is_reusable(
                number,
                &listed,
                previous,
                Some(loaded_generation),
            ),
            "freshness published against another durable cursor must not authorize reuse"
        );
    }

    #[tokio::test]
    async fn a_new_gating_context_requires_another_committed_poll_to_quiesce() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let listed = listed_pull_request(HEAD_SHA);
        let generation = RepoWatchCursorGeneration::INITIAL;
        fixture.poller.record_fetched_pull_request(
            PULL_NUMBER,
            &listed,
            PullRequestSettlement::Settled,
            vec![String::from(CHECK_RUN_NAME)],
        );
        fixture.poller.publish_freshness(generation);
        let expanded_inventory = vec![
            String::from(CHECK_RUN_NAME),
            String::from("later gating check"),
        ];

        assert!(!fixture.poller.gating_check_inventory_quiesced(
            PULL_NUMBER,
            &listed,
            Some(generation),
            &expanded_inventory,
        ));
        fixture.poller.record_gating_check_inventory(
            PULL_NUMBER,
            &listed,
            expanded_inventory.clone(),
        );
        fixture.poller.publish_freshness(generation);
        assert!(fixture.poller.gating_check_inventory_quiesced(
            PULL_NUMBER,
            &listed,
            Some(generation),
            &expanded_inventory,
        ));
    }

    #[tokio::test]
    async fn changed_pull_request_timestamp_authorizes_no_reuse() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let observation = complete_typed_observation().await;
        let previous = &observation.state().pull_requests()[0];
        let listed = listed_pull_request(HEAD_SHA);
        let changed_listing = ListedPullRequest {
            updated_at: String::from(CHANGED_PULL_UPDATED_AT),
            head_sha: listed.head_sha.clone(),
        };
        let number = previous.context().number().get();
        fixture.poller.record_fetched_pull_request(
            number,
            &listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);

        assert!(!fixture.poller.pull_request_detail_is_reusable(
            number,
            &changed_listing,
            previous,
            Some(RepoWatchCursorGeneration::INITIAL),
        ));
    }

    #[tokio::test]
    async fn pull_request_reuse_stops_at_the_skipped_poll_limit() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let observation = complete_typed_observation().await;
        let previous = &observation.state().pull_requests()[0];
        let listed = listed_pull_request(HEAD_SHA);
        let number = previous.context().number().get();
        fixture.poller.record_fetched_pull_request(
            number,
            &listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);
        fixture
            .poller
            .freshness()
            .get_mut(&number)
            .expect("fixture freshness is recorded")
            .skipped_polls = MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS - 1;

        assert!(fixture.poller.pull_request_detail_is_reusable(
            number,
            &listed,
            previous,
            Some(RepoWatchCursorGeneration::INITIAL),
        ));
        fixture.poller.record_skipped_poll(number);
        assert!(!fixture.poller.pull_request_detail_is_reusable(
            number,
            &listed,
            previous,
            Some(RepoWatchCursorGeneration::INITIAL),
        ));
        fixture.poller.record_skipped_poll(number);
        assert!(!fixture.poller.pull_request_detail_is_reusable(
            number,
            &listed,
            previous,
            Some(RepoWatchCursorGeneration::INITIAL),
        ));
    }

    #[tokio::test]
    async fn a_listed_head_change_forbids_pull_request_reuse() {
        let fixture = poller_fixture(
            Url::parse("http://provider.invalid/").expect("fixture base forms a URL"),
        )
        .expect("poller is constructed");
        let observation = complete_typed_observation().await;
        let previous = &observation.state().pull_requests()[0];
        let previously_listed = listed_pull_request(HEAD_SHA);
        let changed_listing = listed_pull_request(CHANGED_LISTED_HEAD_SHA);
        let number = previous.context().number().get();
        fixture.poller.record_fetched_pull_request(
            number,
            &previously_listed,
            PullRequestSettlement::Settled,
            Vec::new(),
        );
        fixture
            .poller
            .publish_freshness(RepoWatchCursorGeneration::INITIAL);

        assert!(
            !fixture.poller.pull_request_detail_is_reusable(
                number,
                &changed_listing,
                previous,
                Some(RepoWatchCursorGeneration::INITIAL),
            ),
            "a head change is never hidden by unchanged listing metadata"
        );
    }

    #[tokio::test]
    async fn a_base_advance_forbids_pull_request_reuse() {
        let observation = complete_typed_observation().await;
        let pull_request = &observation.state().pull_requests()[0];
        let advanced_base = RepoWatchBranchHead::new(
            pull_request.context().base_branch().clone(),
            CommitSha::try_new(CHANGED_LISTED_HEAD_SHA.to_owned())
                .expect("changed fixture base revision is canonical"),
        );

        assert!(!super::pull_request_base_revision_matches(
            &observation,
            pull_request,
            &[advanced_base],
        ));
    }

    #[tokio::test]
    async fn every_check_run_member_the_decoder_requires_exists_in_the_provider_payload() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(provider_defined_check_runs()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let head = CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head is valid");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let (runs, _) = fixture
            .poller
            .fetch_check_runs(&head, std::slice::from_ref(&suite))
            .await
            .expect("a page carrying the provider's complete check-run member set must decode");
        server.finish().await;

        assert_eq!(runs.len(), COMPLETED_CHECK_RUN_IDS.len());
        assert_eq!(
            runs[0].completion_generation().as_str(),
            CHECK_RUN_COMPLETION_GENERATION
        );
    }

    #[tokio::test]
    async fn a_completed_check_run_without_a_completion_time_is_an_invalid_response() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(completed_check_run_without_a_completion_time()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let head = CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head is valid");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let result = fixture
            .poller
            .fetch_check_runs(&head, std::slice::from_ref(&suite))
            .await;
        server.finish().await;

        assert_eq!(result, Err(RepositoryWatchAttemptError::InvalidResponse));
    }

    #[test]
    fn commit_check_run_search_is_used_only_for_a_provably_complete_nonempty_inventory() {
        assert!(!commit_check_run_search_is_complete(0));
        assert!(commit_check_run_search_is_complete(
            MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH
        ));
        assert!(!commit_check_run_search_is_complete(
            MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH + 1
        ));
    }

    #[tokio::test]
    async fn an_unfinished_report_only_run_does_not_unsettle_gating_checks() {
        let response = check_runs().replace(IN_PROGRESS_CHECK_RUN_NAME, "coverage (report only)");
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(COMMIT_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(response),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let head = CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture head is valid");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let (_, every_gating_run_completed) = fixture
            .poller
            .fetch_check_runs(&head, std::slice::from_ref(&suite))
            .await
            .expect("report-only run is valid check evidence");
        server.finish().await;

        assert!(every_gating_run_completed);
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_submitted_reviews() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.reviews()[0].reviewer().as_str(), REVIEWER);
        assert_eq!(pull.reviews()[0].state(), EXPECTED_REVIEW_STATE);
    }

    #[tokio::test]
    async fn a_complete_poll_retains_dismissed_review_identity_without_a_payload() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.reviews()[1].state(), EXPECTED_DISMISSED_REVIEW_STATE);
    }

    #[tokio::test]
    async fn a_complete_poll_excludes_unsubmitted_pending_reviews() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.reviews().len(), RETAINED_REVIEW_IDS.len());
    }

    #[tokio::test]
    async fn a_deleted_review_author_reuses_the_prior_review_identity() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(REVIEWS_TARGET.to_owned()),
            ResponseBody(identity_less_review(RETAINED_REVIEW_IDS[0])),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let previous = submitted_review(RETAINED_REVIEW_IDS[0]);

        let reviews = fixture
            .poller
            .fetch_reviews(PULL_NUMBER, Some(std::slice::from_ref(&previous)))
            .await
            .expect("identity-less historical review is retained");
        server.finish().await;

        assert_eq!(reviews[0].reviewer(), previous.reviewer());
    }

    #[tokio::test]
    async fn a_new_review_without_an_author_identity_is_omitted() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(REVIEWS_TARGET.to_owned()),
            ResponseBody(identity_less_review(RETAINED_REVIEW_IDS[0])),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let reviews = fixture
            .poller
            .fetch_reviews(PULL_NUMBER, None)
            .await
            .expect("identity-less new review is safely omitted");
        server.finish().await;

        assert!(reviews.is_empty());
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_review_threads() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.threads().len(), REVIEW_THREADS.len());
        assert_eq!(pull.threads()[0].thread().as_str(), REVIEW_THREAD);
        assert_eq!(pull.threads()[0].state(), EXPECTED_OPEN_THREAD_STATE);
        assert_eq!(pull.threads()[1].thread().as_str(), RESOLVED_REVIEW_THREAD);
        assert_eq!(pull.threads()[1].state(), EXPECTED_RESOLVED_THREAD_STATE);
    }

    #[tokio::test]
    async fn a_complete_poll_filters_reactions_to_signal_reviewers() {
        let observation = complete_typed_observation().await;
        let pull = &observation.state().pull_requests()[0];

        assert_eq!(pull.reactions().len(), SIGNAL_REACTION_CONTENTS.len());
        assert_eq!(pull.reactions()[0].reactor().as_str(), REVIEWER);
    }

    #[tokio::test]
    async fn no_signal_reviewers_skip_every_reaction_request() {
        let server = ScriptedServer::start(Vec::new()).await;
        let fixture = poller_fixture_with_signal_reviewers(server.base_url.clone(), Vec::new())
            .expect("poller is constructed");

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER, None)
            .await
            .expect("empty signal-reviewer policy needs no reaction request");
        server.finish().await;

        assert!(reactions.is_empty());
    }

    #[tokio::test]
    async fn a_reaction_without_an_actor_identity_is_omitted() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(identity_less_reaction()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER, None)
            .await
            .expect("identity-less reaction is safely omitted");
        server.finish().await;

        assert!(reactions.is_empty());
    }

    #[tokio::test]
    async fn a_reaction_without_an_actor_identity_retains_prior_subject_reactions() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(identity_less_reaction()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
        ])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let previous = RepoWatchReactionObservation::new(
            ReactionSubject::PullRequestBody,
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
                .expect("reviewer fixture is valid"),
            ReactionContent::try_new(String::from(SIGNAL_REACTION_CONTENTS[0]))
                .expect("reaction fixture is valid"),
        );

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER, Some(std::slice::from_ref(&previous)))
            .await
            .expect("identity-less reaction preserves the prior subject baseline");
        server.finish().await;

        assert_eq!(reactions, [previous]);
    }

    #[tokio::test]
    async fn a_changed_signal_reviewer_filter_drops_identity_less_prior_reactions() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(
                RequestTarget(PULL_REACTIONS_TARGET.to_owned()),
                ResponseBody(identity_less_reaction()),
            ),
            ScriptedResponse::ok(
                RequestTarget(ISSUE_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
            ScriptedResponse::ok(
                RequestTarget(REVIEW_COMMENTS_TARGET.to_owned()),
                ResponseBody(EMPTY_LIST.to_owned()),
            ),
        ])
        .await;
        let current_reviewer = RepoWatchAuthorLogin::try_new(String::from(AMBIENT_REACTOR))
            .expect("current reviewer fixture is valid");
        let fixture =
            poller_fixture_with_signal_reviewers(server.base_url.clone(), vec![current_reviewer])
                .expect("poller is constructed");
        let previous = RepoWatchReactionObservation::new(
            ReactionSubject::PullRequestBody,
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))
                .expect("previous reviewer fixture is valid"),
            ReactionContent::try_new(String::from(SIGNAL_REACTION_CONTENTS[0]))
                .expect("reaction fixture is valid"),
        );

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER, Some(std::slice::from_ref(&previous)))
            .await
            .expect("filter changes discard identity-less prior reactions");
        server.finish().await;

        assert!(reactions.is_empty());
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_reaction_subjects() {
        let observation = complete_typed_observation().await;
        let reactions = observation.state().pull_requests()[0].reactions();

        assert_eq!(reactions[0].subject(), ReactionSubject::PullRequestBody);
        assert_eq!(reactions[1].subject(), issue_comment_subject());
        assert_eq!(reactions[2].subject(), review_comment_subject());
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_reaction_content() {
        let observation = complete_typed_observation().await;
        let reactions = observation.state().pull_requests()[0].reactions();

        assert_eq!(reactions[0].content().as_str(), SIGNAL_REACTION_CONTENTS[0]);
        assert_eq!(reactions[1].content().as_str(), SIGNAL_REACTION_CONTENTS[1]);
        assert_eq!(reactions[2].content().as_str(), SIGNAL_REACTION_CONTENTS[2]);
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_branch_heads() {
        let observation = complete_typed_observation().await;
        let state = observation.state();

        assert_eq!(state.branch_heads().len(), BRANCHES.len());
        assert_eq!(state.branch_heads()[0].branch().as_str(), HEAD_BRANCH);
        assert_eq!(state.branch_heads()[0].head().as_str(), HEAD_SHA);
        assert_eq!(state.branch_heads()[1].branch().as_str(), BASE_BRANCH);
        assert_eq!(state.branch_heads()[1].head().as_str(), BASE_SHA);
    }

    #[tokio::test]
    async fn a_complete_poll_normalizes_latest_branch_workflow_runs() {
        let observation = complete_typed_observation().await;
        let state = observation.state();

        assert_eq!(state.workflow_runs().len(), WORKFLOW_RUN_IDS.len());
        assert_eq!(state.workflow_runs()[0].branch().as_str(), HEAD_BRANCH);
        assert_eq!(state.workflow_runs()[0].workflow_id().get(), WORKFLOW_ID);
        assert_eq!(
            state.workflow_runs()[0].attempt().get(),
            WORKFLOW_RUN_ATTEMPT
        );
        assert_eq!(state.workflow_runs()[0].workflow().as_str(), WORKFLOW_NAME);
        assert_eq!(
            state.workflow_runs()[0].conclusion(),
            EXPECTED_FEATURE_WORKFLOW_CONCLUSION
        );
        assert_eq!(state.workflow_runs()[1].branch().as_str(), BASE_BRANCH);
        assert_eq!(
            state.workflow_runs()[1].conclusion(),
            EXPECTED_MAIN_WORKFLOW_CONCLUSION
        );
    }

    #[test]
    fn process_local_cache_sheds_a_resource_beyond_its_total_wire_bound() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());

        cache.insert(
            key.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            MAX_CACHED_WIRE_BYTES + 1,
            Vec::<u8>::new(),
        );

        assert_eq!(cache.entity_tag(&key), None);
    }

    #[test]
    fn process_local_cache_retains_an_entry_for_four_untouched_poll_completions() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());
        cache.insert(
            key.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
        );

        cache.complete_poll();
        cache.complete_poll();
        cache.complete_poll();
        cache.complete_poll();

        assert_eq!(
            cache.entity_tag(&key),
            Some(&EntityTag(ENTITY_TAG.to_owned()))
        );
    }

    #[test]
    fn process_local_cache_evicts_an_entry_on_the_fifth_untouched_poll_completion() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());
        cache.insert(
            key.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
        );

        cache.complete_poll();
        cache.complete_poll();
        cache.complete_poll();
        cache.complete_poll();
        cache.complete_poll();

        assert_eq!(cache.entity_tag(&key), None);
    }

    #[test]
    fn process_local_cache_replaces_an_untouched_stale_entry_at_capacity() {
        let mut cache = PollCache::default();
        let retained = ResourceKey(CACHE_RETAINED_KEY.to_owned());
        let stale = ResourceKey(CACHE_STALE_KEY.to_owned());
        let replacement = ResourceKey(CACHE_REPLACEMENT_KEY.to_owned());
        cache.insert_with_resource_limit(
            retained.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );
        cache.insert_with_resource_limit(
            stale.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );
        cache.begin_poll();
        cache
            .touch(retained.clone())
            .expect("retained cache fixture is touched");
        cache
            .touch(replacement.clone())
            .expect("replacement cache fixture is touched");

        cache.insert_with_resource_limit(
            replacement.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );

        assert_eq!(cache.resources.len(), TEST_CACHE_RESOURCE_LIMIT);
        assert!(cache.resources.contains_key(&retained));
        assert!(cache.resources.contains_key(&replacement));
        assert!(!cache.resources.contains_key(&stale));
    }

    #[test]
    fn a_not_modified_response_refreshes_the_tag_it_validated() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());
        cache.insert(
            key.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
        );

        cache
            .accepted_for_validator::<Vec<u8>>(
                &key,
                Some(&EntityTag(ENTITY_TAG.to_owned())),
                Some(EntityTag(REFRESHED_ENTITY_TAG.to_owned())),
            )
            .expect("the validated pair is still cached");

        assert_eq!(
            cache.entity_tag(&key),
            Some(&EntityTag(REFRESHED_ENTITY_TAG.to_owned()))
        );
    }

    #[test]
    fn a_not_modified_response_leaves_a_concurrently_replaced_pair_alone() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());
        cache.insert(
            key.clone(),
            EntityTag(CONCURRENTLY_REPLACED_ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
        );

        cache
            .accepted_for_validator::<Vec<u8>>(
                &key,
                Some(&EntityTag(ENTITY_TAG.to_owned())),
                Some(EntityTag(REFRESHED_ENTITY_TAG.to_owned())),
            )
            .expect("the replacement pair is cached");

        assert_eq!(
            cache.entity_tag(&key),
            Some(&EntityTag(CONCURRENTLY_REPLACED_ENTITY_TAG.to_owned()))
        );
    }

    #[test]
    fn process_local_cache_sheds_at_capacity_when_every_entry_is_touched() {
        let mut cache = PollCache::default();
        let first = ResourceKey(CACHE_RETAINED_KEY.to_owned());
        let second = ResourceKey(CACHE_STALE_KEY.to_owned());
        let admitted = ResourceKey(CACHE_REPLACEMENT_KEY.to_owned());
        cache.insert_with_resource_limit(
            first.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );
        cache.insert_with_resource_limit(
            second.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );
        cache.begin_poll();
        cache.touch(first.clone()).expect("first entry is touched");
        cache
            .touch(second.clone())
            .expect("second entry is touched");

        cache.insert_with_resource_limit(
            admitted.clone(),
            EntityTag(ENTITY_TAG.to_owned()),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
            TEST_CACHE_RESOURCE_LIMIT,
        );

        assert_eq!(cache.entity_tag(&admitted), None);
        assert!(cache.resources.contains_key(&first));
        assert!(cache.resources.contains_key(&second));
    }

    #[test]
    fn a_new_attempt_clears_prior_poll_metrics_without_discarding_the_cache() {
        let mut cache = PollCache::default();
        let key = ResourceKey(CACHE_RESOURCE_KEY.to_owned());
        let entity_tag = EntityTag(ENTITY_TAG.to_owned());
        cache.insert(
            key.clone(),
            entity_tag.clone(),
            CACHE_WIRE_BYTES,
            Vec::<u8>::new(),
        );
        cache.begin_poll();
        cache.touch(key.clone()).expect("cached fixture is touched");
        cache
            .record_poll_wire_bytes(CACHE_KEY_KIND, CACHE_WIRE_BYTES)
            .expect("fixture wire bytes are admitted");

        cache.begin_attempt();

        assert_eq!(cache.requests, 0);
        assert_eq!(cache.poll_wire_bytes, 0);
        assert_eq!(cache.cached_wire_bytes, CACHE_WIRE_BYTES);
        assert_eq!(cache.entity_tag(&key), Some(&entity_tag));
    }

    #[test]
    fn one_poll_rejects_response_bytes_beyond_its_aggregate_wire_bound() {
        let mut cache = PollCache::default();
        cache.begin_poll();
        cache
            .record_poll_wire_bytes(CACHE_KEY_KIND, MAX_POLL_WIRE_BYTES)
            .expect("exact aggregate wire bound is accepted");

        assert_eq!(
            cache.record_poll_wire_bytes(CACHE_KEY_KIND, 1),
            Err(RepositoryWatchAttemptError::ResourceLimit)
        );
    }

    #[test]
    fn process_local_cache_keys_do_not_retain_request_urls_or_query_values() {
        let url = Url::parse(CACHE_KEY_URL).expect("cache-key URL fixture is valid");
        let key = ResourceKey::new(CACHE_KEY_KIND, &reqwest::Method::GET, &url, None);

        assert!(key.0.starts_with(CACHE_KEY_KIND));
        assert!(!key.0.contains(CACHE_KEY_QUERY_VALUE));
        assert!(!key.0.contains(WATCHED_REPOSITORY));
    }

    #[test]
    fn pull_request_dispatch_context_serializes_the_complete_triggering_fact() {
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())
            .expect("fixture repository is valid");
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(
                PULL_NUMBER
                    .try_into()
                    .expect("fixture pull-request number is positive"),
            ),
            head_sha: CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture SHA is valid"),
            head_repository: repository.clone(),
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())
                .expect("fixture base branch is valid"),
            head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())
                .expect("fixture head branch is valid"),
            title: PullRequestTitle::try_new("Repository watch".to_owned())
                .expect("fixture title is valid"),
            body: PullRequestBody::try_new("Conflict detected.".to_owned())
                .expect("fixture body is valid"),
            labels: Vec::new(),
            draft: false,
            author: None,
        });
        let event = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(uuid::Uuid::from_u128(71)),
            repository,
            context,
            RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Conflicting,
            },
        )
        .expect("fixture event is coherent");
        let encoded: serde_json::Value =
            serde_json::from_str(&dispatch_context_json(&event)).expect("dispatch context is JSON");

        assert_eq!(encoded["type"], "pull_request");
        assert_eq!(encoded["repo"], WATCHED_REPOSITORY);
        assert_eq!(encoded["number"], PULL_NUMBER);
        assert_eq!(encoded["head_sha"], HEAD_SHA);
        assert_eq!(encoded["event"]["target"]["base_branch"], BASE_BRANCH);
        assert_eq!(encoded["event"]["target"]["head_branch"], HEAD_BRANCH);
        assert_eq!(encoded["event"]["kind"], "MergeableStateChanged");
        assert_eq!(encoded["event"]["payload"]["current"], "conflicting");
    }

    #[test]
    fn base_advance_dispatch_context_targets_the_dependent_pull_request() {
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())
            .expect("fixture repository is valid");
        let base_branch =
            BranchName::try_new(BASE_BRANCH.to_owned()).expect("fixture base branch is valid");
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(
                PULL_NUMBER
                    .try_into()
                    .expect("fixture pull-request number is positive"),
            ),
            head_sha: CommitSha::try_new(HEAD_SHA.to_owned()).expect("fixture SHA is valid"),
            head_repository: repository.clone(),
            base_branch: base_branch.clone(),
            head_branch: BranchName::try_new(AGENT_HEAD_BRANCH.to_owned())
                .expect("fixture head branch is valid"),
            title: PullRequestTitle::try_new(PULL_TITLE.to_owned())
                .expect("fixture title is valid"),
            body: PullRequestBody::try_new(PULL_BODY.to_owned()).expect("fixture body is valid"),
            labels: Vec::new(),
            draft: false,
            author: None,
        });
        let event = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(uuid::Uuid::from_u128(BASE_ADVANCE_EVENT_ID)),
            repository,
            context,
            RepoWatchEventKindV1::BaseAdvanced {
                branch: base_branch,
            },
        )
        .expect("fixture event is coherent");
        let encoded: serde_json::Value =
            serde_json::from_str(&dispatch_context_json(&event)).expect("dispatch context is JSON");

        assert_eq!(encoded["type"], "pull_request");
        assert_eq!(encoded["number"], PULL_NUMBER);
        assert_eq!(encoded["event"]["target"]["base_branch"], BASE_BRANCH);
        assert_eq!(encoded["event"]["target"]["head_branch"], AGENT_HEAD_BRANCH);
        assert_eq!(encoded["event"]["kind"], "BaseAdvanced");
        assert_eq!(encoded["event"]["payload"]["branch"], BASE_BRANCH);
    }

    #[tokio::test]
    async fn owed_dispatch_context_collapses_history_into_the_current_durable_state() {
        let observation = complete_typed_observation().await;
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())
            .expect("fixture repository is valid");
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(
                PULL_NUMBER
                    .try_into()
                    .expect("fixture pull-request number is positive"),
            ),
            head_sha: CommitSha::try_new(OWED_EVENT_HEAD_SHA.to_owned())
                .expect("fixture old SHA is valid"),
            head_repository: repository.clone(),
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())
                .expect("fixture base branch is valid"),
            head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())
                .expect("fixture head branch is valid"),
            title: PullRequestTitle::try_new("Earlier repository watch".to_owned())
                .expect("fixture title is valid"),
            body: PullRequestBody::try_new("Earlier review state.".to_owned())
                .expect("fixture body is valid"),
            labels: Vec::new(),
            draft: false,
            author: None,
        });
        let event = RepoWatchEvent::try_pull_request(
            RepoWatchEventId::from_uuid(uuid::Uuid::from_u128(81)),
            repository,
            context,
            RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: RepoWatchAuthorLogin::try_new(PROVIDER_REVIEWER.to_owned())
                    .expect("fixture reviewer is valid"),
                state: ReviewState::Approved,
                commit: CommitSha::try_new(OWED_EVENT_HEAD_SHA.to_owned())
                    .expect("fixture review commit is valid"),
            },
        )
        .expect("fixture event is coherent");
        let encoded: serde_json::Value = serde_json::from_str(&owed_dispatch_context_json_parts(
            &event,
            uuid::Uuid::from_u128(82),
            RepoWatchEventId::from_uuid(uuid::Uuid::from_u128(79)),
            OWED_MATCH_COUNT,
            &observation,
        ))
        .expect("owed dispatch context is JSON");

        assert_eq!(encoded["event"]["target"]["head_sha"], OWED_EVENT_HEAD_SHA);
        assert_eq!(encoded["delivery"]["mode"], "owed_current_state");
        assert_eq!(encoded["delivery"]["matched_event_count"], OWED_MATCH_COUNT);
        assert_eq!(
            encoded["delivery"]["current"]["target"]["head_sha"],
            HEAD_SHA
        );
        assert_eq!(
            encoded["delivery"]["current"]["reviews"]
                .as_array()
                .map(Vec::len),
            Some(RETAINED_REVIEW_IDS.len())
        );
        assert_eq!(
            encoded["delivery"]["current"]["threads"][0]["thread"],
            REVIEW_THREAD
        );
        assert_eq!(
            encoded["delivery"]["current"]["threads"][0]["state"],
            "open"
        );
        assert_eq!(
            encoded["delivery"]["current"]["threads"][1]["thread"],
            RESOLVED_REVIEW_THREAD
        );
    }
}
