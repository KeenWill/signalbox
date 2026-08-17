//! Conditional GitHub polling and durable repository-watch handoff.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
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
    RepoWatchCheckSuiteObservation, RepoWatchDifferFailureKind, RepoWatchDispatchService,
    RepoWatchDispatchTransaction, RepoWatchEventIdentityFrontierV1, RepoWatchEventOccurrenceV1,
    RepoWatchObservation, RepoWatchObservationApplyV1, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchReactionObservation,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewObservation,
    RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome, RepoWatchTargetedRefreshCoalescerV1,
    RepoWatchTargetedRefreshV1, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWebhookDeliveryV1, RepoWatchWebhookDeliveryV1Input, RepoWatchWebhookIgnoredReasonV1,
    RepoWatchWebhookMappedNoChangeV1, RepoWatchWebhookMappingError, RepoWatchWebhookMappingV1,
    RepoWatchWorkflowRunObservation, UuidV7RepoWatchDispatchIdGenerator,
    UuidV7RepoWatchEventIdGenerator, apply_repo_watch_observation_patch_v1,
    derive_repo_watch_events, map_repo_watch_webhook_delivery_v1,
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
use signalbox_persistence::repo_watch_dispatch_obligation::RepoWatchDispatchObligation;
use signalbox_persistence::repo_watch_webhook::{
    PendingRepoWatchWebhookDelivery, PostgresRepoWatchWebhookStore, RepoWatchWebhookDeliveryKey,
    RepoWatchWebhookDisposition, RepoWatchWebhookParityCauseV1, RepoWatchWebhookPendingPageSize,
    RepoWatchWebhookProjection, RepoWatchWebhookStoreError, RepoWatchWebhookTargetedQuery,
    RepoWatchWebhookTerminalRequest,
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, sleep_until},
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
const WEBHOOK_WORK_CHANNEL_CAPACITY: usize = 1;
const WEBHOOK_PENDING_PAGE_SIZE: NonZeroU16 =
    NonZeroU16::new(100).expect("webhook pending page size is positive");
// One webhook drain visits at most this many pending pages before returning to
// the scheduler. Webhook wakes accelerate reconciliation and must never crowd
// out the full poll that performs it, so a sustained stream re-arms its own wake
// instead of holding the worker across poll deadlines.
const WEBHOOK_DRAIN_PAGE_LIMIT: usize = 2;

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
    WebhookListenerExited,
    WebhookListenerFailed,
    TaskSetEmpty,
}

