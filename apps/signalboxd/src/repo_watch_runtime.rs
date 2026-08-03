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
    RepoWatchBranchHead, RepoWatchCheckRunObservation, RepoWatchCheckSuiteObservation,
    RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchThreadObservation,
    RepoWatchThreadState, RepoWatchWorkflowRunObservation, UuidV7RepoWatchEventIdGenerator,
    derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionContent, ReactionSubject, RepoWatchAuthorLogin,
    RepositorySlug, ReviewState, ReviewThreadId, WorkflowName,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_persistence::repo_watch::{
    PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
    RepoWatchCursorCandidate,
};
use sqlx::PgPool;
use tokio::{select, sync::watch, task::JoinSet, time::sleep};

use crate::configuration::{
    FileCredentialAccess, RepositoryWatchConfiguration, WatchedRepositoryConfiguration,
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
  $owner: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $owner, name: $name) {
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
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let mut tasks = Vec::with_capacity(configuration.repositories().len());
        for repository in configuration.repositories() {
            tasks.push(RepositoryWatchTask::try_new(
                pool.clone(),
                repository,
                configuration.signal_reviewers().to_vec(),
            )?);
        }
        Ok(Self { tasks })
    }

    /// Runs every repository task until the daemon broadcasts shutdown.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepositoryWatchRuntimeError> {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut tasks = JoinSet::new();
        for task in self.tasks {
            tasks.spawn(task.run(shutdown.clone()));
        }
        loop {
            select! {
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
                        Some(Ok(())) => Err(RepositoryWatchRuntimeError::RepositoryTaskExited),
                        Some(Err(_)) => Err(RepositoryWatchRuntimeError::RepositoryTaskPanicked),
                        None => Err(RepositoryWatchRuntimeError::TaskSetEmpty),
                    };
                }
            }
        }
    }
}

struct RepositoryWatchTask {
    repository: RepositorySlug,
    interval: Duration,
    poller: GitHubRepositoryPoller,
    store: PostgresRepoWatchStore,
}

