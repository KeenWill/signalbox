//! Conditional GitHub polling and durable repository-watch handoff.

use std::{
    any::Any,
    collections::{BTreeSet, HashMap, HashSet},
    error::Error,
    fmt,
    num::NonZeroU64,
    time::Duration,
};

use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderValue, IF_NONE_MATCH, USER_AGENT},
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use signalbox_application::{
    EligibilityNudge, InProcessEligibilityNudge, RepoWatchBranchHead, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchDispatchService, RepoWatchDispatchTransaction,
    RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchRuleEvaluation,
    RepoWatchRuleEvaluationOutcome, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWorkflowRunObservation, UuidV7RepoWatchDispatchIdGenerator,
    UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, ModelAlias, PullRequestBody, PullRequestEventContext,
    PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionChange,
    ReactionContent, ReactionSubject, RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventKindV1,
    RepoWatchEventTarget, RepoWatchRule, RepoWatchWorkflowRunAttempt, RepositorySlug, ReviewState,
    ReviewThreadId, UserContent, WorkflowName,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_persistence::repo_watch::{
    PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
    RepoWatchCursorCandidate,
};
use signalbox_persistence::repo_watch_dispatch::{
    PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError,
};
use sqlx::PgPool;
use tokio::{select, sync::watch, task::JoinSet, time::sleep};

use crate::SessionTemplateConfiguration;
use crate::configuration::{
    FileCredentialAccess, HubModelConfiguration, RepositoryWatchConfiguration,
    WatchedRepositoryConfiguration,
};

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
const MAX_AGGREGATE_WIRE_BYTES: usize = 64 * 1024 * 1024;

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
    TaskSetEmpty,
}

impl fmt::Display for RepositoryWatchRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RepositoryTaskExited => "repository-watch task exited before shutdown",
            Self::RepositoryTaskPanicked => "repository-watch task panicked",
            Self::TaskSetEmpty => "repository-watch task set became empty",
        })
    }
}

impl Error for RepositoryWatchRuntimeError {}

/// Supervisor for one independent polling task per configured repository.
pub struct RepositoryWatchRuntime {
    tasks: Vec<RepositoryWatchTask>,
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
        for repository in configuration.repositories() {
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
                },
            )?);
        }
        Ok(Self { tasks })
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
        for task in self.tasks {
            tasks.spawn(task.run(shutdown.clone()));
        }
        supervise_repository_tasks(tasks, shutdown).await
    }
}

async fn supervise_repository_tasks(
    mut tasks: JoinSet<()>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RepositoryWatchRuntimeError> {
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
                    Some(Ok(())) if *shutdown.borrow() => {
                        while let Some(result) = tasks.join_next().await {
                            result.map_err(|_| RepositoryWatchRuntimeError::RepositoryTaskPanicked)?;
                        }
                        Ok(())
                    }
                    Some(Ok(())) => Err(RepositoryWatchRuntimeError::RepositoryTaskExited),
                    Some(Err(_)) => Err(RepositoryWatchRuntimeError::RepositoryTaskPanicked),
                    None => Err(RepositoryWatchRuntimeError::TaskSetEmpty),
                };
            }
        }
    }
}

struct RepositoryWatchTask {
    repository: RepositorySlug,
    interval: Duration,
    poller: GitHubRepositoryPoller,
    store: PostgresRepoWatchStore,
    dispatch_store: PostgresRepoWatchDispatchStore,
    rules: Vec<RepoWatchRule>,
    templates: SessionTemplateConfiguration,
    models: HubModelConfiguration,
    eligibility_nudge: InProcessEligibilityNudge,
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
            poller: GitHubRepositoryPoller::try_new(
                configuration.repository().clone(),
                signal_reviewers,
                credentials,
                credential_reference,
            )?,
            store,
            dispatch_store: PostgresRepoWatchDispatchStore::new(pool, credential_pin),
            rules,
            templates,
            models,
            eligibility_nudge,
            rules_activated: false,
        })
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            select! {
                result = self.run_attempt() => {
                    match result {
                        Ok(()) => tracing::debug!(
                            repository = %self.repository.as_str(),
                            "repository-watch polling attempt completed"
                        ),
                        Err(error) => tracing::warn!(
                            repository = %self.repository.as_str(),
                            cause_code = error.cause_code(),
                            "repository-watch polling attempt failed closed"
                        ),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
            select! {
                () = sleep(self.interval) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }

    async fn run_attempt(&mut self) -> Result<(), RepositoryWatchAttemptError> {
        if !self.rules_activated {
            self.activate_rules().await?;
            self.rules_activated = true;
        }
        self.process_dispatches().await?;
        self.poll_and_commit().await?;
        self.process_dispatches().await
    }

    async fn activate_rules(&self) -> Result<(), RepositoryWatchAttemptError> {
        let configured = self
            .rules
            .iter()
            .map(|rule| (rule.id().clone(), rule.version()))
            .collect::<Vec<_>>();
        self.dispatch_store
            .reconcile_rules(&self.repository, &configured)
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)
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
        let observation = self.poller.poll(previous).await?;
        let events = derive_repo_watch_events(
            &self.repository,
            previous,
            &observation,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|_| RepositoryWatchAttemptError::Differ)?;
        let outcome = self
            .store
            .commit(
                &self.repository,
                RepoWatchCommitRequest::new(
                    cursor.as_ref().map(|cursor| cursor.generation()),
                    RepoWatchCursorCandidate::new(observation),
                    events,
                ),
            )
            .await
            .map_err(|_| RepositoryWatchAttemptError::Persistence)?;
        match outcome {
            RepoWatchCommitOutcome::Committed(_)
            | RepoWatchCommitOutcome::Replayed(_)
            | RepoWatchCommitOutcome::Unchanged(_) => Ok(()),
            RepoWatchCommitOutcome::Conflict { current: _ } => {
                Err(RepositoryWatchAttemptError::Persistence)
            }
        }
    }
}

struct RepoWatchDispatchPersistence<'configuration> {
    store: PostgresRepoWatchDispatchStore,
    models: &'configuration HubModelConfiguration,
}