impl fmt::Display for RepositoryWatchRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RepositoryTaskExited => "repository-watch task exited before shutdown",
            Self::RepositoryTaskPanicked => "repository-watch task panicked",
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
        for repository in configuration.repositories() {
            let mut webhook_nudge = None;
            let webhook_work = repository.webhook().map(|_| {
                let (sender, receiver) = mpsc::channel(WEBHOOK_WORK_CHANNEL_CAPACITY);
                webhook_workers.insert(repository.repository().clone(), sender.clone());
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
        let mut pollers = Vec::with_capacity(self.tasks.len());
        for task in self.tasks {
            pollers.push(Arc::clone(&task.poller));
            let task_shutdown = shutdown.clone();
            tasks.spawn(async move {
                task.run(task_shutdown).await;
                RepositoryWatchChildExit::Repository
            });
        }
        if let Some(webhook) = self.webhook {
            let webhook_shutdown = shutdown.clone();
            tasks.spawn(async move {
                RepositoryWatchChildExit::Webhook(webhook.run(webhook_shutdown).await)
            });
        }
        supervise_repository_tasks(tasks, pollers, shutdown).await
    }
}

enum RepositoryWatchChildExit {
    Repository,
    Webhook(Result<(), RepoWatchWebhookRuntimeError>),
}

async fn supervise_repository_tasks(
    mut tasks: JoinSet<RepositoryWatchChildExit>,
    pollers: Vec<Arc<GitHubRepositoryPoller>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RepositoryWatchRuntimeError> {
    let result = async {
        if *shutdown.borrow() {
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
                        while let Some(result) = tasks.join_next().await {
                            result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
                        }
                        return Ok(());
                    }
                }
                completed = tasks.join_next() => {
                    return match completed {
                        Some(Ok(_)) if *shutdown.borrow() => {
                            while let Some(result) = tasks.join_next().await {
                                result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
                            }
                            Ok(())
                        }
                        Some(Ok(RepositoryWatchChildExit::Repository)) => {
                            Err(RepositoryWatchRuntimeError::RepositoryTaskExited)
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

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    for poller in &pollers {
        poller.drain_fetches().await;
    }
    result
}

async fn receive_webhook_work(receiver: &mut Option<mpsc::Receiver<()>>) -> bool {
    let received = match receiver {
        Some(receiver) => receiver.recv().await.is_some(),
        None => std::future::pending().await,
    };
    if !received {
        *receiver = None;
    }
    received
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
    webhook_work: Option<mpsc::Receiver<()>>,
    webhook_nudge: Option<mpsc::Sender<()>>,
    webhook_shadow: Option<WebhookShadowBaseline>,
    webhook_shadow_superseded: bool,
    rules_activated: bool,
}

struct RepositoryWatchTaskContext {
    pool: PgPool,
    signal_reviewers: Vec<RepoWatchAuthorLogin>,
    rules: Vec<RepoWatchRule>,
    templates: SessionTemplateConfiguration,
    models: HubModelConfiguration,
    credential_pin: signalbox_persistence::SessionCredentialPin,
    eligibility_nudge: InProcessEligibilityNudge,
    webhook_work: Option<mpsc::Receiver<()>>,
    webhook_nudge: Option<mpsc::Sender<()>>,
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
            rules_activated: false,
        })
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        if self.webhook_work.is_some() {
            let result = select! {
                result = self.run_webhook_attempt() => Some(result),
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                    None
                }
            };
            let Some(result) = result else {
                return;
            };
            match &result {
                Ok(()) => tracing::debug!(
                    repository = %self.repository.as_str(),
                    "repository-watch startup webhook backlog drained"
                ),
                Err(error) => tracing::warn!(
                    repository = %self.repository.as_str(),
                    cause_code = error.cause_code(),
                    "repository-watch startup webhook backlog awaits retry"
                ),
            }
            if result.is_err_and(RepositoryWatchAttemptError::is_permanent) {
                return;
            }
        }
        let mut next_poll = Instant::now();
        loop {
            if *shutdown.borrow() {
                return;
            }
            select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                () = sleep_until(next_poll) => {
                    let cycle_started = Instant::now();
                    let mut attempt_cancelled = false;
                    let result = select! {
                        result = self.run_attempt() => Some(result),
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                attempt_cancelled = true;
                            }
                            None
                        }
                    };
                    if attempt_cancelled {
                        // A cancelled full poll may own spawned PR fetches.
                        self.poller.drain_fetches().await;
                        return;
                    }
                    let Some(result) = result else {
                        continue;
                    };
                    let metrics = self.poller.attempt_metrics();
                    match &result {
                        Ok(()) => tracing::debug!(
                            repository = %self.repository.as_str(),
                            "repository-watch polling attempt completed"
                        ),
                        Err(error) => tracing::warn!(
                            repository = %self.repository.as_str(),
                            cause_code = error.cause_code(),
                            request_count = metrics.requests,
                            poll_wire_bytes = metrics.poll_wire_bytes,
                            cached_resource_count = metrics.cached_resources,
                            cached_wire_bytes = metrics.cached_wire_bytes,
                            "repository-watch polling attempt failed closed"
                        ),
                    }
                    if result.is_err_and(RepositoryWatchAttemptError::is_permanent) {
                        return;
                    }
                    next_poll = cycle_started + self.interval;
                }
                admitted = receive_webhook_work(&mut self.webhook_work) => {
                    if !admitted {
                        continue;
                    }
                    let mut attempt_cancelled = false;
                    let result = select! {
                        result = self.run_webhook_attempt() => Some(result),
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                attempt_cancelled = true;
                            }
                            None
                        }
                    };
                    if attempt_cancelled {
                        return;
                    }
                    let Some(result) = result else {
                        continue;
                    };
                    match &result {
                        Ok(()) => tracing::debug!(
                            repository = %self.repository.as_str(),
                            "repository-watch webhook work completed"
                        ),
                        Err(error) => tracing::warn!(
                            repository = %self.repository.as_str(),
                            cause_code = error.cause_code(),
                            "repository-watch webhook work failed closed"
                        ),
                    }
                    if result.is_err_and(RepositoryWatchAttemptError::is_permanent) {
                        return;
                    }
                }
            }
        }
    }

    async fn run_attempt(&mut self) -> Result<(), RepositoryWatchAttemptError> {
        self.poller.begin_attempt();
        let result = async {
            if !self.rules_activated {
                self.activate_rules().await?;
                self.rules_activated = true;
            }
            self.process_cutoffs().await?;
            self.process_dispatches().await?;
            // Deliveries already admitted are projected before this poll runs.
            // A poll that observes the same transition would otherwise advance
            // the cursor past them, and every one of them would then apply to
            // state that already contains it and record nothing.
            //
            // Its failure is reported but not propagated: acceleration failing
            // must not cancel the reconciliation sweep, or one delivery whose
            // targeted request keeps failing would abort every scheduled poll.
            let accelerated = self.process_webhook_deliveries().await;
            if let Err(error) = &accelerated {
                tracing::warn!(
                    repository = %self.repository.as_str(),
                    cause_code = error.cause_code(),
                    "repository-watch webhook pre-poll drain failed; polling continues"
                );
            }
            self.poll_and_commit().await?;
            self.process_webhook_deliveries().await?;
            accelerated?;
            self.process_cutoffs().await?;
            self.process_dispatches().await
        }
        .await;
        if result.is_err() {
            // Any failed attempt may leave published entries tied to an older
            // durable cursor, regardless of which step failed.
            self.poller.invalidate_freshness();
        }
        result
    }

    async fn run_webhook_attempt(&mut self) -> Result<(), RepositoryWatchAttemptError> {
        self.poller.begin_attempt();
        let result = async {
            if !self.rules_activated {
                self.activate_rules().await?;
                self.rules_activated = true;
            }
            self.process_cutoffs().await?;
            self.process_dispatches().await?;
            self.process_webhook_deliveries().await?;
            self.process_cutoffs().await?;
            self.process_dispatches().await
        }
        .await;
        if result.is_err() {
            self.poller.invalidate_freshness();
        }
        result
    }

    async fn process_webhook_deliveries(&mut self) -> Result<(), RepositoryWatchAttemptError> {
        let page_size = RepoWatchWebhookPendingPageSize::try_new(WEBHOOK_PENDING_PAGE_SIZE)
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        let mut deferred: HashSet<RepoWatchWebhookDeliveryKey> = HashSet::new();
        let mut first_failure: Option<RepositoryWatchAttemptError> = None;
        let mut pages = 0_usize;
        // Every receipt this drain has visited, so a deferred head cannot be
        // reloaded ahead of what follows it. A page bounded by bytes can hold
        // nothing but that head, which would otherwise leave every later
        // receipt permanently unreachable.
        let mut after_receipt: Option<NonZeroU64> = None;
        loop {
            let deliveries = self
                .webhook_store
                .load_pending(&self.repository, page_size, after_receipt)
                .await
                .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
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
                match self.process_webhook_delivery(delivery, &mut page).await {
                    Ok(()) => {}
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
                        }
                    }
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
        match first_failure {
            Some(error) => Err(error),
            None => Ok(()),
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
            // A full channel already carries the pending wake.
            let _ = nudge.try_send(());
        }
    }

    async fn process_webhook_delivery(
        &mut self,
        pending: &PendingRepoWatchWebhookDelivery,
        page: &mut RepoWatchTargetedRefreshCoalescerV1,
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
                        self.webhook_shadow = Some(WebhookShadowBaseline {
                            observation,
                            identity_frontier,
                        });
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
                        let prepared = self.prepare_targeted_refresh(&unissued).await?;
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
                        // Its disposition is then durable before the cursor
                        // mutation becomes externally visible, so a failure
                        // between the two reproduces this disposition rather than
                        // re-deriving against a cursor that already moved past it.
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
                        if let Some(prepared) = prepared {
                            // A targeted poll reconciles only the pull requests it
                            // names, so its cursor does not carry what the webhook
                            // stream has projected for anything else. The shadow
                            // is kept rather than reloaded; the next full poll is
                            // the complete sweep that replaces it.
                            self.commit_targeted_refresh(prepared).await?;
                            // Recorded only once the refresh has landed, so a
                            // failure above leaves the hydration for the page's
                            // remaining deliveries to reissue.
                            page.record_issued(&issued);
                        }
                        self.process_dispatches().await
                    }
                }
            }
        }
    }

    async fn record_webhook_terminal(
        &self,
        pending: &PendingRepoWatchWebhookDelivery,
        projections: Vec<RepoWatchWebhookProjection>,
        disposition: RepoWatchWebhookDisposition,
        outcome_code: Option<&str>,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let request = RepoWatchWebhookTerminalRequest::try_new(
            projections,
            disposition,
            outcome_code.map(str::to_owned),
        )
        .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        match self
            .webhook_store
            .record_terminal(pending.key(), &request)
            .await
        {
            Ok(_) => Ok(()),
            // A commit whose result was lost in transit may already be durable,
            // and the delivery would then never be loaded again. Re-recording
            // the exact same request resolves which happened: it reports the
            // row already terminal, or records it now.
            Err(RepoWatchWebhookStoreError::CommitAmbiguous(_)) => self
                .webhook_store
                .record_terminal(pending.key(), &request)
                .await
                .map(|_| ())
                .map_err(|_| RepositoryWatchAttemptError::Persistence),
            Err(_) => Err(RepositoryWatchAttemptError::Persistence),
        }
    }

    /// Runs one delivery's targeted provider queries without writing anything.
    ///
    /// The fetch is separated from its commit so a transient provider failure
    /// leaves the delivery pending and retryable, rather than terminal with a
    /// targeted query that never ran.
    async fn prepare_targeted_refresh(
        &mut self,
        refreshes: &[RepoWatchTargetedRefreshV1],
    ) -> Result<Option<PreparedTargetedRefresh>, RepositoryWatchAttemptError> {
        if refreshes.is_empty() {
            return Ok(None);
        }
        let cursor = self
            .store
            .load_cursor(&self.repository)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?
            .ok_or(RepositoryWatchAttemptError::Persistence)?;
        let targets = targeted_pull_requests(cursor.candidate().observation(), refreshes)?;
        if targets.is_empty() {
            return Ok(None);
        }
        // A refresh naming no pull request the cursor carries is never sent, so
        // it must not be recorded as a query that happened.
        let queried = refreshes
            .iter()
            .filter(|refresh| refresh_reaches_a_target(refresh, &targets))
            .cloned()
            .collect::<Vec<_>>();
        let mut event_identity_frontier = cursor.candidate().event_identity_frontier().clone();
        let observation = self
            .poller
            .poll_targeted_pull_requests_against_cursor(cursor.candidate().observation(), &targets)
            .await?;
        let events = derive_repo_watch_events(
            &self.repository,
            Some(cursor.candidate().observation()),
            &observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|_| RepositoryWatchAttemptError::Differ)?;
        Ok(Some(PreparedTargetedRefresh {
            generation: cursor.generation(),
            candidate: RepoWatchCursorCandidate::with_event_identity_frontier(
                observation,
                event_identity_frontier,
            ),
            events,
            queried,
        }))
    }

    /// Commits one prepared targeted refresh against the generation it read.
    async fn commit_targeted_refresh(
        &self,
        prepared: PreparedTargetedRefresh,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let outcome = self
            .store
            .commit(
                &self.repository,
                RepoWatchCommitRequest::new(
                    Some(prepared.generation),
                    prepared.candidate,
                    prepared.events,
                ),
            )
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        match outcome {
            RepoWatchCommitOutcome::Committed(cursor)
            | RepoWatchCommitOutcome::Replayed(cursor)
            | RepoWatchCommitOutcome::Unchanged(cursor) => {
                self.poller.publish_freshness(cursor.generation());
                Ok(())
            }
            RepoWatchCommitOutcome::Conflict { current: _ } => {
                Err(RepositoryWatchAttemptError::Persistence)
            }
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
                Ok(false) => return Ok(()),
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
                .load_next_dispatch_obligation(&self.repository, rule.id(), rule.version())
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
            | RepoWatchRuleEvaluationOutcome::Occupied
            | RepoWatchRuleEvaluationOutcome::Cooldown => {}
        }
    }

    async fn poll_and_commit(&mut self) -> Result<(), RepositoryWatchAttemptError> {
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
        let observation = self
            .poller
            .poll_against_cursor(previous, cursor_generation)
            .await?;
        let events = derive_repo_watch_events(
            &self.repository,
            previous,
            &observation,
            &mut event_identity_frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|error| match error.kind() {
            RepoWatchDifferFailureKind::EventConstruction => RepositoryWatchAttemptError::Differ,
            // The frontier refuses on every later comparison too, so the cause
            // code has to say the frontier stopped this repository rather than
            // leave it indistinguishable from a one-observation differ defect.
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
        let outcome = self
            .store
            .commit(
                &self.repository,
                RepoWatchCommitRequest::new(
                    cursor_generation,
                    RepoWatchCursorCandidate::with_event_identity_frontier(
                        observation,
                        event_identity_frontier,
                    ),
                    events,
                ),
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
                Ok(())
            }
            RepoWatchCommitOutcome::Conflict { current: _ } => {
                Err(RepositoryWatchAttemptError::Persistence)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetedPullRequest {
    number: PullRequestNumber,
    expected_head: Option<CommitSha>,
}

/// One targeted refresh that has been fetched and derived but not committed.
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
    RetiredRuleIdentity,
    ChangedRuleIdentity,
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
            Self::RetiredRuleIdentity => "repository_watch_rule_identity_retired",
            Self::ChangedRuleIdentity => "repository_watch_rule_identity_changed",
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
            Self::RetiredRuleIdentity | Self::ChangedRuleIdentity => true,
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
            | Self::Persistence => false,
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
        _ => RepositoryWatchAttemptError::Persistence,
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
    fetches:
        tokio::sync::Mutex<JoinSet<Result<RepoWatchPullRequestState, RepositoryWatchAttemptError>>>,
}

struct PullRequestFreshness {
    updated_at: String,
    settlement: PullRequestSettlement,
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
        self.poll_against_cursor(previous, Some(RepoWatchCursorGeneration::INITIAL))
            .await
    }

    async fn poll_against_cursor(
        self: &Arc<Self>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
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
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
        let mut state = RepoWatchRepositoryStateInput {
            pull_requests: previous.state().pull_requests().to_vec(),
            workflow_runs: previous.state().workflow_runs().to_vec(),
            branch_heads: previous.state().branch_heads().to_vec(),
        };
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
                tracing::debug!(
                    repository = %self.repository.as_str(),
                    pull_request = target.number.get(),
                    "targeted repository refresh was superseded before its response"
                );
                continue;
            }
            match retained_index {
                Some(index) => state.pull_requests[index] = fetched.state,
                None => state.pull_requests.push(fetched.state),
            }
        }
        let state = RepoWatchRepositoryState::try_new(state)
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        Ok(RepoWatchObservation::new(
            previous.signal_reviewers().to_vec(),
            state,
        ))
    }

    async fn poll_complete(
        self: &Arc<Self>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
        let listed = self.fetch_open_pull_numbers().await?;
        let mut pull_numbers: BTreeSet<u64> = listed.keys().copied().collect();
        if let Some(previous) = previous {
            for pull_request in previous.state().pull_requests() {
                if pull_request.lifecycle() == RepoWatchPullRequestLifecycle::Open {
                    pull_numbers.insert(pull_request.context().number().get());
                }
            }
        }
        let pull_requests = self
            .fetch_pull_requests(pull_numbers, &listed, previous, cursor_generation)
            .await?;
        let branch_heads = self.fetch_branch_heads().await?;
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
            pull_requests,
            workflow_runs,
            branch_heads,
        })
        .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        Ok(RepoWatchObservation::new(
            self.signal_reviewers.clone(),
            state,
        ))
    }

    async fn fetch_pull_requests(
        self: &Arc<Self>,
        pull_numbers: BTreeSet<u64>,
        listed: &BTreeMap<u64, ListedPullRequest>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
    ) -> Result<Vec<RepoWatchPullRequestState>, RepositoryWatchAttemptError> {
        self.forget_unlisted_freshness(&pull_numbers);
        let mut fetches = self.fetches.lock().await;
        // A cancelled attempt drops this future mid-collection, which aborts
        // the children without joining them; they stay behind in the shared
        // set. Join any such survivor before spawning, so no child of an
        // earlier attempt can interleave with this one.
        fetches.shutdown().await;
        let collected = self
            .collect_pull_request_fetches(
                pull_numbers,
                listed,
                previous,
                cursor_generation,
                &mut fetches,
            )
            .await;
        // Dropping the set aborts the siblings but does not wait for them.
        // An aborted task only stops at its next await, so it can still charge
        // wire bytes, touch cache entries, or record freshness after this
        // attempt returns, landing that state in the next attempt. Wait for
        // every task to finish before the caller can begin another poll.
        fetches.shutdown().await;
        let mut pull_requests = collected?;
        pull_requests.sort_by_key(|pull_request| pull_request.context().number().get());
        Ok(pull_requests)
    }

    /// Joins every child fetch a cancelled attempt left behind. The repository
    /// task calls this after cancelling an in-flight attempt, so a reported
    /// stop means no child is still resolving credentials, holding a
    /// connection, or touching shared state.
    async fn drain_fetches(&self) {
        self.fetches.lock().await.shutdown().await;
    }

    async fn collect_pull_request_fetches(
        self: &Arc<Self>,
        pull_numbers: BTreeSet<u64>,
        listed: &BTreeMap<u64, ListedPullRequest>,
        previous: Option<&RepoWatchObservation>,
        cursor_generation: Option<RepoWatchCursorGeneration>,
        fetches: &mut JoinSet<Result<RepoWatchPullRequestState, RepositoryWatchAttemptError>>,
    ) -> Result<Vec<RepoWatchPullRequestState>, RepositoryWatchAttemptError> {
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
                fetches.spawn(async move {
                    poller
                        .fetch_or_reuse_pull_request(
                            number,
                            listed_pull_request.as_ref(),
                            previous_pull_request.as_ref(),
                            cursor_generation,
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
    ) -> Result<RepoWatchPullRequestState, RepositoryWatchAttemptError> {
        if let (Some(listed), Some(previous)) = (listed_pull_request, previous_pull_request)
            && self.pull_request_detail_is_reusable(number, listed, previous, cursor_generation)
        {
            let reviews = self.fetch_reviews(number, Some(previous.reviews())).await?;
            let threads = self.fetch_threads(number).await?;
            let reactions = self
                .fetch_reactions(number, Some(previous.reactions()))
                .await?;
            self.record_skipped_poll(number);
            return reuse_pull_request(previous, reviews, threads, reactions);
        }
        let fetched = self
            .fetch_pull_request(number, previous_pull_request)
            .await?;
        match listed_pull_request {
            Some(listed) => {
                self.record_fetched_pull_request(number, listed, fetched.settlement);
            }
            None => self.forget_pull_request(number),
        }
        Ok(fetched.state)
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

    fn record_fetched_pull_request(
        &self,
        number: u64,
        listed: &ListedPullRequest,
        settlement: PullRequestSettlement,
    ) {
        self.freshness().insert(
            number,
            PullRequestFreshness {
                updated_at: listed.updated_at.clone(),
                settlement,
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
        Ok(FetchedPullRequest { state, settlement })
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
                    } else {
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

#[cfg(test)]
struct PollCycleTiming {
    interval: Duration,
    elapsed: Duration,
}

#[cfg(test)]
const fn remaining_interval(timing: PollCycleTiming) -> Duration {
    timing.interval.saturating_sub(timing.elapsed)
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
        fs,
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
        time::sleep,
    };

    use super::{
        CheckConclusion, ChecksOutcome, EntityTag, FileCredentialAccess, GitHubRepositoryPoller,
        ListedPullRequest, MAX_CACHED_WIRE_BYTES, MAX_CONCURRENT_PULL_REQUEST_FETCHES,
        MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS, MAX_POLL_WIRE_BYTES, MergeableState, PAGE_SIZE,
        PollCache, PollCycleTiming, PullRequestSettlement, PullResponse, ReactionContent,
        RepoWatchAuthorLogin, RepoWatchBranchHead, RepoWatchCursorGeneration, RepoWatchObservation,
        RepoWatchPullRequestLifecycle, RepoWatchReactionObservation, RepoWatchReviewObservation,
        RepoWatchThreadState, RepoWatchWorkflowRunAttempt, RepoWatchWorkflowRunObservation,
        RepositorySlug, RepositoryWatchAttemptError, RepositoryWatchChildExit,
        RepositoryWatchRuntimeConstructionError, RepositoryWatchRuntimeError, ResourceKey,
        ReviewState, TargetedPullRequest, Url, UuidV7RepoWatchEventIdGenerator, WorkflowName,
        WorkflowResponse, derive_repo_watch_events, dispatch_context_json,
        normalize_checks_outcome, normalize_pull_request_context, object_id,
        owed_dispatch_context_json_parts, remaining_interval, rule_activation_error,
        supervise_repository_tasks, targeted_pull_requests,
    };
    use signalbox_application::{RepoWatchEventIdentityFrontierV1, RepoWatchTargetedRefreshV1};
    use signalbox_domain::{
        BranchName, CommitSha, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionSubject,
        RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindV1, RepoWatchRuleId,
        RepoWatchRuleVersion,
    };
    use signalbox_model_runtime::CredentialReference;
    use signalbox_persistence::repo_watch_dispatch::RepoWatchDispatchRepositoryError;

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
                validator: Some(ENTITY_TAG),
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                link: None,
                body: body.0,
                delay: Duration::ZERO,
            }
        }

        fn not_modified(target: RequestTarget) -> Self {
            Self {
                method: "GET",
                target: target.0,
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
                validator: None,
                status: "200 OK",
                entity_tag: None,
                link: None,
                body: body.0,
                delay: Duration::ZERO,
            }
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
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert_ne!(read, 0, "request headers must be complete");
        }
        String::from_utf8(request).expect("request headers are UTF-8")
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
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
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
            .skip(1)
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
                ResponseBody(empty_threads()),
            ),
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
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
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
                RequestTarget(REVIEWS_TARGET.to_owned()),
                ResponseBody(reviews),
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
                RequestTarget(BRANCHES_TARGET.to_owned()),
                ResponseBody(branches()),
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
        assert_eq!(refreshed, previous);
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

        let result = supervise_repository_tasks(tasks, Vec::new(), receiver).await;
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
                    )
                    .await;
                RepositoryWatchChildExit::Repository
            }
        });
        server.request_in_flight().await;
        tasks.spawn(async { panic!("fixture repository task panics") });
        let (_sender, receiver) = watch::channel(false);

        let result =
            supervise_repository_tasks(tasks, vec![Arc::clone(&fixture.poller)], receiver).await;

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
                    )
                    .await;
                RepositoryWatchChildExit::Repository
            }
        });
        server.request_in_flight().await;
        tasks.spawn(async { panic!("fixture repository task panics during shutdown") });
        let (_sender, receiver) = watch::channel(true);

        let result =
            supervise_repository_tasks(tasks, vec![Arc::clone(&fixture.poller)], receiver).await;

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
        });

        assert_eq!(error, RepositoryWatchAttemptError::ChangedRuleIdentity);
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

    #[test]
    fn a_cycle_shorter_than_the_interval_waits_out_the_remainder() {
        assert_eq!(
            remaining_interval(PollCycleTiming {
                interval: POLL_INTERVAL,
                elapsed: SHORT_CYCLE,
            }),
            SHORT_CYCLE_REMAINDER
        );
    }

    #[test]
    fn a_cycle_that_reaches_the_interval_starts_the_next_immediately() {
        assert_eq!(
            remaining_interval(PollCycleTiming {
                interval: POLL_INTERVAL,
                elapsed: POLL_INTERVAL,
            }),
            Duration::ZERO
        );
    }

    #[test]
    fn a_cycle_that_overruns_the_interval_starts_the_next_immediately() {
        assert_eq!(
            remaining_interval(PollCycleTiming {
                interval: POLL_INTERVAL,
                elapsed: OVERRUNNING_CYCLE,
            }),
            Duration::ZERO
        );
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
            )
            .await
            .expect("every open pull request is fetched");
        let peak_in_flight = server.finish().await;

        (
            pull_requests
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
        fixture
            .poller
            .record_fetched_pull_request(number, &listed, PullRequestSettlement::Settled);
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
        fixture
            .poller
            .record_fetched_pull_request(number, &listed, PullRequestSettlement::Settled);
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
        fixture
            .poller
            .record_fetched_pull_request(number, &listed, PullRequestSettlement::Settled);
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
        fixture
            .poller
            .record_fetched_pull_request(number, &listed, PullRequestSettlement::Settled);
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
