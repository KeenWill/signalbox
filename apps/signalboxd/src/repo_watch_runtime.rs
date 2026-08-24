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
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderValue, IF_NONE_MATCH, LINK, USER_AGENT,
    },
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use signalbox_application::{
    EligibilityNudge, InProcessEligibilityNudge, RepoWatchBranchHead,
    RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceAssessmentInput, RepoWatchDifferFailureKind, RepoWatchDispatchService,
    RepoWatchDispatchTransaction, RepoWatchEventIdentityFrontierV1, RepoWatchEventOccurrenceV1,
    RepoWatchObservation, RepoWatchObservationApplyV1, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchReactionObservation,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewDecision,
    RepoWatchReviewObservation, RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchTargetedRefreshCoalescerV1, RepoWatchTargetedRefreshV1, RepoWatchThreadObservation,
    RepoWatchThreadState, RepoWatchWebhookDeliveryV1, RepoWatchWebhookDeliveryV1Input,
    RepoWatchWebhookIgnoredReasonV1, RepoWatchWebhookMappedNoChangeV1,
    RepoWatchWebhookMappingError, RepoWatchWebhookMappingV1, RepoWatchWorkflowRunObservation,
    UuidV7RepoWatchDispatchIdGenerator, UuidV7RepoWatchEventIdGenerator,
    apply_repo_watch_observation_patch_v1, derive_repo_watch_events,
    map_repo_watch_webhook_delivery_v1,
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
    RepoWatchCursorCandidate, RepoWatchCursorGeneration,
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
    WatchedRepositoryConfiguration,
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
    NonZeroU16::new(100).expect("webhook pending page size is positive");
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
// Pending webhook work ordinarily drains in seconds. One minute leaves ample
// room for an in-flight bounded provider request while ensuring a task wedge
// becomes an operator-visible error well before the next full poll.
const WEBHOOK_DRAIN_STALL_THRESHOLD: Duration = Duration::from_secs(60);
// The serialized repository task must return to its scheduler even when one
// drain step never does. Individual provider requests have their own deadline,
// but a drain can perform many requests and database operations; without this
// outer bound, admission wakes and retries remain coalesced behind it forever.
// Pending delivery records are durable, so cancellation leaves the unfinished
// work for the existing bounded backoff path to retry.
const WEBHOOK_DRAIN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
// Every shared child-set join uses this bound. A later attempt may retry the
// join, but it never spawns alongside survivors or wedges the scheduler while
// waiting for a child that does not finish cancellation.
const WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
// Shutdown gives a retained targeted completion a short grace period, then
// aborts and joins it so a wedged database operation cannot prevent the
// repository supervisor from stopping.
const WEBHOOK_TARGETED_COMPLETION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// The monitor reads through the shared daemon pool, whose connections wedged
// repositories can hold all of. An unbounded acquisition would leave the
// observer silent during exactly the degradation it exists to expose, so the
// inspection is bounded and expiry is itself an operator-visible signal. Well
// under the stall threshold, so a bounded failure is reported within the
// cadence rather than displacing the report it exists to produce.
const WEBHOOK_DRAIN_MONITOR_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
// One webhook drain visits at most this many pending pages before returning to
// the scheduler. Webhook wakes accelerate reconciliation and must never crowd
// out the full poll that performs it, so a sustained stream re-arms its own wake
// instead of holding the worker across poll deadlines.
const WEBHOOK_DRAIN_PAGE_LIMIT: usize = 2;
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

impl RepositoryWatchRuntime {
    /// Constructs all repository-specific clients without reading credentials.
    pub fn try_new(
        pool: PgPool,
        configuration: &RepositoryWatchConfiguration,
        templates: SessionTemplateConfiguration,
        models: HubModelConfiguration,
        credential_pin: signalbox_persistence::SessionCredentialPin,
        eligibility_nudge: InProcessEligibilityNudge,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
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
                },
            )?);
        }
        let webhook = RepoWatchWebhookRuntime::try_new(pool, configuration, webhook_workers)
            .map_err(|_| RepositoryWatchRuntimeConstructionError)?;
        Ok(Self { tasks, webhook })
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
                let store = task.webhook_store.clone();
                let monitor_shutdown = task_shutdown.clone();
                tasks.spawn(async move {
                    monitor_webhook_drain(repository, store, monitor_shutdown).await;
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
        poller.drain_fetches().await;
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

async fn await_poll_or_interrupt<F>(
    poll: F,
    shutdown: &mut watch::Receiver<bool>,
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
    store: PostgresRepoWatchWebhookStore,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut next_inspection = Instant::now() + WEBHOOK_DRAIN_MONITOR_INTERVAL;
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
        // The inspection acquires a pooled connection and queries, neither of
        // which this task bounds. Awaiting it outside shutdown would hold this
        // child while PostgreSQL was unresponsive, and the supervisor joins
        // every child before aborting the set, so the daemon could not stop.
        if run_until_shutdown(
            &mut shutdown,
            inspect_webhook_drain(&repository, &store, WEBHOOK_DRAIN_STALL_THRESHOLD),
        )
        .await
        .is_none()
        {
            return;
        }
    }
}

async fn inspect_webhook_drain(
    repository: &RepositorySlug,
    store: &PostgresRepoWatchWebhookStore,
    stall_threshold: Duration,
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
        Ok(Ok(None)) => return,
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
    if pending_for < stall_threshold {
        return;
    }
    tracing::error!(
        repository = %repository.as_str(),
        hook_id = oldest.key().hook_id().get(),
        delivery_id = %oldest.key().delivery_id(),
        receipt_sequence = oldest.receipt().sequence().get(),
        pending_seconds = pending_for.as_secs(),
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
    webhook_shadow: Option<WebhookShadowBaseline>,
    webhook_shadow_superseded: bool,
    webhook_shadow_supersession_epoch: u64,
    webhook_projected_terminal_in_flight: Option<RepoWatchWebhookDeliveryKey>,
    webhook_dispatch_in_flight: bool,
    webhook_targeted_completion:
        Option<JoinHandle<Result<TargetedWebhookCompletion, TargetedWebhookCompletionError>>>,
    webhook_terminal_ambiguous: Option<RepoWatchWebhookDeliveryKey>,
    webhook_drain_first_failure: Option<RepositoryWatchAttemptError>,
    webhook_drain_projection_failure: Option<RepositoryWatchAttemptError>,
    webhook_drain_timed_out: bool,
    payload_purge: WebhookPayloadPurgeSchedule,
    rules_activated: bool,
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
            payload_purge,
            rules_activated: false,
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
                handle.abort();
                let _ = handle.await;
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
        let mut webhook_retry = WebhookDrainRetry::default();
        if self.webhook_work.is_some() {
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
        let mut next_poll = Instant::now();
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
                    let webhook_interrupt = match webhook_retry.is_backing_off() {
                        true => WebhookPollInterrupt::Suppressed,
                        false => WebhookPollInterrupt::Enabled,
                    };
                    let outcome = self
                        .run_preemptible_attempt_until_shutdown(
                            drain,
                            &mut shutdown,
                            webhook_interrupt,
                            &mut drained,
                            &mut trailing_failure,
                        )
                        .await;
                    let result = match outcome {
                        PollAttemptWait::Completed(result) => result,
                        PollAttemptWait::Shutdown => {
                            // A cancelled full poll may own spawned PR fetches.
                            self.poller.drain_fetches().await;
                            self.poller.invalidate_freshness();
                            return;
                        }
                        PollAttemptWait::Continue => {
                            let _ = self.poller.drain_fetches_bounded().await;
                            self.poller.invalidate_freshness();
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
                            tracing::debug!(
                                repository = %self.repository.as_str(),
                                "repository-watch webhook work preempted a full poll"
                            );
                            // One wake may preempt a due poll. Its resumed
                            // attempt is deliberately not interruptible, so a
                            // sustained valid webhook stream cannot starve the
                            // complete reconciliation sweep. Any later wake
                            // remains coalesced for the next scheduling pass.
                            let resumed_drain = webhook_retry.poll_drain();
                            // The preempting webhook attempt has already updated
                            // retry state. Only a drain performed by the resumed
                            // poll may disposition that state below.
                            drained = None;
                            trailing_failure = None;
                            let Some(result) = run_until_shutdown(
                                &mut shutdown,
                                self.run_attempt(
                                    resumed_drain,
                                    &mut drained,
                                    &mut trailing_failure,
                                ),
                            )
                            .await
                            else {
                                self.poller.drain_fetches().await;
                                self.poller.invalidate_freshness();
                                return;
                            };
                            result
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

    async fn run_webhook_attempt_until_shutdown(
        &mut self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Option<WebhookAttemptOutcome> {
        run_until_shutdown(shutdown, self.run_webhook_attempt()).await
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

    /// Runs one due poll with only its provider sweep cancellable by a webhook.
    ///
    /// Rule activation, dispatch, webhook projection, and cursor commit all
    /// remain outside that cancellation region. Cancelling the provider sweep
    /// therefore abandons only read-side work and its spawned fetches.
    async fn run_preemptible_attempt_until_shutdown(
        &mut self,
        drain: WebhookDrain,
        shutdown: &mut watch::Receiver<bool>,
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

    async fn run_webhook_attempt(&mut self) -> WebhookAttemptOutcome {
        self.poller.begin_attempt();
        let outcome = async {
            if !self.rules_activated {
                if let Err(error) = self.activate_rules().await {
                    return WebhookAttemptOutcome::FailedBeforeDrain(error);
                }
                self.rules_activated = true;
            }
            // Leading reconciliation failures no longer gate the drain:
            // pending deliveries owe none of that work, and gating here left
            // every newly admitted delivery waiting on unrelated dispatch
            // trouble. The failure is preserved and reported once the drain
            // has run.
            let leading_failure = if let Err(error) = self.process_cutoffs().await {
                Some(error)
            } else {
                self.process_dispatches().await.err()
            };
            match self.process_webhook_deliveries_with_timeout().await {
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

    async fn process_webhook_deliveries(&mut self) -> WebhookDrainOutcome {
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
        loop {
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
                match self
                    .process_webhook_delivery(delivery, &mut page, &mut dispatch_failure)
                    .await
                {
                    Ok(()) => {
                        if self.webhook_terminal_ambiguous == Some(delivery.key()) {
                            self.webhook_terminal_ambiguous = None;
                        }
                    }
                    Err(error) => {
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
                    }
                }
                if chronological_first.is_none() {
                    chronological_first = first_failure.or(dispatch_failure);
                    self.webhook_drain_first_failure = chronological_first;
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
        self.process_webhook_deliveries_with_deadline(WEBHOOK_DRAIN_ATTEMPT_TIMEOUT)
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
                // fetch set while hydrating a delivery. Bound this cleanup so
                // cancellation itself cannot wedge the repository task. The
                // poller's next attempt drains the same shared set before it
                // can spawn, preserving the no-interleaving policy.
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
                // ambiguous. A targeted cursor commit is retained separately
                // and settled before another drain, so its delivery keeps the
                // pre-commit shadow needed to reproduce its projections.
                if let Some(key) = self.webhook_projected_terminal_in_flight.take() {
                    self.webhook_shadow = None;
                    self.webhook_shadow_superseded = false;
                    self.webhook_terminal_ambiguous = Some(key);
                }
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
                    self.webhook_dispatch_in_flight = false;
                    WebhookDrainOutcome::ProjectionFailed(projection_failure)
                } else if self.webhook_dispatch_in_flight {
                    self.webhook_dispatch_in_flight = false;
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
    async fn seed_webhook_shadow(&mut self) -> Result<bool, RepositoryWatchAttemptError> {
        if self.webhook_shadow.is_some() {
            return Ok(false);
        }
        let cursor = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            .ok_or(RepositoryWatchAttemptError::Persistence)?;
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
            RepoWatchWebhookMappingV1::Patch(patch) => {
                let cause = self
                    .seed_webhook_shadow()
                    .await?
                    .then_some(RepoWatchWebhookParityCauseV1::CrossDrainShadowGap);
                let Some(shadow) = self.webhook_shadow.as_ref() else {
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
                            shadow,
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
                            shadow,
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
                            self.complete_targeted_webhook_projection(
                                prepared,
                                pending.key(),
                                projections,
                                WebhookShadowBaseline {
                                    observation,
                                    identity_frontier,
                                },
                            )
                            .await?;
                            page.record_issued(&issued);
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
        let targets = targeted_pull_requests(cursor.candidate().observation(), refreshes)?;
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
        let events = derive_repo_watch_events(
            &self.repository,
            Some(cursor.candidate().observation()),
            &observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|_| RepositoryWatchAttemptError::Differ)?;
        Ok(PreparedTargetedRefreshOutcome::Prepared(
            PreparedTargetedRefresh {
                generation: cursor.generation(),
                candidate: RepoWatchCursorCandidate::with_event_identity_frontier(
                    observation,
                    event_identity_frontier,
                ),
                events,
                queried,
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
    ) -> Result<(), RepositoryWatchAttemptError> {
        let store = self.store.clone();
        let webhook_store = self.webhook_store.clone();
        let poller = Arc::clone(&self.poller);
        let repository = self.repository.clone();
        let request = RepoWatchCommitRequest::new(
            Some(prepared.generation),
            prepared.candidate,
            prepared.events,
        );
        let terminal = RepoWatchWebhookTerminalRequest::try_new(
            projections,
            RepoWatchWebhookDisposition::Projected,
            None,
        )
        .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        let supersession_epoch = self.webhook_shadow_supersession_epoch;
        self.webhook_targeted_completion = Some(tokio::spawn(async move {
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
                    return Ok(TargetedWebhookCompletion::CursorSuperseded { key });
                }
            }
            Ok(TargetedWebhookCompletion::Applied {
                key,
                shadow,
                supersession_epoch,
            })
        }));
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
    ) -> Option<Result<(), RepositoryWatchAttemptError>> {
        let result = {
            let handle = self.webhook_targeted_completion.as_mut()?;
            handle
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
                Some(Ok(()))
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
                Some(Ok(()))
            }
            Err(TargetedWebhookCompletionError::Terminal(
                key,
                WebhookTerminalRecordError::Ambiguous,
            )) => {
                self.webhook_shadow = None;
                self.webhook_shadow_superseded = false;
                self.webhook_terminal_ambiguous = Some(key);
                Some(Err(RepositoryWatchAttemptError::Persistence))
            }
            Err(TargetedWebhookCompletionError::Cursor) => {
                // The terminal disposition and exact projections are durable,
                // but the cursor outcome is unknown. Reload the durable cursor
                // before projecting any later pending receipt.
                self.webhook_shadow = None;
                self.webhook_shadow_superseded = false;
                Some(Err(RepositoryWatchAttemptError::Persistence))
            }
            Err(_) => Some(Err(RepositoryWatchAttemptError::Persistence)),
        }
    }

    async fn process_cutoffs(&self) -> Result<(), RepositoryWatchAttemptError> {
        loop {
            match self
                .dispatch_store
                .process_next_lifecycle_cutoff(&self.repository, || {
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
                        cause_code = "repository_watch_cutoff_corruption",
                        error = %error,
                        "repository-watch lifecycle cutoff quarantined a corrupt goal; dispatch processing continues"
                    );
                    continue;
                }
                Err(_) => return Err(RepositoryWatchAttemptError::Persistence),
            }
        }
        loop {
            match self
                .dispatch_store
                .process_next_convergence_cutoff(&self.repository, || {
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
                        cause_code = "repository_watch_convergence_cutoff_corruption",
                        error = %error,
                        "repository-watch convergence cutoff quarantined a corrupt goal; dispatch processing continues"
                    );
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
            }
        }
        Ok(())
    }

    fn nudge_dispatched_sessions(&self, outcome: &RepoWatchRuleEvaluationOutcome) {
        match outcome {
            RepoWatchRuleEvaluationOutcome::Dispatched { sessions, .. }
            | RepoWatchRuleEvaluationOutcome::Replayed { sessions, .. } => {
                for session in sessions {
                    let _ = self.eligibility_nudge.nudge(*session);
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
        let events = derive_repo_watch_events(
            &self.repository,
            previous,
            &polled.observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|error| match error.kind() {
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
        Ok(PreparedCompletePoll {
            cursor_generation,
            candidate: RepoWatchCursorCandidate::with_event_identity_frontier(
                polled.observation,
                event_identity_frontier,
            ),
            events,
            convergence: polled.convergence,
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
                self.poller.publish_freshness(cursor.generation());
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

/// One complete provider sweep derived against a durable cursor but not yet
/// committed.
struct PreparedCompletePoll {
    cursor_generation: Option<RepoWatchCursorGeneration>,
    candidate: RepoWatchCursorCandidate,
    events: Vec<RepoWatchEventOccurrenceV1>,
    convergence: Vec<RepoWatchConvergenceAssessment>,
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
}

impl WebhookShadowBaseline {
    fn from_cursor(cursor: &RepoWatchCursor) -> Self {
        Self {
            observation: cursor.candidate().observation().clone(),
            identity_frontier: cursor.candidate().event_identity_frontier().clone(),
        }
    }
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
    let projections = derive_repo_watch_events(
        repository,
        Some(&baseline.observation),
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
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        let select_definition = |alias: ModelAlias| self.models.resolve_alias(alias);
        match self.obligation.take() {
            Some(obligation) => {
                self.store
                    .handle_repo_watch_obligation_with_alias_resolver(
                        obligation,
                        evaluation,
                        select_definition,
                    )
                    .await
            }
            None => {
                self.store
                    .handle_repo_watch_evaluation_with_alias_resolver(evaluation, select_definition)
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
            | Self::WebhookDrainTimedOut => false,
        }
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
}

#[derive(Debug)]
struct FetchedPullRequests {
    states: Vec<RepoWatchPullRequestState>,
    convergence: Vec<RepoWatchConvergenceAssessment>,
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
        self.drain_fetches_bounded().await?;
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
        for pull_request in pull_requests {
            let base_revision = branch_heads
                .iter()
                .find(|branch_head| {
                    branch_head.branch() == pull_request.state.context().base_branch()
                })
                .map(|branch_head| branch_head.head().clone())
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?;
            convergence.push(
                pull_request
                    .convergence_evidence
                    .assess(&pull_request.state, base_revision)?,
            );
            states.push(pull_request.state);
        }
        Ok(FetchedPullRequests {
            states,
            convergence,
        })
    }

    /// Joins every child fetch a cancelled attempt left behind. The repository
    /// task calls this after cancelling an in-flight attempt, so a reported
    /// stop means no child is still resolving credentials, holding a
    /// connection, or touching shared state.
    async fn drain_fetches_bounded(&self) -> Result<(), RepositoryWatchAttemptError> {
        let mut fetches = self.fetches.lock().await;
        drain_pull_request_fetches(&mut fetches).await
    }

    /// Strict shutdown settlement. A clean repository-task exit means no child
    /// fetch remains able to hold resources or mutate shared freshness state.
    async fn drain_fetches(&self) {
        self.fetches.lock().await.shutdown().await;
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
            self.fetch_check_runs(&check_suite_ids).await?;
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
        suite_ids: &[GitHubObjectId],
    ) -> Result<(Vec<RepoWatchCheckRunObservation>, bool), RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut every_run_completed = true;
        for suite_id in suite_ids {
            let suite_id = suite_id.get().to_string();
            let mut page = 1_u16;
            loop {
                let response = self
                    .conditional_json_page::<CheckRunsResponse>(
                        "check-runs",
                        Method::GET,
                        self.repository_url(
                            &["check-suites", &suite_id, "check-runs"],
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
                return Err(RepositoryWatchAttemptError::Rejected);
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
                return Err(RepositoryWatchAttemptError::Rejected);
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
            gating_check_inventory_quiesced: false,
            gating_check_inventory,
            review_decision: retained_review_decision
                .ok_or(RepositoryWatchAttemptError::InvalidResponse)?,
            gating_check_count,
            non_green_gating_checks,
        })
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
            return Err(RepositoryWatchAttemptError::Rejected);
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

#[derive(Clone, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Clone, Deserialize)]
struct GraphQlError {}

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
        num::NonZeroU64,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Notify, watch},
        task::{JoinHandle, JoinSet},
        time::{Instant, sleep},
    };

    use super::{
        CheckConclusion, ChecksOutcome, ConvergenceCheck, EntityTag, FileCredentialAccess,
        GitHubRepositoryPoller, ListedPullRequest, MAX_CACHED_WIRE_BYTES,
        MAX_CONCURRENT_PULL_REQUEST_FETCHES, MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS,
        MAX_POLL_WIRE_BYTES, MergeableState, PAGE_SIZE, PollAttemptWait, PollCache,
        PullRequestSettlement, PullResponse, ReactionContent, RepoWatchAuthorLogin,
        RepoWatchBranchHead, RepoWatchCursorGeneration, RepoWatchObservation,
        RepoWatchPullRequestLifecycle, RepoWatchReactionObservation, RepoWatchReviewObservation,
        RepoWatchThreadState, RepoWatchWorkflowRunAttempt, RepoWatchWorkflowRunObservation,
        RepositorySlug, RepositoryWatchAttemptError, RepositoryWatchChildExit,
        RepositoryWatchRuntimeConstructionError, RepositoryWatchRuntimeError, RepositoryWatchTask,
        RepositoryWatchWake, ResourceKey, ReviewState, TargetedPollOutcome, TargetedPullRequest,
        Url, UuidV7RepoWatchEventIdGenerator, WEBHOOK_DRAIN_ATTEMPT_TIMEOUT,
        WEBHOOK_DRAIN_RETRY_DELAY, WEBHOOK_DRAIN_RETRY_MAX_DELAY, WebhookDrain,
        WebhookDrainOutcome, WebhookDrainRetry, WebhookPayloadPurgeSchedule, WebhookPollInterrupt,
        WorkflowName, WorkflowResponse, await_poll_or_interrupt, derive_repo_watch_events,
        dispatch_context_json, inspect_webhook_drain, next_cadence_deadline, next_repository_wake,
        normalize_checks_outcome, normalize_pull_request_context, object_id,
        observe_webhook_work_before_drain, owed_dispatch_context_json_parts, rule_activation_error,
        run_until_shutdown, supervise_repository_tasks, targeted_pull_requests,
    };
    use signalbox_application::{
        InProcessEligibilityWorkSource, RepoWatchEventIdentityFrontierV1,
        RepoWatchTargetedRefreshV1,
    };
    use signalbox_domain::{
        BranchName, CommitSha, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionSubject,
        RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindV1, RepoWatchRuleId,
        RepoWatchRuleIdentityField, RepoWatchRuleVersion,
    };
    use signalbox_model_runtime::CredentialReference;
    use signalbox_persistence::{
        disposable_postgres_server_args, disposable_postgres_state_tmpfs,
        disposable_test_container_labels, local_test_connection_options, migrate,
        repo_watch::{PostgresRepoWatchStore, RepoWatchCommitRequest, RepoWatchCursorCandidate},
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
    const COMPLETED_SUITE_CHECK_RUNS_TARGET: &str =
        "/repos/namespace/project/check-suites/11/check-runs?filter=all&per_page=100&page=1";
    const QUEUED_SUITE_CHECK_RUNS_TARGET: &str =
        "/repos/namespace/project/check-suites/12/check-runs?filter=all&per_page=100&page=1";
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
            .with_mount(disposable_postgres_state_tmpfs())
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
        let repository = RepositorySlug::try_new(WATCHED_REPOSITORY.to_owned())?;
        let observation = complete_typed_observation().await;
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
                payload_purge: WebhookPayloadPurgeSchedule::starting_now(),
                rules_activated: true,
            },
            _credential_directory: fixture._credential_directory,
        })
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

    fn empty_check_runs() -> &'static str {
        "{\"check_runs\":[]}"
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
                body: String::new(),
                delay: Duration::ZERO,
            }
        }

        fn delayed(mut self, delay: Duration) -> Self {
            self.delay = delay;
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
        let encoded = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{}{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            entity_tag,
            link,
            response.body.len(),
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
                RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
                ResponseBody(check_runs()),
            ),
            ScriptedResponse::ok(
                RequestTarget(QUEUED_SUITE_CHECK_RUNS_TARGET.to_owned()),
                ResponseBody(empty_check_runs().to_owned()),
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

    fn complete_pull_request_responses() -> Vec<ScriptedResponse> {
        complete_typed_observation_responses()
            .into_iter()
            .skip(2)
            .take(12)
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
                RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
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
                if response.target == COMPLETED_SUITE_CHECK_RUNS_TARGET {
                    ScriptedResponse::ok(
                        RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
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
            .filter_map(|response| {
                if response.target == CHECK_SUITES_TARGET {
                    Some(ScriptedResponse::ok(
                        RequestTarget(CHECK_SUITES_TARGET.to_owned()),
                        ResponseBody(settled_check_suites()),
                    ))
                } else if response.target == QUEUED_SUITE_CHECK_RUNS_TARGET {
                    None
                } else {
                    Some(response)
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

    #[tokio::test]
    async fn targeted_refresh_reuses_the_repository_poller_and_preserves_untouched_state() {
        let previous = complete_typed_observation().await;
        let server = ScriptedServer::start(complete_pull_request_responses()).await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        fixture
            .poller
            .fetches
            .lock()
            .await
            .spawn(std::future::pending::<
                Result<super::FetchedPullRequest, RepositoryWatchAttemptError>,
            >());
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
        assert_eq!(
            refreshed,
            TargetedPollOutcome::Observation {
                observation: previous,
                superseded_targets: Vec::new(),
            }
        );
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

        let targets = targeted_pull_requests(&previous, &refreshes)
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

        let result = targeted_pull_requests(&previous, &refreshes);

        assert_eq!(result, Err(RepositoryWatchAttemptError::Normalization));
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
        webhook_sender.send_replace(());

        let outcome = await_poll_or_interrupt(
            std::future::pending::<()>(),
            &mut shutdown,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Webhook));
    }

    #[tokio::test]
    async fn a_complete_poll_wins_without_an_interrupt() {
        let (_webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        const COMPLETION: u8 = 7;

        let outcome = await_poll_or_interrupt(
            async { COMPLETION },
            &mut shutdown,
            &mut webhook_work,
            WebhookPollInterrupt::Enabled,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Completed(COMPLETION)));
    }

    #[tokio::test]
    async fn a_backed_off_webhook_does_not_preempt_a_complete_poll() {
        let (webhook_sender, webhook_receiver) = watch::channel(());
        let mut webhook_work = Some(webhook_receiver);
        let (_shutdown_sender, mut shutdown) = watch::channel(false);
        const COMPLETION: u8 = 11;
        webhook_sender.send_replace(());

        let outcome = await_poll_or_interrupt(
            async { COMPLETION },
            &mut shutdown,
            &mut webhook_work,
            WebhookPollInterrupt::Suppressed,
        )
        .await;

        assert!(matches!(outcome, PollAttemptWait::Completed(COMPLETION)));
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
    async fn admission_during_a_concurrent_drain_is_drained_by_the_same_task_run()
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
        let fixture = webhook_task(&pool).await?;
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
        assert!(webhook_disposition_exists(&webhook_store, second.key()).await?);
        Ok(())
    }

    /// INV-072: deadline cancellation preserves durable webhook work for retry.
    #[tokio::test(start_paused = true)]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_webhook_drain_deadline_cancels_and_retries_durable_work()
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
        {
            // Keep virtual time runnable until the database operation reaches
            // the injected wedge, so Tokio cannot auto-advance the production
            // deadline during PostgreSQL setup.
            let clock_guard = keep_paused_clock_runnable();
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
        assert_eq!(
            fixture.task.webhook_terminal_ambiguous,
            Some(admission.key()),
            "deadline cancellation retains the exact unsettled terminal write"
        );
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
            Err(RepositoryWatchAttemptError::Persistence),
            "an unsettled cancelled terminal write blocks cursor-advancing polls"
        );
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

        inspect_webhook_drain(&repository, &store, Duration::ZERO)
            .with_subscriber(subscriber)
            .await;

        let telemetry = captured.text();
        assert!(telemetry.contains("ERROR"));
        assert!(telemetry.contains("cause_code=\"webhook_projection_drain_stalled\""));
        assert!(telemetry.contains(&admission.key().delivery_id().to_string()));
        Ok(())
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
    async fn a_complete_poll_enumerates_completed_check_runs_through_suites() {
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
            RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(provider_defined_check_runs()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let (runs, _) = fixture
            .poller
            .fetch_check_runs(std::slice::from_ref(&suite))
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
            RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(completed_check_run_without_a_completion_time()),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let result = fixture
            .poller
            .fetch_check_runs(std::slice::from_ref(&suite))
            .await;
        server.finish().await;

        assert_eq!(result, Err(RepositoryWatchAttemptError::InvalidResponse));
    }

    #[tokio::test]
    async fn an_unfinished_report_only_run_does_not_unsettle_gating_checks() {
        let response = check_runs().replace(IN_PROGRESS_CHECK_RUN_NAME, "coverage (report only)");
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            RequestTarget(COMPLETED_SUITE_CHECK_RUNS_TARGET.to_owned()),
            ResponseBody(response),
        )])
        .await;
        let fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let suite =
            object_id(COMPLETED_CHECK_SUITE_IDS[0]).expect("fixture suite identity is positive");

        let (_, every_gating_run_completed) = fixture
            .poller
            .fetch_check_runs(std::slice::from_ref(&suite))
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