impl RepoWatchDispatchTransaction for RepoWatchDispatchPersistence<'_> {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        self.store
            .handle_repo_watch_evaluation_with_alias_resolver(evaluation, |alias: ModelAlias| {
                self.models.resolve_alias(alias)
            })
            .await
    }
}

fn dispatch_context_json(event: &RepoWatchEvent) -> String {
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
        })
        .to_string(),
        RepoWatchEventTarget::Branch => {
            let RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            } = event.kind()
            else {
                return event_value.to_string();
            };
            serde_json::json!({
                "type": "branch",
                "repo": event.repository().as_str(),
                "branch": branch.as_str(),
                "workflow": workflow.as_str(),
                "conclusion": check_conclusion_name(*conclusion),
                "event": event_value,
            })
            .to_string()
        }
    }
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
    Differ,
    Dispatch,
    Persistence,
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
            Self::Differ => "repository_differ_failed",
            Self::Dispatch => "repository_dispatch_failed",
            Self::Persistence => "repository_watch_persistence_failed",
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
    cache: PollCache,
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
            cache: PollCache::default(),
        })
    }

    async fn poll(
        &mut self,
        previous: Option<&RepoWatchObservation>,
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
        self.cache.begin_poll();
        let result = self.poll_complete(previous).await;
        if result.is_ok() {
            self.cache.complete_poll();
        }
        result
    }

    async fn poll_complete(
        &mut self,
        previous: Option<&RepoWatchObservation>,
    ) -> Result<RepoWatchObservation, RepositoryWatchAttemptError> {
        let mut pull_numbers = self.fetch_open_pull_numbers().await?;
        if let Some(previous) = previous {
            for pull_request in previous.state().pull_requests() {
                if pull_request.lifecycle() == RepoWatchPullRequestLifecycle::Open {
                    pull_numbers.insert(pull_request.context().number().get());
                }
            }
        }
        let mut pull_requests = Vec::with_capacity(pull_numbers.len());
        for number in pull_numbers {
            let previous_pull_request = previous.and_then(|observation| {
                observation
                    .state()
                    .pull_requests()
                    .iter()
                    .find(|pull_request| pull_request.context().number().get() == number)
            });
            pull_requests.push(
                self.fetch_pull_request(number, previous_pull_request)
                    .await?,
            );
        }
        let branch_heads = self.fetch_branch_heads().await?;
        let workflows = self.fetch_workflows().await?;
        let mut workflow_runs = Vec::new();
        for branch in &branch_heads {
            for workflow in &workflows {
                if let Some(run) = self.fetch_workflow_run(branch, workflow).await? {
                    workflow_runs.push(run);
                }
            }
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

    async fn fetch_open_pull_numbers(
        &mut self,
    ) -> Result<BTreeSet<u64>, RepositoryWatchAttemptError> {
        let mut numbers = BTreeSet::new();
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
            let values: Vec<PullNumberResponse> = self
                .conditional_json("pulls", Method::GET, url, None)
                .await?;
            let has_next = values.len() == PAGE_SIZE;
            for value in values {
                positive(value.number)?;
                numbers.insert(value.number);
            }
            if !has_next {
                return Ok(numbers);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_pull_request(
        &mut self,
        number: u64,
        previous_pull_request: Option<&RepoWatchPullRequestState>,
    ) -> Result<RepoWatchPullRequestState, RepositoryWatchAttemptError> {
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
        let completed_check_suites = self.fetch_check_suites(&head_sha).await?;
        let completed_check_runs = self.fetch_check_runs(&head_sha).await?;
        let reviews = self
            .fetch_reviews(
                number,
                previous_pull_request.map(RepoWatchPullRequestState::reviews),
            )
            .await?;
        let threads = self.fetch_threads(number).await?;
        let reactions = self.fetch_reactions(number).await?;
        RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context,
            lifecycle: normalize_lifecycle(&detail)?,
            mergeable_state: match detail.mergeable {
                Some(true) => MergeableState::Mergeable,
                Some(false) => MergeableState::Conflicting,
                None => MergeableState::Unknown,
            },
            completed_check_suites,
            completed_check_runs,
            reviews,
            threads,
            reactions,
        })
        .map_err(|_| RepositoryWatchAttemptError::Normalization)
    }

    async fn fetch_check_suites(
        &mut self,
        head: &CommitSha,
    ) -> Result<Vec<RepoWatchCheckSuiteObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: CheckSuitesResponse = self
                .conditional_json(
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
            let has_next = response.check_suites.len() == PAGE_SIZE;
            for suite in response.check_suites {
                if suite.status == "completed" {
                    observations.push(RepoWatchCheckSuiteObservation::new(
                        object_id(suite.id)?,
                        normalize_checks_outcome(suite.conclusion.as_deref())?,
                    ));
                }
            }
            if !has_next {
                return Ok(observations);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_check_runs(
        &mut self,
        head: &CommitSha,
    ) -> Result<Vec<RepoWatchCheckRunObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: CheckRunsResponse = self
                .conditional_json(
                    "check-runs",
                    Method::GET,
                    self.repository_url(
                        &["commits", head.as_str(), "check-runs"],
                        &[
                            ("filter", "all".to_owned()),
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.check_runs.len() == PAGE_SIZE;
            for run in response.check_runs {
                if run.status == "completed" {
                    observations.push(RepoWatchCheckRunObservation::new(
                        object_id(run.id)?,
                        CheckRunName::try_new(run.name)
                            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
                        normalize_conclusion(run.conclusion.as_deref())?,
                    ));
                }
            }
            if !has_next {
                return Ok(observations);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_reviews(
        &mut self,
        number: u64,
        previous: Option<&[RepoWatchReviewObservation]>,
    ) -> Result<Vec<RepoWatchReviewObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let number_text = number.to_string();
        let mut page = 1_u16;
        loop {
            let response: Vec<ReviewResponse> = self
                .conditional_json(
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
            let has_next = response.len() == PAGE_SIZE;
            for review in response {
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
        &mut self,
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
        &mut self,
        number: u64,
    ) -> Result<Vec<RepoWatchReactionObservation>, RepositoryWatchAttemptError> {
        if self.signal_reviewers.is_empty() {
            return Ok(Vec::new());
        }
        let number_text = number.to_string();
        let mut observations = self
            .fetch_reaction_pages(
                &["issues", &number_text, "reactions"],
                ReactionSubject::PullRequestBody,
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
                )
                .await?,
            );
        }
        Ok(observations)
    }

    async fn fetch_comment_ids(
        &mut self,
        resource_kind: &'static str,
        suffix: &[&str],
    ) -> Result<Vec<GitHubObjectId>, RepositoryWatchAttemptError> {
        let mut ids = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: Vec<CommentResponse> = self
                .conditional_json(
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
            let has_next = response.len() == PAGE_SIZE;
            for comment in response {
                ids.push(object_id(comment.id)?);
            }
            if !has_next {
                return Ok(ids);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_reaction_pages(
        &mut self,
        suffix: &[&str],
        subject: ReactionSubject,
    ) -> Result<Vec<RepoWatchReactionObservation>, RepositoryWatchAttemptError> {
        let mut observations = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: Vec<ReactionResponse> = self
                .conditional_json(
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
            let has_next = response.len() == PAGE_SIZE;
            for reaction in response {
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
        &mut self,
    ) -> Result<Vec<RepoWatchBranchHead>, RepositoryWatchAttemptError> {
        let mut heads = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: Vec<BranchResponse> = self
                .conditional_json(
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
            let has_next = response.len() == PAGE_SIZE;
            for branch in response {
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

    async fn fetch_workflows(
        &mut self,
    ) -> Result<Vec<WorkflowResponse>, RepositoryWatchAttemptError> {
        let mut workflows = Vec::new();
        let mut page = 1_u16;
        loop {
            let response: WorkflowsResponse = self
                .conditional_json(
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
            let has_next = response.workflows.len() == PAGE_SIZE;
            for workflow in &response.workflows {
                positive(workflow.id)?;
                WorkflowName::try_new(workflow.name.clone())
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
            }
            workflows.extend(response.workflows);
            if !has_next {
                return Ok(workflows);
            }
            page = next_page(page)?;
        }
    }

    async fn fetch_workflow_run(
        &mut self,
        branch: &RepoWatchBranchHead,
        workflow: &WorkflowResponse,
    ) -> Result<Option<RepoWatchWorkflowRunObservation>, RepositoryWatchAttemptError> {
        let workflow_id = workflow.id.to_string();
        let workflow_identity = object_id(workflow.id)?;
        let workflow_name = WorkflowName::try_new(workflow.name.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
        let mut page = 1_u16;
        loop {
            let response: WorkflowRunsResponse = self
                .conditional_json(
                    "workflow-runs",
                    Method::GET,
                    self.repository_url(
                        &["actions", "workflows", &workflow_id, "runs"],
                        &[
                            ("branch", branch.branch().as_str().to_owned()),
                            ("status", "completed".to_owned()),
                            ("per_page", PAGE_SIZE.to_string()),
                            ("page", page.to_string()),
                        ],
                    )?,
                    None,
                )
                .await?;
            let has_next = response.workflow_runs.len() == PAGE_SIZE;
            for run in response.workflow_runs {
                let run_id = object_id(run.id)?;
                let run_attempt = RepoWatchWorkflowRunAttempt::new(
                    NonZeroU64::new(run.run_attempt)
                        .ok_or(RepositoryWatchAttemptError::Normalization)?,
                );
                let Some(head_repository) = run.head_repository else {
                    continue;
                };
                let head_repository = RepositorySlug::try_new(head_repository.full_name)
                    .map_err(|_| RepositoryWatchAttemptError::Normalization)?;
                if head_repository == self.repository {
                    return Ok(Some(RepoWatchWorkflowRunObservation::new(
                        run_id,
                        workflow_identity,
                        run_attempt,
                        branch.branch().clone(),
                        workflow_name,
                        normalize_conclusion(run.conclusion.as_deref())?,
                    )));
                }
            }
            if !has_next {
                return Ok(None);
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

    async fn conditional_json<T>(
        &mut self,
        resource_kind: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T, RepositoryWatchAttemptError>
    where
        T: Any + Clone + DeserializeOwned + Send + Sync,
    {
        let key = ResourceKey::new(resource_kind, &method, &url, body.as_deref());
        self.cache.touch(key.clone())?;
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
        if let Some(entity_tag) = self.cache.entity_tag(&key) {
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
            let accepted = self.cache.accepted::<T>(&key)?;
            if let Some(entity_tag) = response.headers().get(ETAG).map(entity_tag).transpose()? {
                self.cache.replace_entity_tag(&key, entity_tag)?;
            }
            return Ok(accepted);
        }
        if response.status() != StatusCode::OK {
            return Err(RepositoryWatchAttemptError::Rejected);
        }
        self.cache.remove(&key);
        let response_entity_tag = response.headers().get(ETAG).map(entity_tag).transpose()?;
        let bytes = read_bounded(response, self.cache.remaining_poll_wire_bytes()?).await?;
        self.cache.record_poll_wire_bytes(bytes.len())?;
        let accepted = serde_json::from_slice::<T>(&bytes)
            .map_err(|_| RepositoryWatchAttemptError::InvalidResponse)?;
        match response_entity_tag {
            Some(entity_tag) => {
                self.cache
                    .insert(key, entity_tag, bytes.len(), accepted.clone())?;
            }
            None => self.cache.remove(&key),
        }
        Ok(accepted)
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

struct CachedResource {
    entity_tag: EntityTag,
    wire_bytes: usize,
    accepted: Box<dyn Any + Send + Sync>,
}

#[derive(Default)]
struct PollCache {
    resources: HashMap<ResourceKey, CachedResource>,
    touched: HashSet<ResourceKey>,
    requests: usize,
    poll_wire_bytes: usize,
    cached_wire_bytes: usize,
}

impl PollCache {
    fn begin_poll(&mut self) {
        self.touched.clear();
        self.requests = 0;
        self.poll_wire_bytes = 0;
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
        self.resources.retain(|key, _| self.touched.contains(key));
        self.cached_wire_bytes = self
            .resources
            .values()
            .map(|resource| resource.wire_bytes)
            .sum();
    }

    fn remaining_poll_wire_bytes(&self) -> Result<usize, RepositoryWatchAttemptError> {
        MAX_AGGREGATE_WIRE_BYTES
            .checked_sub(self.poll_wire_bytes)
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)
    }

    fn record_poll_wire_bytes(
        &mut self,
        wire_bytes: usize,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let projected = self
            .poll_wire_bytes
            .checked_add(wire_bytes)
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
        if projected > MAX_AGGREGATE_WIRE_BYTES {
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        self.poll_wire_bytes = projected;
        Ok(())
    }

    fn entity_tag(&self, key: &ResourceKey) -> Option<&EntityTag> {
        self.resources.get(key).map(|resource| &resource.entity_tag)
    }

    fn accepted<T: Any + Clone>(
        &self,
        key: &ResourceKey,
    ) -> Result<T, RepositoryWatchAttemptError> {
        self.resources
            .get(key)
            .and_then(|resource| resource.accepted.downcast_ref::<T>())
            .cloned()
            .ok_or(RepositoryWatchAttemptError::MissingCachedResource)
    }

    fn insert<T: Any + Send + Sync>(
        &mut self,
        key: ResourceKey,
        entity_tag: EntityTag,
        wire_bytes: usize,
        accepted: T,
    ) -> Result<(), RepositoryWatchAttemptError> {
        self.insert_with_resource_limit(key, entity_tag, wire_bytes, accepted, MAX_CACHED_RESOURCES)
    }

    fn insert_with_resource_limit<T: Any + Send + Sync>(
        &mut self,
        key: ResourceKey,
        entity_tag: EntityTag,
        wire_bytes: usize,
        accepted: T,
        resource_limit: usize,
    ) -> Result<(), RepositoryWatchAttemptError> {
        while !self.resources.contains_key(&key) && self.resources.len() >= resource_limit {
            if !self.evict_one_untouched() {
                break;
            }
        }
        if !self.resources.contains_key(&key) && self.resources.len() >= resource_limit {
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        let mut projected_bytes = self.projected_cached_bytes(&key, wire_bytes)?;
        while projected_bytes > MAX_AGGREGATE_WIRE_BYTES {
            if !self.evict_one_untouched() {
                break;
            }
            projected_bytes = self.projected_cached_bytes(&key, wire_bytes)?;
        }
        if projected_bytes > MAX_AGGREGATE_WIRE_BYTES {
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        self.resources.insert(
            key,
            CachedResource {
                entity_tag,
                wire_bytes,
                accepted: Box::new(accepted),
            },
        );
        self.cached_wire_bytes = projected_bytes;
        Ok(())
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

    fn replace_entity_tag(
        &mut self,
        key: &ResourceKey,
        entity_tag: EntityTag,
    ) -> Result<(), RepositoryWatchAttemptError> {
        let resource = self
            .resources
            .get_mut(key)
            .ok_or(RepositoryWatchAttemptError::MissingCachedResource)?;
        resource.entity_tag = entity_tag;
        Ok(())
    }

    fn remove(&mut self, key: &ResourceKey) {
        if let Some(resource) = self.resources.remove(key) {
            self.cached_wire_bytes = self.cached_wire_bytes.saturating_sub(resource.wire_bytes);
        }
    }
}

async fn read_bounded(
    mut response: Response,
    remaining_poll_bytes: usize,
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
        if next > MAX_RESPONSE_BYTES || next > remaining_poll_bytes {
            return Err(RepositoryWatchAttemptError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
        CheckConclusion::Success => Ok(ChecksOutcome::Success),
        CheckConclusion::Failure
        | CheckConclusion::Neutral
        | CheckConclusion::Cancelled
        | CheckConclusion::Skipped
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

#[derive(Clone, Deserialize)]
struct CheckSuiteResponse {
    id: u64,
    status: String,
    conclusion: Option<String>,
}

#[derive(Clone, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRunResponse>,
}

#[derive(Clone, Deserialize)]
struct CheckRunResponse {
    id: u64,
    status: String,
    name: String,
    conclusion: Option<String>,
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

#[derive(Clone, Deserialize)]
struct WorkflowResponse {
    id: u64,
    name: String,
}

#[derive(Clone, Deserialize)]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRunResponse>,
}

#[derive(Clone, Deserialize)]
struct WorkflowRunResponse {
    id: u64,
    run_attempt: u64,
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
    use std::{fs, path::PathBuf, sync::Arc, time::Duration};

    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Notify, watch},
        task::{JoinHandle, JoinSet},
    };

    use super::{
        CheckConclusion, ChecksOutcome, EntityTag, FileCredentialAccess, GitHubRepositoryPoller,
        MAX_AGGREGATE_WIRE_BYTES, MergeableState, PAGE_SIZE, PollCache, PullResponse,
        RepoWatchAuthorLogin, RepoWatchBranchHead, RepoWatchObservation,
        RepoWatchPullRequestLifecycle, RepoWatchReviewObservation, RepoWatchThreadState,
        RepositorySlug, RepositoryWatchAttemptError, RepositoryWatchRuntimeConstructionError,
        ResourceKey, ReviewState, Url, WorkflowResponse, dispatch_context_json,
        normalize_pull_request_context, object_id, supervise_repository_tasks,
    };
    use signalbox_domain::{
        BranchName, CommitSha, PullRequestBody, PullRequestEventContext,
        PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionSubject,
        RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindV1,
    };
    use signalbox_model_runtime::CredentialReference;

    const WATCHED_REPOSITORY: &str = "namespace/project";
    const CREDENTIAL_REFERENCE: &str = "repository-watch:namespace/project";
    const CREDENTIAL_FILE_NAME: &str = "watch-token";
    const CREDENTIAL_VALUE: &str = "fixture-token";
    const ENTITY_TAG: &str = "\"fixture-etag\"";
    const PULLS_TARGET: &str = "/repos/namespace/project/pulls?state=open&per_page=100&page=1";
    const BRANCHES_TARGET: &str = "/repos/namespace/project/branches?per_page=100&page=1";
    const WORKFLOWS_TARGET: &str = "/repos/namespace/project/actions/workflows?per_page=100&page=1";
    const SECOND_WORKFLOWS_PAGE_TARGET: &str =
        "/repos/namespace/project/actions/workflows?per_page=100&page=2";
    const PULL_DETAIL_TARGET: &str = "/repos/namespace/project/pulls/7";
    const CHECK_SUITES_TARGET: &str = "/repos/namespace/project/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-suites?filter=all&per_page=100&page=1";
    const CHECK_RUNS_TARGET: &str = "/repos/namespace/project/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs?filter=all&per_page=100&page=1";
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
    const MAIN_WORKFLOW_TARGET: &str = "/repos/namespace/project/actions/workflows/61/runs?branch=main&status=completed&per_page=100&page=1";
    const SECOND_MAIN_WORKFLOW_PAGE_TARGET: &str = "/repos/namespace/project/actions/workflows/61/runs?branch=main&status=completed&per_page=100&page=2";
    const FEATURE_WORKFLOW_TARGET: &str = "/repos/namespace/project/actions/workflows/61/runs?branch=feature%2Fwatch&status=completed&per_page=100&page=1";
    const EMPTY_LIST: &str = "[]";
    const EMPTY_WORKFLOW_LIST: &str = "{\"workflows\":[]}";
    const MALFORMED_JSON: &str = "not-json";
    const CACHE_RESOURCE_KEY: &str = "fixture/resource";
    const CACHE_RETAINED_KEY: &str = "fixture/retained";
    const CACHE_STALE_KEY: &str = "fixture/stale";
    const CACHE_REPLACEMENT_KEY: &str = "fixture/replacement";
    const TEST_CACHE_RESOURCE_LIMIT: usize = 2;
    const CACHE_WIRE_BYTES: usize = 1;
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
    const HEAD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BASE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD_REPOSITORY: &str = "fork/repository";
    const HEAD_BRANCH: &str = "feature/watch";
    const BASE_BRANCH: &str = "main";
    const PULL_TITLE: &str = "Exercise repository watch";
    const PULL_BODY: &str = "Typed fixture body";
    const PULL_AUTHOR: &str = "pull-author";
    const PULL_LABEL: &str = "watch-me";
    const CHECK_RUN_NAME: &str = "build";
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
        serde_json::json!([{ "number": PULL_NUMBERS[0] }]).to_string()
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
                    "conclusion": "success"
                },
                { "id": QUEUED_CHECK_SUITE_ID, "status": "queued", "conclusion": null }
            ]
        })
        .to_string()
    }

    fn check_runs() -> String {
        serde_json::json!({
            "check_runs": [
                {
                    "id": COMPLETED_CHECK_RUN_IDS[0],
                    "status": "completed",
                    "name": CHECK_RUN_NAME,
                    "conclusion": "failure"
                },
                {
                    "id": IN_PROGRESS_CHECK_RUN_ID,
                    "status": "in_progress",
                    "name": IN_PROGRESS_CHECK_RUN_NAME,
                    "conclusion": null
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
            "workflow_runs": [{
                "id": WORKFLOW_RUN_IDS[0],
                "run_attempt": WORKFLOW_RUN_ATTEMPT,
                "conclusion": "success",
                "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
            }]
        })
        .to_string()
    }

    fn feature_workflow_run() -> String {
        serde_json::json!({
            "workflow_runs": [{
                "id": WORKFLOW_RUN_IDS[1],
                "run_attempt": WORKFLOW_RUN_ATTEMPT,
                "conclusion": "failure",
                "head_repository": { "full_name": PROVIDER_BASE_REPOSITORY }
            }]
        })
        .to_string()
    }

    fn foreign_then_watched_workflow_runs() -> String {
        serde_json::json!({
            "workflow_runs": [
                {
                    "id": FOREIGN_WORKFLOW_RUN_ID,
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
                    "conclusion": "failure",
                    "head_repository": { "full_name": PROVIDER_HEAD_REPOSITORY }
                },
                {
                    "id": WORKFLOW_RUN_IDS[0],
                    "run_attempt": WORKFLOW_RUN_ATTEMPT,
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

    struct ScriptedResponse {
        method: &'static str,
        target: &'static str,
        validator: Option<&'static str>,
        status: &'static str,
        entity_tag: Option<&'static str>,
        body: String,
    }

    impl ScriptedResponse {
        fn ok(target: &'static str, body: impl Into<String>) -> Self {
            Self {
                method: "GET",
                target,
                validator: None,
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                body: body.into(),
            }
        }

        fn conditional_ok(target: &'static str, body: impl Into<String>) -> Self {
            Self {
                method: "GET",
                target,
                validator: Some(ENTITY_TAG),
                status: "200 OK",
                entity_tag: Some(ENTITY_TAG),
                body: body.into(),
            }
        }

        fn not_modified(target: &'static str) -> Self {
            Self {
                method: "GET",
                target,
                validator: Some(ENTITY_TAG),
                status: "304 Not Modified",
                entity_tag: None,
                body: String::new(),
            }
        }

        fn post(target: &'static str, body: impl Into<String>) -> Self {
            Self {
                method: "POST",
                target,
                validator: None,
                status: "200 OK",
                entity_tag: None,
                body: body.into(),
            }
        }
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

    async fn serve_response(listener: &TcpListener, response: ScriptedResponse) {
        let (mut stream, _) = listener.accept().await.expect("scripted request arrives");
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
        let request = String::from_utf8(request).expect("request headers are UTF-8");
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
        let entity_tag = response
            .entity_tag
            .map(|value| format!("ETag: {value}\r\n"))
            .unwrap_or_default();
        let encoded = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            entity_tag,
            response.body.len(),
            response.body,
        );
        stream
            .write_all(encoded.as_bytes())
            .await
            .expect("scripted response can be written");
    }

    struct PollerFixture {
        poller: GitHubRepositoryPoller,
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
            poller,
            _credential_directory: credential_directory,
        })
    }

    fn complete_poll_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(PULLS_TARGET, EMPTY_LIST),
            ScriptedResponse::ok(BRANCHES_TARGET, EMPTY_LIST),
            ScriptedResponse::ok(WORKFLOWS_TARGET, EMPTY_WORKFLOW_LIST),
        ]
    }

    fn conditional_poll_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::not_modified(PULLS_TARGET),
            ScriptedResponse::not_modified(BRANCHES_TARGET),
            ScriptedResponse::not_modified(WORKFLOWS_TARGET),
        ]
    }

    fn complete_typed_observation_responses() -> Vec<ScriptedResponse> {
        vec![
            ScriptedResponse::ok(PULLS_TARGET, pulls_with_one()),
            ScriptedResponse::ok(PULL_DETAIL_TARGET, pull_detail()),
            ScriptedResponse::ok(CHECK_SUITES_TARGET, check_suites()),
            ScriptedResponse::ok(CHECK_RUNS_TARGET, check_runs()),
            ScriptedResponse::ok(REVIEWS_TARGET, reviews()),
            ScriptedResponse::post(THREADS_TARGET, threads()),
            ScriptedResponse::ok(PULL_REACTIONS_TARGET, pull_reactions()),
            ScriptedResponse::ok(ISSUE_COMMENTS_TARGET, issue_comments()),
            ScriptedResponse::ok(ISSUE_COMMENT_REACTIONS_TARGET, issue_comment_reactions()),
            ScriptedResponse::ok(REVIEW_COMMENTS_TARGET, review_comments()),
            ScriptedResponse::ok(REVIEW_COMMENT_REACTIONS_TARGET, review_comment_reactions()),
            ScriptedResponse::ok(BRANCHES_TARGET, branches()),
            ScriptedResponse::ok(WORKFLOWS_TARGET, workflows()),
            ScriptedResponse::ok(MAIN_WORKFLOW_TARGET, main_workflow_run()),
            ScriptedResponse::ok(FEATURE_WORKFLOW_TARGET, feature_workflow_run()),
        ]
    }

    async fn complete_typed_observation() -> RepoWatchObservation {
        let server = ScriptedServer::start(complete_typed_observation_responses()).await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;
        observation
    }

    #[tokio::test]
    async fn shutdown_wins_when_a_repository_task_exits_cleanly_at_the_same_time() {
        let (sender, receiver) = watch::channel(false);
        let exit = Arc::new(Notify::new());
        let mut tasks = JoinSet::new();
        tasks.spawn({
            let exit = Arc::clone(&exit);
            async move { exit.notified().await }
        });
        let trigger = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sender
                .send(true)
                .expect("fixture supervisor still holds the shutdown receiver");
            exit.notify_one();
        });

        let result = supervise_repository_tasks(tasks, receiver).await;
        trigger.await.expect("fixture race trigger completes");

        assert_eq!(result, Ok(()));
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
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
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
        let mut first_poller =
            poller_fixture(server.base_url.clone()).expect("first poller is constructed");
        let first = first_poller
            .poller
            .poll(None)
            .await
            .expect("first poll succeeds");
        let mut restarted = poller_fixture(server.base_url.clone()).expect("poller restarts");
        let after_restart = restarted
            .poller
            .poll(Some(&first))
            .await
            .expect("restart performs a full poll");
        server.finish().await;

        assert_eq!(after_restart, first);
    }

    #[tokio::test]
    async fn workflow_listing_fetches_the_page_after_a_full_page() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(PULLS_TARGET, EMPTY_LIST),
            ScriptedResponse::ok(BRANCHES_TARGET, EMPTY_LIST),
            ScriptedResponse::ok(WORKFLOWS_TARGET, full_workflow_page()),
            ScriptedResponse::ok(SECOND_WORKFLOWS_PAGE_TARGET, EMPTY_WORKFLOW_LIST),
        ])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let observation = fixture.poller.poll(None).await.expect("full poll succeeds");
        server.finish().await;

        assert!(observation.state().workflow_runs().is_empty());
    }

    #[tokio::test]
    async fn branch_projection_skips_a_fork_run_with_the_same_branch_name() {
        let server = ScriptedServer::start(vec![ScriptedResponse::ok(
            MAIN_WORKFLOW_TARGET,
            foreign_then_watched_workflow_runs(),
        )])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();

        let run = fixture
            .poller
            .fetch_workflow_run(&branch, &workflow)
            .await
            .expect("workflow-run response is valid")
            .expect("watched-repository run remains in the response");
        server.finish().await;

        assert_eq!(run.id().get(), WORKFLOW_RUN_IDS[0]);
    }

    #[tokio::test]
    async fn branch_projection_follows_a_full_page_of_fork_runs() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(MAIN_WORKFLOW_TARGET, full_foreign_workflow_run_page()),
            ScriptedResponse::ok(SECOND_MAIN_WORKFLOW_PAGE_TARGET, main_workflow_run()),
        ])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
        let branch = base_branch_head();
        let workflow = workflow_response();

        let run = fixture
            .poller
            .fetch_workflow_run(&branch, &workflow)
            .await
            .expect("workflow-run pages are valid")
            .expect("watched-repository run is found on the next page");
        server.finish().await;

        assert_eq!(run.id().get(), WORKFLOW_RUN_IDS[0]);
    }

    #[tokio::test]
    async fn an_invalid_changed_response_invalidates_its_cached_resource_pair() {
        let responses = complete_poll_responses()
            .into_iter()
            .chain([ScriptedResponse::conditional_ok(
                PULLS_TARGET,
                MALFORMED_JSON,
            )])
            .chain([
                ScriptedResponse::ok(PULLS_TARGET, EMPTY_LIST),
                ScriptedResponse::not_modified(BRANCHES_TARGET),
                ScriptedResponse::not_modified(WORKFLOWS_TARGET),
            ])
            .collect();
        let server = ScriptedServer::start(responses).await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
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

    #[tokio::test]
    async fn a_complete_poll_normalizes_completed_check_runs() {
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
            REVIEWS_TARGET,
            identity_less_review(RETAINED_REVIEW_IDS[0]),
        )])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");
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
            REVIEWS_TARGET,
            identity_less_review(RETAINED_REVIEW_IDS[0]),
        )])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

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
        let mut fixture = poller_fixture_with_signal_reviewers(server.base_url.clone(), Vec::new())
            .expect("poller is constructed");

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER)
            .await
            .expect("empty signal-reviewer policy needs no reaction request");
        server.finish().await;

        assert!(reactions.is_empty());
    }

    #[tokio::test]
    async fn a_reaction_without_an_actor_identity_is_omitted() {
        let server = ScriptedServer::start(vec![
            ScriptedResponse::ok(PULL_REACTIONS_TARGET, identity_less_reaction()),
            ScriptedResponse::ok(ISSUE_COMMENTS_TARGET, EMPTY_LIST),
            ScriptedResponse::ok(REVIEW_COMMENTS_TARGET, EMPTY_LIST),
        ])
        .await;
        let mut fixture = poller_fixture(server.base_url.clone()).expect("poller is constructed");

        let reactions = fixture
            .poller
            .fetch_reactions(PULL_NUMBER)
            .await
            .expect("identity-less reaction is safely omitted");
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
    fn process_local_cache_rejects_a_resource_beyond_its_total_wire_bound() {
        let mut cache = PollCache::default();
        let result = cache.insert(
            ResourceKey(CACHE_RESOURCE_KEY.to_owned()),
            EntityTag(ENTITY_TAG.to_owned()),
            MAX_AGGREGATE_WIRE_BYTES + 1,
            Vec::<u8>::new(),
        );

        assert_eq!(result, Err(RepositoryWatchAttemptError::ResourceLimit));
    }

    #[test]
    fn process_local_cache_replaces_an_untouched_stale_entry_at_capacity() {
        let mut cache = PollCache::default();
        let retained = ResourceKey(CACHE_RETAINED_KEY.to_owned());
        let stale = ResourceKey(CACHE_STALE_KEY.to_owned());
        let replacement = ResourceKey(CACHE_REPLACEMENT_KEY.to_owned());
        cache
            .insert_with_resource_limit(
                retained.clone(),
                EntityTag(ENTITY_TAG.to_owned()),
                CACHE_WIRE_BYTES,
                Vec::<u8>::new(),
                TEST_CACHE_RESOURCE_LIMIT,
            )
            .expect("retained cache fixture is admitted");
        cache
            .insert_with_resource_limit(
                stale.clone(),
                EntityTag(ENTITY_TAG.to_owned()),
                CACHE_WIRE_BYTES,
                Vec::<u8>::new(),
                TEST_CACHE_RESOURCE_LIMIT,
            )
            .expect("stale cache fixture is admitted");
        cache.begin_poll();
        cache
            .touch(retained.clone())
            .expect("retained cache fixture is touched");
        cache
            .touch(replacement.clone())
            .expect("replacement cache fixture is touched");

        cache
            .insert_with_resource_limit(
                replacement.clone(),
                EntityTag(ENTITY_TAG.to_owned()),
                CACHE_WIRE_BYTES,
                Vec::<u8>::new(),
                TEST_CACHE_RESOURCE_LIMIT,
            )
            .expect("replacement evicts the untouched stale fixture");

        assert_eq!(cache.resources.len(), TEST_CACHE_RESOURCE_LIMIT);
        assert!(cache.resources.contains_key(&retained));
        assert!(cache.resources.contains_key(&replacement));
        assert!(!cache.resources.contains_key(&stale));
    }

    #[test]
    fn one_poll_rejects_response_bytes_beyond_its_aggregate_wire_bound() {
        let mut cache = PollCache::default();
        cache.begin_poll();
        cache
            .record_poll_wire_bytes(MAX_AGGREGATE_WIRE_BYTES)
            .expect("exact aggregate wire bound is accepted");

        assert_eq!(
            cache.record_poll_wire_bytes(1),
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
}