impl RepositoryWatchTask {
    fn try_new(
        pool: PgPool,
        configuration: &WatchedRepositoryConfiguration,
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
    ) -> Result<Self, RepositoryWatchRuntimeConstructionError> {
        let credential_reference = configuration.credential_reference();
        let credentials = FileCredentialAccess::new_bounded(
            configuration.credential_file().to_path_buf(),
            credential_reference.clone(),
            MAX_CREDENTIAL_BYTES,
        );
        Ok(Self {
            repository: configuration.repository().clone(),
            interval: configuration.poll_interval(),
            poller: GitHubRepositoryPoller::try_new(
                configuration.repository().clone(),
                signal_reviewers,
                credentials,
                credential_reference,
            )?,
            store: PostgresRepoWatchStore::new(pool),
        })
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            select! {
                result = self.poll_and_commit() => {
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
            pull_requests.push(self.fetch_pull_request(number).await?);
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
        let context = normalize_pull_request_context(&detail, head_sha.clone())?;
        let completed_check_suites = self.fetch_check_suites(&head_sha).await?;
        let completed_check_runs = self.fetch_check_runs(&head_sha).await?;
        let reviews = self.fetch_reviews(number).await?;
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
                observations.push(RepoWatchReviewObservation::new(
                    object_id(review.id)?,
                    RepoWatchAuthorLogin::try_new(review.user.login)
                        .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
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
        let (owner, name) = self
            .repository
            .as_str()
            .split_once('/')
            .ok_or(RepositoryWatchAttemptError::Normalization)?;
        let owner = owner.to_owned();
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
                    owner: &owner,
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
                if let Some(reactor) = self.signal_reviewer(&reaction.user.login) {
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
        let response: WorkflowRunsResponse = self
            .conditional_json(
                "workflow-runs",
                Method::GET,
                self.repository_url(
                    &["actions", "workflows", &workflow_id, "runs"],
                    &[
                        ("branch", branch.branch().as_str().to_owned()),
                        ("status", "completed".to_owned()),
                        ("per_page", "1".to_owned()),
                    ],
                )?,
                None,
            )
            .await?;
        let Some(run) = response.workflow_runs.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(RepoWatchWorkflowRunObservation::new(
            object_id(run.id)?,
            branch.branch().clone(),
            WorkflowName::try_new(workflow.name.clone())
                .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
            normalize_conclusion(run.conclusion.as_deref())?,
        )))
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
            let (owner, name) = self
                .repository
                .as_str()
                .split_once('/')
                .ok_or(RepositoryWatchAttemptError::Normalization)?;
            segments.push(owner);
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
        if !self.resources.contains_key(&key) && self.resources.len() >= MAX_CACHED_RESOURCES {
            return Err(RepositoryWatchAttemptError::ResourceLimit);
        }
        let replaced_bytes = self
            .resources
            .get(&key)
            .map_or(0, |resource| resource.wire_bytes);
        let projected_bytes = self
            .cached_wire_bytes
            .checked_sub(replaced_bytes)
            .and_then(|retained| retained.checked_add(wire_bytes))
            .ok_or(RepositoryWatchAttemptError::ResourceLimit)?;
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
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(positive(response.number)?),
        head_sha,
        head_repository: RepositorySlug::try_new(response.head.repo.full_name.clone())
            .map_err(|_| RepositoryWatchAttemptError::Normalization)?,
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
    repo: RepositoryResponse,
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
    user: UserResponse,
    state: String,
    commit_id: String,
}

#[derive(Clone, Deserialize)]
struct CommentResponse {
    id: u64,
}

#[derive(Clone, Deserialize)]
struct ReactionResponse {
    user: UserResponse,
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
    conclusion: Option<String>,
}

#[derive(Serialize)]
struct GraphQlRequest<T> {
    query: &'static str,
    variables: T,
}

#[derive(Serialize)]
struct ThreadVariables<'a> {
    owner: &'a str,
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
    use std::{fs, path::PathBuf, time::Duration};

    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::{
        CheckConclusion, ChecksOutcome, EntityTag, FileCredentialAccess, GitHubRepositoryPoller,
        MAX_AGGREGATE_WIRE_BYTES, MergeableState, PAGE_SIZE, PollCache, RepoWatchAuthorLogin,
        RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchThreadState, RepositorySlug,
        RepositoryWatchAttemptError, RepositoryWatchRuntimeConstructionError, ResourceKey,
        ReviewState, Url, object_id,
    };
    use signalbox_domain::ReactionSubject;
    use signalbox_model_runtime::CredentialReference;

    const WATCHED_REPOSITORY: &str = "owner/repository";
    const CREDENTIAL_REFERENCE: &str = "repository-watch:owner/repository";
    const CREDENTIAL_FILE_NAME: &str = "watch-token";
    const CREDENTIAL_VALUE: &str = "fixture-token";
    const ENTITY_TAG: &str = "\"fixture-etag\"";
    const PULLS_TARGET: &str = "/repos/owner/repository/pulls?state=open&per_page=100&page=1";
    const BRANCHES_TARGET: &str = "/repos/owner/repository/branches?per_page=100&page=1";
    const WORKFLOWS_TARGET: &str = "/repos/owner/repository/actions/workflows?per_page=100&page=1";
    const SECOND_WORKFLOWS_PAGE_TARGET: &str =
        "/repos/owner/repository/actions/workflows?per_page=100&page=2";
    const PULL_DETAIL_TARGET: &str = "/repos/owner/repository/pulls/7";
    const CHECK_SUITES_TARGET: &str = "/repos/owner/repository/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-suites?per_page=100&page=1";
    const CHECK_RUNS_TARGET: &str = "/repos/owner/repository/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs?per_page=100&page=1";
    const REVIEWS_TARGET: &str = "/repos/owner/repository/pulls/7/reviews?per_page=100&page=1";
    const THREADS_TARGET: &str = "/graphql";
    const PULL_REACTIONS_TARGET: &str =
        "/repos/owner/repository/issues/7/reactions?per_page=100&page=1";
    const ISSUE_COMMENTS_TARGET: &str =
        "/repos/owner/repository/issues/7/comments?per_page=100&page=1";
    const ISSUE_COMMENT_REACTIONS_TARGET: &str =
        "/repos/owner/repository/issues/comments/41/reactions?per_page=100&page=1";
    const REVIEW_COMMENTS_TARGET: &str =
        "/repos/owner/repository/pulls/7/comments?per_page=100&page=1";
    const REVIEW_COMMENT_REACTIONS_TARGET: &str =
        "/repos/owner/repository/pulls/comments/51/reactions?per_page=100&page=1";
    const MAIN_WORKFLOW_TARGET: &str =
        "/repos/owner/repository/actions/workflows/61/runs?branch=main&status=completed&per_page=1";
    const FEATURE_WORKFLOW_TARGET: &str = "/repos/owner/repository/actions/workflows/61/runs?branch=feature%2Fwatch&status=completed&per_page=1";
    const EMPTY_LIST: &str = "[]";
    const EMPTY_WORKFLOW_LIST: &str = "{\"workflows\":[]}";
    const MALFORMED_JSON: &str = "not-json";
    const CACHE_RESOURCE_KEY: &str = "fixture/resource";
    const CACHE_KEY_KIND: &str = "fixture-page";
    const CACHE_KEY_QUERY_VALUE: &str = "provider-controlled-branch";
    const CACHE_KEY_URL: &str =
        "https://api.github.com/repos/owner/repository/runs?branch=provider-controlled-branch";
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
    const PROVIDER_HEAD_REPOSITORY: &str = "Fork/Repository";
    const PROVIDER_BASE_REPOSITORY: &str = "Owner/Repository";
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
            "workflow_runs": [{ "id": WORKFLOW_RUN_IDS[0], "conclusion": "success" }]
        })
        .to_string()
    }

    fn feature_workflow_run() -> String {
        serde_json::json!({
            "workflow_runs": [{ "id": WORKFLOW_RUN_IDS[1], "conclusion": "failure" }]
        })
        .to_string()
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
        let reviewer = RepoWatchAuthorLogin::try_new("signal-reviewer".to_owned())
            .expect("reviewer fixture is valid");
        let poller = GitHubRepositoryPoller::try_new_with_rest_base(
            repository,
            vec![reviewer],
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
}
