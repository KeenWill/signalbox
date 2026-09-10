//! GitHub observation composition for the repository task.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
};

use serde_json::{Value, json};
use signalbox_session_ownership::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, MergeableState,
    OffsetDateTime, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionContent, ReactionSubject, RepoWatchAuthorLogin,
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchMergedPullRequestBaselineV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchReactionObservation, RepoWatchRepositoryState, RepoWatchRepositoryStateError,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchThreadObservation,
    RepoWatchWorkflowRunAttempt, RepoWatchWorkflowRunObservation, RepositorySlug, ReviewState,
    ReviewThreadId, WorkflowName,
};

use crate::{
    EventProducer, RepoWatchStore, StoreError,
    github::{GitHubClient, GitHubClientError},
    ingest::{RepositoryObservation, RepositoryTask},
    observation_decode::{array, conclusion, object_id, positive, text},
};

// GitHub's maximum REST page size, used only to select complete provider pages.
pub(crate) const PAGE_SIZE: u16 = 100;
// One attempt may consume at most one fifth of the authenticated user's
// 5,000-request REST allowance, counting GraphQL requests against the same ceiling.
const MAX_OBSERVATION_REQUESTS: usize = 1_000;
// GitHub's commit check-run endpoint includes only its 1,000 most recent suites:
// https://docs.github.com/en/rest/checks/runs#list-check-runs-for-a-git-reference
const COMMIT_CHECK_SUITE_LIMIT: usize = 1_000;
// GitHub caps parameterized workflow-run searches at 1,000 results:
// https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository
const WORKFLOW_RUN_SEARCH_LIMIT: u64 = 1_000;
const THREADS_QUERY: &str = r#"
query RepositoryWatchReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        nodes {
          id
          isResolved
          resolvedBy { login }
          comments(first: 1) { nodes { author { login } } }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

/// Provider failures retain request context without response bodies or credentials.
#[derive(Debug)]
pub enum ObservationError {
    Cache(StoreError),
    Transport(GitHubClientError),
    InvalidResponse,
    InvalidState {
        repository: RepositorySlug,
        pull_request: Option<PullRequestNumber>,
        source: RepoWatchRepositoryStateError,
    },
    RequestBudgetExceeded {
        limit: usize,
    },
    RestBudgetUnavailable {
        required: usize,
        remaining: u64,
    },
    CheckSuiteLimitExceeded {
        limit: usize,
    },
    WorkflowRunLimitExceeded {
        limit: u64,
    },
    Request {
        path: String,
        source: Box<ObservationError>,
    },
}

impl ObservationError {
    fn at(self, path: &str) -> Self {
        Self::Request {
            path: path.to_owned(),
            source: Box::new(self),
        }
    }
}

impl fmt::Display for ObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cache(error) => write!(f, "{error}"),
            Self::Transport(error) => write!(f, "{error}"),
            Self::InvalidState {
                repository,
                pull_request,
                source,
            } => write!(
                f,
                "repository-watch observation {} pull_request={pull_request:?}: {source}",
                repository.as_str()
            ),
            Self::WorkflowRunLimitExceeded { limit } => write!(
                f,
                "workflow run search exceeds GitHub's {limit}-result limit"
            ),
            Self::CheckSuiteLimitExceeded { limit } => write!(
                f,
                "commit check-run inventory exceeds GitHub's {limit}-suite limit"
            ),
            Self::RestBudgetUnavailable {
                required,
                remaining,
            } => write!(
                f,
                "repository-watch REST budget requires {required} requests; {remaining} remain"
            ),
            Self::RequestBudgetExceeded { limit } => write!(
                f,
                "repository-watch observation request budget exhausted after {limit} requests"
            ),
            Self::InvalidResponse => {
                f.write_str("repository-watch provider observation is invalid")
            }
            Self::Request { path, source } => write!(f, "{path}: {source}"),
        }
    }
}
impl Error for ObservationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Cache(error) => Some(error),
            Self::InvalidState { source, .. } => Some(source),
            Self::Request { source, .. } => Some(source),
            Self::InvalidResponse
            | Self::RestBudgetUnavailable { .. }
            | Self::RequestBudgetExceeded { .. }
            | Self::CheckSuiteLimitExceeded { .. }
            | Self::WorkflowRunLimitExceeded { .. } => None,
        }
    }
}

struct ResponseValue {
    value: Value,
    path: String,
}

impl std::ops::Deref for ResponseValue {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}

impl ResponseValue {
    fn admit<T>(&self, value: Option<T>) -> Result<T, ObservationError> {
        value.ok_or_else(|| ObservationError::InvalidResponse.at(&self.path))
    }
    fn text(&self, value: &Value) -> Result<String, ObservationError> {
        self.admit(text(value))
    }
    fn invalid(&self) -> ObservationError {
        ObservationError::InvalidResponse.at(&self.path)
    }
}

async fn read_page(
    io: &impl GitHubObservationRead,
    path: &str,
) -> Result<(ResponseValue, bool), ObservationError> {
    let (value, next) = io.page(path).await.map_err(|error| error.at(path))?;
    Ok((
        ResponseValue {
            value,
            path: path.to_owned(),
        },
        next,
    ))
}

/// The external read capability used to build a complete observation.
pub trait GitHubObservationRead: Send + Sync {
    fn page(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<(Value, bool), ObservationError>> + Send;
    fn threads(
        &self,
        request: Value,
    ) -> impl Future<Output = Result<Value, ObservationError>> + Send;
}

impl GitHubObservationRead for GitHubClient {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        let (body, next) = self
            .get_page(path)
            .await
            .map_err(ObservationError::Transport)?;
        Ok((
            serde_json::from_slice(&body)
                .map_err(|_| ObservationError::InvalidResponse.at(path))?,
            next,
        ))
    }

    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        let body = self
            .graphql(request.to_string().into_bytes())
            .await
            .map_err(ObservationError::Transport)?;
        serde_json::from_slice(&body).map_err(|_| ObservationError::InvalidResponse.at("/graphql"))
    }
}

struct ObservationReadBudget<'a, T> {
    io: &'a T,
    requests: std::sync::atomic::AtomicUsize,
}

impl<T> ObservationReadBudget<'_, T> {
    fn reserve(&self, path: &str) -> Result<(), ObservationError> {
        self.requests
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |count| (count < MAX_OBSERVATION_REQUESTS).then_some(count + 1),
            )
            .map(|_| ())
            .map_err(|_| {
                ObservationError::RequestBudgetExceeded {
                    limit: MAX_OBSERVATION_REQUESTS,
                }
                .at(path)
            })
    }
}

impl<T: GitHubObservationRead> GitHubObservationRead for ObservationReadBudget<'_, T> {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        self.reserve(path)?;
        self.io.page(path).await
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        self.reserve("/graphql")?;
        self.io.threads(request).await
    }
}

/// Resolves credentials outside the module and exposes only a client handle.
pub trait RepositoryClientLoader: Send {
    type Error;
    fn load_client(&self) -> impl Future<Output = Result<GitHubClient, Self::Error>> + Send;
}

/// A repository's provider fetch and durable ingest composed as one attempt.
pub struct GitHubRepositoryTask<Loader> {
    pub repository: RepositorySlug,
    pub signal_reviewers: Vec<RepoWatchAuthorLogin>,
    pub subject_retention: std::time::Duration,
    pub poll_request_budget: std::num::NonZeroUsize,
    pub clients: Loader,
    pub store: RepoWatchStore,
}

/// A repository attempt's credential, external-read, or durable-ingest failure.
#[derive(Debug)]
pub enum RepositoryAttemptError<E> {
    Client(E),
    Observation(ObservationError),
    Store(StoreError),
    FrontierConflict,
}

impl<Loader: RepositoryClientLoader> RepositoryTask for GitHubRepositoryTask<Loader>
where
    Loader::Error: fmt::Display,
{
    type Error = RepositoryAttemptError<Loader::Error>;

    async fn poll(&mut self, producer: EventProducer) -> Result<(), Self::Error> {
        self.poll_outcome(producer).await.map(|_| ())
    }
}

impl<Loader: RepositoryClientLoader> GitHubRepositoryTask<Loader> {
    /// Runs the existing bounded attempt and retains its measured outcome.
    pub async fn poll_outcome(
        &mut self,
        producer: EventProducer,
    ) -> Result<crate::measurements::PollOutcome, RepositoryAttemptError<Loader::Error>> {
        let attempt = crate::measurements::AttemptGuard::start(
            self.store.measurements.clone(),
            self.repository.clone(),
        );
        let result = self.observe(producer).await;
        use crate::measurements::PollOutcome;
        attempt.finish(match &result {
            Ok(outcome) => *outcome,
            Err(RepositoryAttemptError::Client(_)) => PollOutcome::ClientFailed,
            Err(RepositoryAttemptError::Observation(_)) => PollOutcome::ObservationFailed,
            Err(RepositoryAttemptError::Store(_)) => PollOutcome::StoreFailed,
            Err(RepositoryAttemptError::FrontierConflict) => PollOutcome::FrontierConflict,
        });
        result
    }
}

impl<Loader: RepositoryClientLoader> GitHubRepositoryTask<Loader> {
    async fn observe(
        &mut self,
        producer: EventProducer,
    ) -> Result<crate::measurements::PollOutcome, RepositoryAttemptError<Loader::Error>> {
        let client = self
            .clients
            .load_client()
            .await
            .map_err(RepositoryAttemptError::Client)?;
        if producer == EventProducer::Webhook {
            return crate::poll_cache::observe_webhook_pulls(
                &client,
                &self.store,
                &self.repository,
                &self.signal_reviewers,
                self.subject_retention,
            )
            .await
            .map_err(|error| match error {
                ObservationError::Cache(error) => RepositoryAttemptError::Store(error),
                error => RepositoryAttemptError::Observation(error),
            });
        }
        crate::poll_cache::poll_with_cache(
            &client,
            &self.store,
            &self.repository,
            &self.signal_reviewers,
            self.subject_retention,
            self.poll_request_budget,
        )
        .await
        .map(|complete| {
            if complete {
                crate::measurements::PollOutcome::Succeeded
            } else {
                crate::measurements::PollOutcome::Partial
            }
        })
        .map_err(|error| match error {
            ObservationError::Cache(error) => RepositoryAttemptError::Store(error),
            error => RepositoryAttemptError::Observation(error),
        })
    }
}

impl<E: fmt::Display> fmt::Display for RepositoryAttemptError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(f, "repository-watch client: {error}"),
            Self::Observation(error) => write!(f, "repository-watch observation: {error}"),
            Self::Store(error) => write!(f, "repository-watch store: {error}"),
            Self::FrontierConflict => f.write_str("repository-watch frontier conflict"),
        }
    }
}

/// Fetches current open PRs and retained subjects, branch heads, and workflow completions.
pub async fn fetch_observation(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    previous: Option<&RepoWatchObservation>,
    merged_baselines: &[RepoWatchMergedPullRequestBaselineV1],
) -> Result<RepositoryObservation, ObservationError> {
    let budget = ObservationReadBudget {
        io,
        requests: std::sync::atomic::AtomicUsize::new(0),
    };
    let io = &budget;
    let root = format!("/repos/{}", repository.as_str());
    let (metadata, _) = read_page(io, &root).await?;
    let default_branch =
        metadata.admit(BranchName::try_new(metadata.text(&metadata["default_branch"])?).ok())?;
    let branch_heads = pages(io, &format!("{root}/branches"), None)
        .await?
        .iter()
        .map(|v| {
            v.admit(Some(RepoWatchBranchHead::new(
                v.admit(BranchName::try_new(v.text(&v["name"])?).ok())?,
                v.admit(CommitSha::try_new(v.text(&v["commit"]["sha"])?).ok())?,
            )))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let default_head = metadata
        .admit(
            branch_heads
                .iter()
                .find(|branch| branch.branch() == &default_branch),
        )?
        .head()
        .clone();
    let mut numbers = pages(io, &format!("{root}/pulls?state=open"), None)
        .await?
        .iter()
        .map(|v| v.admit(positive(&v["number"]).map(PullRequestNumber::new)))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if let Some(previous) = previous {
        numbers.extend(
            previous
                .state()
                .pull_requests()
                .iter()
                .map(|p| p.context().number()),
        );
    }
    let mut pulls = Vec::new();
    let mut merged_at = BTreeMap::new();
    for number in numbers {
        let prior = previous.and_then(|p| {
            p.state()
                .pull_requests()
                .iter()
                .find(|p| p.context().number() == number)
        });
        let merged = merged_baselines
            .iter()
            .find(|baseline| baseline.number() == number);
        let (pull, merge_time) =
            fetch_pull(io, &root, repository, number, reviewers, prior, merged).await?;
        if let Some(merge_time) = merge_time {
            merged_at.insert(number, merge_time);
        }
        pulls.push(pull);
    }
    let retained_branches = branch_heads
        .iter()
        .filter(|branch| {
            branch.branch() == &default_branch
                || pulls.iter().any(|pull| {
                    pull.lifecycle() == RepoWatchPullRequestLifecycle::Open
                        && pull.context().head_repository() == repository
                        && branch.branch() == pull.context().head_branch()
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let workflow_runs =
        fetch_workflows(io, &root, repository, &retained_branches, previous).await?;
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: pulls,
        branch_heads,
        workflow_runs,
    })
    .map_err(|source| ObservationError::InvalidState {
        repository: repository.clone(),
        pull_request: None,
        source,
    })?;
    Ok(RepositoryObservation {
        repository: repository.clone(),
        default_branch,
        default_head,
        observation: RepoWatchObservation::new(reviewers.to_vec(), state),
        merged_at,
        observed_at: OffsetDateTime::now_utc(),
    })
}

/// Refreshes one pull request while retaining all other committed comparison subjects.
pub async fn fetch_pull_observation(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    baseline: &crate::ingest::IngestBaseline,
    number: PullRequestNumber,
) -> Result<RepositoryObservation, ObservationError> {
    let budget = ObservationReadBudget {
        io,
        requests: std::sync::atomic::AtomicUsize::new(0),
    };
    fetch_poll_pull(&budget, repository, reviewers, baseline, number).await
}

pub(crate) async fn fetch_poll_pull(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    baseline: &crate::ingest::IngestBaseline,
    number: PullRequestNumber,
) -> Result<RepositoryObservation, ObservationError> {
    let root = format!("/repos/{}", repository.as_str());
    let default_branch = if let Some(branch) = &baseline.default_branch {
        branch.clone()
    } else {
        let (metadata, _) = read_page(io, &root).await?;
        metadata.admit(BranchName::try_new(metadata.text(&metadata["default_branch"])?).ok())?
    };
    let previous = baseline.observation.as_ref();
    let mut branch_heads = previous
        .map(|prior| prior.state().branch_heads().to_vec())
        .unwrap_or_default();
    if !branch_heads
        .iter()
        .any(|branch| branch.branch() == &default_branch)
    {
        branch_heads = pages(io, &format!("{root}/branches"), None)
            .await?
            .iter()
            .map(|v| {
                Ok(RepoWatchBranchHead::new(
                    v.admit(BranchName::try_new(v.text(&v["name"])?).ok())?,
                    v.admit(CommitSha::try_new(v.text(&v["commit"]["sha"])?).ok())?,
                ))
            })
            .collect::<Result<Vec<_>, ObservationError>>()?;
    }
    let default_head = branch_heads
        .iter()
        .find(|branch| branch.branch() == &default_branch)
        .ok_or(ObservationError::InvalidResponse)?
        .head()
        .clone();
    let mut pulls = previous
        .map(|prior| prior.state().pull_requests().to_vec())
        .unwrap_or_default();
    let prior = pulls.iter().find(|pull| pull.context().number() == number);
    let merged = baseline
        .merged_baselines
        .iter()
        .find(|entry| entry.state.number() == number)
        .map(|entry| &entry.state);
    let (pull, merge_time) =
        fetch_pull(io, &root, repository, number, reviewers, prior, merged).await?;
    pulls.retain(|pull| pull.context().number() != number);
    pulls.push(pull);
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: pulls,
        branch_heads,
        workflow_runs: previous
            .map(|prior| prior.state().workflow_runs().to_vec())
            .unwrap_or_default(),
    })
    .map_err(|source| ObservationError::InvalidState {
        repository: repository.clone(),
        pull_request: Some(number),
        source,
    })?;
    Ok(RepositoryObservation {
        repository: repository.clone(),
        default_branch,
        default_head,
        observation: RepoWatchObservation::new(reviewers.to_vec(), state),
        merged_at: merge_time.map(|time| (number, time)).into_iter().collect(),
        observed_at: OffsetDateTime::now_utc(),
    })
}

async fn fetch_repository_heads(
    io: &impl GitHubObservationRead,
    root: &str,
) -> Result<(BranchName, CommitSha, Vec<RepoWatchBranchHead>), ObservationError> {
    let (metadata, _) = read_page(io, root).await?;
    let default_branch =
        metadata.admit(BranchName::try_new(metadata.text(&metadata["default_branch"])?).ok())?;
    let branch_heads = pages(io, &format!("{root}/branches"), None)
        .await?
        .iter()
        .map(|v| {
            Ok(RepoWatchBranchHead::new(
                v.admit(BranchName::try_new(v.text(&v["name"])?).ok())?,
                v.admit(CommitSha::try_new(v.text(&v["commit"]["sha"])?).ok())?,
            ))
        })
        .collect::<Result<Vec<_>, ObservationError>>()?;
    let default_head = metadata
        .admit(
            branch_heads
                .iter()
                .find(|branch| branch.branch() == &default_branch),
        )?
        .head()
        .clone();
    Ok((default_branch, default_head, branch_heads))
}

pub(crate) async fn fetch_poll_repository(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    baseline: &crate::ingest::IngestBaseline,
) -> Result<(RepositoryObservation, Vec<PullRequestNumber>), ObservationError> {
    let root = format!("/repos/{}", repository.as_str());
    let (default_branch, default_head, branch_heads) = if let Some(prior) = &baseline.observation {
        (
            baseline
                .default_branch
                .clone()
                .ok_or(ObservationError::InvalidResponse)?,
            baseline
                .default_head
                .clone()
                .ok_or(ObservationError::InvalidResponse)?,
            prior.state().branch_heads().to_vec(),
        )
    } else {
        fetch_repository_heads(io, &root).await?
    };
    let previous = baseline.observation.as_ref();
    let pulls = previous
        .map(|prior| prior.state().pull_requests().to_vec())
        .unwrap_or_default();
    let mut numbers = pages(io, &format!("{root}/pulls?state=open"), None)
        .await?
        .iter()
        .map(|v| v.admit(positive(&v["number"]).map(PullRequestNumber::new)))
        .collect::<Result<BTreeSet<_>, _>>()?;
    numbers.extend(pulls.iter().map(|pull| pull.context().number()));
    let workflow_runs = match previous {
        Some(prior) => prior.state().workflow_runs().to_vec(),
        None => {
            fetch_workflows(
                io,
                &root,
                repository,
                &branch_heads
                    .iter()
                    .filter(|head| head.branch() == &default_branch)
                    .cloned()
                    .collect::<Vec<_>>(),
                None,
            )
            .await?
        }
    };
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: pulls,
        branch_heads,
        workflow_runs,
    })
    .map_err(|source| ObservationError::InvalidState {
        repository: repository.clone(),
        pull_request: None,
        source,
    })?;
    Ok((
        RepositoryObservation {
            repository: repository.clone(),
            default_branch,
            default_head,
            observation: RepoWatchObservation::new(reviewers.to_vec(), state),
            merged_at: BTreeMap::new(),
            observed_at: OffsetDateTime::now_utc(),
        },
        numbers.into_iter().collect(),
    ))
}

pub(crate) async fn fetch_poll_workflows(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    baseline: &crate::ingest::IngestBaseline,
) -> Result<RepositoryObservation, ObservationError> {
    let root = format!("/repos/{}", repository.as_str());
    let previous = baseline
        .observation
        .as_ref()
        .ok_or(ObservationError::InvalidResponse)?;
    let (default_branch, default_head, branch_heads) = fetch_repository_heads(io, &root).await?;
    let state = previous.state();
    let retained_branches = branch_heads
        .iter()
        .filter(|branch| {
            branch.branch() == &default_branch
                || state.pull_requests().iter().any(|pull| {
                    pull.context().head_repository() == repository
                        && branch.branch() == pull.context().head_branch()
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let workflow_runs =
        fetch_workflows(io, &root, repository, &retained_branches, Some(previous)).await?;
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: state.pull_requests().to_vec(),
        branch_heads,
        workflow_runs,
    })
    .map_err(|source| ObservationError::InvalidState {
        repository: repository.clone(),
        pull_request: None,
        source,
    })?;
    Ok(RepositoryObservation {
        repository: repository.clone(),
        default_branch,
        default_head,
        observation: RepoWatchObservation::new(reviewers.to_vec(), state),
        merged_at: BTreeMap::new(),
        observed_at: OffsetDateTime::now_utc(),
    })
}

async fn pages(
    io: &impl GitHubObservationRead,
    path: &str,
    member: Option<&str>,
) -> Result<Vec<ResponseValue>, ObservationError> {
    let mut result = Vec::new();
    let mut page = 1_u64;
    loop {
        let (value, next) = read_page(io, &page_path(path, page)).await?;
        if member == Some("workflow_runs")
            && value.admit(value["total_count"].as_u64())? > WORKFLOW_RUN_SEARCH_LIMIT
        {
            return Err(ObservationError::WorkflowRunLimitExceeded {
                limit: WORKFLOW_RUN_SEARCH_LIMIT,
            }
            .at(&value.path));
        }
        result.extend(
            value
                .admit(member.map_or(&*value, |key| &value[key]).as_array())?
                .iter()
                .map(|item| ResponseValue {
                    value: item.clone(),
                    path: value.path.clone(),
                }),
        );
        if !next {
            return Ok(result);
        }
        page = page
            .checked_add(1)
            .ok_or(ObservationError::InvalidResponse)?;
    }
}

fn page_path(path: &str, page: u64) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}per_page={PAGE_SIZE}&page={page}")
}

async fn fetch_pull(
    io: &impl GitHubObservationRead,
    root: &str,
    repository: &RepositorySlug,
    number: PullRequestNumber,
    reviewers: &[RepoWatchAuthorLogin],
    previous: Option<&RepoWatchPullRequestState>,
    merged: Option<&RepoWatchMergedPullRequestBaselineV1>,
) -> Result<(RepoWatchPullRequestState, Option<OffsetDateTime>), ObservationError> {
    let path = format!("{root}/pulls/{}", number.get());
    let (detail, _) = read_page(io, &path).await?;
    let context = detail.admit(pull_context(
        &detail,
        previous
            .map(|state| state.context().head_repository())
            .or_else(|| merged.map(RepoWatchMergedPullRequestBaselineV1::head_repository)),
    ))?;
    if context.number() != number {
        return Err(detail.invalid());
    }
    let merged_at = if detail["merged_at"].is_null() {
        None
    } else {
        Some(detail.admit(github_timestamp(
            detail.admit(detail["merged_at"].as_str())?,
        ))?)
    };
    let lifecycle = match (detail["state"].as_str(), merged_at.is_some()) {
        (Some("open"), false) => RepoWatchPullRequestLifecycle::Open,
        (Some("closed"), false) => RepoWatchPullRequestLifecycle::Closed,
        (Some("closed"), true) => RepoWatchPullRequestLifecycle::Merged,
        _ => return Err(detail.invalid()),
    };
    let mergeable_state = match detail["mergeable"].as_bool() {
        Some(true) => MergeableState::Mergeable,
        Some(false) => MergeableState::Conflicting,
        None => MergeableState::Unknown,
    };
    let ((completed_check_suites, completed_check_runs), reviews, threads, reactions) =
        if lifecycle == RepoWatchPullRequestLifecycle::Open {
            (
                fetch_checks(io, root, context.head_sha()).await?,
                fetch_reviews(io, &path, previous).await?,
                fetch_threads(io, repository, number).await?,
                fetch_reactions(io, root, number, reviewers).await?,
            )
        } else if let Some(previous) = previous {
            (
                (
                    previous.completed_check_suites().to_vec(),
                    previous.completed_check_runs().to_vec(),
                ),
                previous.reviews().to_vec(),
                previous.threads().to_vec(),
                previous.reactions().to_vec(),
            )
        } else {
            ((Vec::new(), Vec::new()), Vec::new(), Vec::new(), Vec::new())
        };
    let state = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context,
        lifecycle,
        mergeable_state,
        completed_check_suites,
        completed_check_runs,
        reviews,
        threads,
        reactions,
    })
    .map_err(|source| ObservationError::InvalidState {
        repository: repository.clone(),
        pull_request: Some(number),
        source,
    })?;
    Ok((state, merged_at))
}

// GitHub REST timestamps use UTC calendar dates with second precision.
pub(crate) fn github_timestamp(value: &str) -> Option<OffsetDateTime> {
    use sqlx::types::time::Date;
    if value.len() != 20 || value.get(10..11)? != "T" || !value.ends_with('Z') {
        return None;
    }
    let (year, rest) = value.get(..10)?.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    let mut clock = value.get(11..19)?.split(':');
    let date = Date::from_calendar_date(
        year.parse().ok()?,
        month.parse::<u8>().ok()?.try_into().ok()?,
        day.parse().ok()?,
    )
    .ok()?;
    Some(
        date.with_hms(
            clock.next()?.parse().ok()?,
            clock.next()?.parse().ok()?,
            clock.next()?.parse().ok()?,
        )
        .ok()?
        .assume_utc(),
    )
}

async fn fetch_checks(
    io: &impl GitHubObservationRead,
    root: &str,
    head: &CommitSha,
) -> Result<
    (
        Vec<RepoWatchCheckSuiteObservation>,
        Vec<RepoWatchCheckRunObservation>,
    ),
    ObservationError,
> {
    let suites = pages(
        io,
        &format!("{root}/commits/{}/check-suites?filter=all", head.as_str()),
        Some("check_suites"),
    )
    .await?;
    if let Some(overflow) = suites.get(COMMIT_CHECK_SUITE_LIMIT) {
        return Err(ObservationError::CheckSuiteLimitExceeded {
            limit: COMMIT_CHECK_SUITE_LIMIT,
        }
        .at(&overflow.path));
    }
    let mut completed_check_suites = Vec::new();
    let mut completed_check_runs = Vec::new();
    for suite in suites {
        let id = suite.admit(object_id(&suite["id"]))?;
        if suite["status"] == "completed" {
            let result = suite.admit(conclusion(&suite["conclusion"]))?;
            completed_check_suites.push(RepoWatchCheckSuiteObservation::new(
                id,
                suite.admit(
                    RepoWatchCheckCompletionGeneration::try_new(suite.text(&suite["updated_at"])?)
                        .ok(),
                )?,
                match result {
                    CheckConclusion::Success
                    | CheckConclusion::Neutral
                    | CheckConclusion::Skipped => ChecksOutcome::Success,
                    CheckConclusion::Failure
                    | CheckConclusion::Cancelled
                    | CheckConclusion::TimedOut
                    | CheckConclusion::ActionRequired
                    | CheckConclusion::Stale
                    | CheckConclusion::StartupFailure => ChecksOutcome::Failure,
                },
            ));
        }
    }
    for run in pages(
        io,
        &format!("{root}/commits/{}/check-runs?filter=all", head.as_str()),
        Some("check_runs"),
    )
    .await?
    {
        if run["status"] == "completed" {
            completed_check_runs.push(run.admit(check_run(&run))?);
        }
    }
    Ok((completed_check_suites, completed_check_runs))
}

async fn fetch_reviews(
    io: &impl GitHubObservationRead,
    path: &str,
    previous: Option<&RepoWatchPullRequestState>,
) -> Result<Vec<RepoWatchReviewObservation>, ObservationError> {
    let mut reviews = Vec::new();
    for review in pages(io, &format!("{path}/reviews"), None).await? {
        let state = match review["state"].as_str() {
            Some("APPROVED") => Some(ReviewState::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewState::ChangesRequested),
            Some("COMMENTED") => Some(ReviewState::Commented),
            Some("DISMISSED") => None,
            Some("PENDING") => continue,
            _ => return Err(review.invalid()),
        };
        let id = review.admit(object_id(&review["id"]))?;
        let reviewer = if review["user"].is_null() {
            let Some(prior) = previous.and_then(|p| p.reviews().iter().find(|r| r.id() == id))
            else {
                continue;
            };
            prior.reviewer().clone()
        } else {
            review
                .admit(RepoWatchAuthorLogin::try_new(review.text(&review["user"]["login"])?).ok())?
        };
        reviews.push(RepoWatchReviewObservation::new(
            id,
            reviewer,
            state,
            review.admit(CommitSha::try_new(review.text(&review["commit_id"])?).ok())?,
        ));
    }
    Ok(reviews)
}

fn pull_context(
    v: &Value,
    previous_head_repository: Option<&RepositorySlug>,
) -> Option<PullRequestEventContext> {
    let head_repository = match v["head"]["repo"].is_null() {
        true => previous_head_repository?.clone(),
        false => RepositorySlug::try_new(text(&v["head"]["repo"]["full_name"])?).ok()?,
    };
    let author = if v["user"].is_null() {
        None
    } else {
        Some(RepoWatchAuthorLogin::try_new(text(&v["user"]["login"])?).ok()?)
    };
    Some(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(positive(&v["number"])?),
        head_sha: CommitSha::try_new(text(&v["head"]["sha"])?).ok()?,
        head_repository,
        base_branch: BranchName::try_new(text(&v["base"]["ref"])?).ok()?,
        head_branch: BranchName::try_new(text(&v["head"]["ref"])?).ok()?,
        title: PullRequestTitle::try_new(text(&v["title"])?).ok()?,
        body: PullRequestBody::try_new(if v["body"].is_null() {
            String::new()
        } else {
            text(&v["body"])?
        })
        .ok()?,
        labels: array(&v["labels"], |v| {
            signalbox_session_ownership::LabelName::try_new(text(&v["name"])?).ok()
        })?,
        draft: v["draft"].as_bool()?,
        author,
    }))
}

fn check_run(v: &Value) -> Option<RepoWatchCheckRunObservation> {
    Some(RepoWatchCheckRunObservation::new(
        object_id(&v["id"])?,
        RepoWatchCheckCompletionGeneration::try_new(text(&v["completed_at"])?).ok()?,
        CheckRunName::try_new(text(&v["name"])?).ok()?,
        conclusion(&v["conclusion"])?,
    ))
}

async fn fetch_threads(
    io: &impl GitHubObservationRead,
    repository: &RepositorySlug,
    number: PullRequestNumber,
) -> Result<Vec<RepoWatchThreadObservation>, ObservationError> {
    let (owner, name) = admit(repository.as_str().split_once('/'))?;
    let mut after = Value::Null;
    let mut threads = Vec::new();
    let mut page = 1_u64;
    loop {
        let path = format!(
            "/graphql repository={} pull_request={} page={page}",
            repository.as_str(),
            number.get()
        );
        let value = io
            .threads(json!({"query": THREADS_QUERY, "variables": {
                "owner": owner, "name": name, "number": number.get(), "after": after,
            }}))
            .await
            .map_err(|error| error.at(&path))?;
        let value = ResponseValue { value, path };
        if value
            .get("errors")
            .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
        {
            return Err(value.invalid());
        }
        let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
        for thread in value.admit(connection["nodes"].as_array())? {
            let id = value.admit(ReviewThreadId::try_new(value.text(&thread["id"])?).ok())?;
            let author = value.admit(
                RepoWatchAuthorLogin::try_new(
                    value.text(&thread["comments"]["nodes"][0]["author"]["login"])?,
                )
                .ok(),
            )?;
            threads.push(if value.admit(thread["isResolved"].as_bool())? {
                RepoWatchThreadObservation::resolved(
                    id,
                    author,
                    value.admit(
                        RepoWatchAuthorLogin::try_new(value.text(&thread["resolvedBy"]["login"])?)
                            .ok(),
                    )?,
                )
            } else {
                RepoWatchThreadObservation::open(id, author)
            });
        }
        if !value.admit(connection["pageInfo"]["hasNextPage"].as_bool())? {
            return Ok(threads);
        }
        after = Value::String(value.text(&connection["pageInfo"]["endCursor"])?);
        page += 1;
    }
}

async fn fetch_reactions(
    io: &impl GitHubObservationRead,
    root: &str,
    number: PullRequestNumber,
    reviewers: &[RepoWatchAuthorLogin],
) -> Result<Vec<RepoWatchReactionObservation>, ObservationError> {
    if reviewers.is_empty() {
        return Ok(Vec::new());
    }
    let mut subjects = vec![(
        format!("{root}/issues/{}/reactions", number.get()),
        ReactionSubject::PullRequestBody,
    )];
    for comment in pages(
        io,
        &format!("{root}/issues/{}/comments", number.get()),
        None,
    )
    .await?
    {
        let id = comment.admit(object_id(&comment["id"]))?;
        subjects.push((
            format!("{root}/issues/comments/{}/reactions", id.get()),
            ReactionSubject::IssueComment { id },
        ));
    }
    for comment in pages(io, &format!("{root}/pulls/{}/comments", number.get()), None).await? {
        let id = comment.admit(object_id(&comment["id"]))?;
        subjects.push((
            format!("{root}/pulls/comments/{}/reactions", id.get()),
            ReactionSubject::ReviewComment { id },
        ));
    }
    let mut reactions = Vec::new();
    for (path, subject) in subjects {
        let values = pages(io, &path, None).await?;
        for value in values {
            let Some(login) = value["user"]["login"].as_str() else {
                continue;
            };
            let Some(reviewer) = reviewers
                .iter()
                .find(|r| r.as_str().eq_ignore_ascii_case(login))
            else {
                continue;
            };
            reactions.push(RepoWatchReactionObservation::new(
                subject,
                reviewer.clone(),
                value.admit(ReactionContent::try_new(value.text(&value["content"])?).ok())?,
            ));
        }
    }
    Ok(reactions)
}

async fn fetch_workflows(
    io: &impl GitHubObservationRead,
    root: &str,
    repository: &RepositorySlug,
    branches: &[RepoWatchBranchHead],
    previous: Option<&RepoWatchObservation>,
) -> Result<Vec<RepoWatchWorkflowRunObservation>, ObservationError> {
    let mut runs = previous
        .into_iter()
        .flat_map(|p| p.state().workflow_runs())
        .filter(|run| {
            branches
                .iter()
                .any(|branch| branch.branch() == run.branch())
        })
        .map(|run| ((run.workflow_id(), run.branch().clone()), run.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    for head in branches
        .iter()
        .map(RepoWatchBranchHead::head)
        .collect::<BTreeSet<_>>()
    {
        let path = format!(
            "{root}/actions/runs?head_sha={}&status=completed",
            head.as_str()
        );
        for run in pages(io, &path, Some("workflow_runs")).await? {
            if run["status"] != "completed"
                || run["head_repository"].is_null()
                || run["head_branch"].is_null()
            {
                continue;
            }
            let head_repository = run.admit(
                RepositorySlug::try_new(run.text(&run["head_repository"]["full_name"])?).ok(),
            )?;
            if &head_repository != repository {
                continue;
            }
            let branch = run.admit(BranchName::try_new(run.text(&run["head_branch"])?).ok())?;
            if !branches
                .iter()
                .any(|retained| retained.branch() == &branch && retained.head() == head)
            {
                continue;
            }
            let candidate = RepoWatchWorkflowRunObservation::new(
                run.admit(object_id(&run["id"]))?,
                run.admit(object_id(&run["workflow_id"]))?,
                RepoWatchWorkflowRunAttempt::new(run.admit(positive(&run["run_attempt"]))?),
                branch.clone(),
                run.admit(WorkflowName::try_new(run.text(&run["name"])?).ok())?,
                run.admit(conclusion(&run["conclusion"]))?,
            );
            let key = (candidate.workflow_id(), branch);
            if runs.get(&key).is_none_or(|prior| {
                (candidate.id(), candidate.attempt().get()) > (prior.id(), prior.attempt().get())
            }) {
                runs.insert(key, candidate);
            }
        }
    }
    Ok(runs.into_values().collect())
}

fn admit<T>(value: Option<T>) -> Result<T, ObservationError> {
    value.ok_or(ObservationError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use signalbox_session_ownership::RepoWatchThreadState;

    struct UnavailableClient;
    impl RepositoryClientLoader for UnavailableClient {
        type Error = &'static str;
        async fn load_client(&self) -> Result<GitHubClient, Self::Error> {
            Err("unavailable")
        }
    }

    #[tokio::test]
    async fn failed_client_load_is_visible_for_periodic_polls_and_webhook_wakes()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::measurements::PollOutcome;
        for producer in [EventProducer::Poll, EventProducer::Webhook] {
            let repository = RepositorySlug::try_new("poll-evidence/project".to_owned())?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused:unused@localhost/unused")?;
            let store = RepoWatchStore::new(pool);
            let mut task = GitHubRepositoryTask {
                repository: repository.clone(),
                signal_reviewers: Vec::new(),
                subject_retention: std::time::Duration::ZERO,
                poll_request_budget: std::num::NonZeroUsize::new(100).expect("fixture budget"),
                clients: UnavailableClient,
                store: store.clone(),
            };
            assert!(matches!(
                task.poll(producer).await,
                Err(RepositoryAttemptError::Client(_))
            ));
            let evidence = store.ingestion_measurements(&repository);
            assert_eq!(
                evidence.last_poll.expect("attempt recorded").outcome,
                PollOutcome::ClientFailed
            );
            assert_eq!(evidence.last_successful_observation, None);
            assert_eq!(evidence.events_recorded, 0);
        }
        Ok(())
    }

    struct LoadedClient(GitHubClient);

    impl RepositoryClientLoader for LoadedClient {
        type Error = std::convert::Infallible;
        async fn load_client(&self) -> Result<GitHubClient, Self::Error> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn unavailable_webhook_queue_records_a_store_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = RepositorySlug::try_new("store-evidence/project".to_owned())?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")?;
        pool.close().await;
        let store = RepoWatchStore::new(pool);
        // Inert fixture credentials: the closed store fails before provider I/O.
        let client = GitHubClient::try_new("repo-watch-test", "unused-test-token")?;
        let mut task = GitHubRepositoryTask {
            repository: repository.clone(),
            signal_reviewers: Vec::new(),
            subject_retention: std::time::Duration::ZERO,
            poll_request_budget: std::num::NonZeroUsize::new(
                crate::poll_cache::DEFAULT_POLL_REQUEST_BUDGET,
            )
            .expect("default budget"),
            clients: LoadedClient(client),
            store: store.clone(),
        };
        assert!(matches!(
            task.poll(EventProducer::Webhook).await,
            Err(RepositoryAttemptError::Store(StoreError::Database(
                sqlx::Error::PoolClosed
            )))
        ));
        let evidence = store.ingestion_measurements(&repository);
        assert_eq!(
            evidence.last_poll.expect("attempt recorded").outcome,
            crate::measurements::PollOutcome::StoreFailed
        );
        assert_eq!(evidence.last_successful_observation, None);
        assert_eq!(evidence.events_recorded, 0);
        Ok(())
    }

    // Distinct provider identities and revisions are arbitrary fixture data.
    const HEAD: &str = "1111111111111111111111111111111111111111";

    struct Fixture {
        pages: BTreeMap<String, (Value, bool)>,
        threads: Value,
    }

    impl GitHubObservationRead for Fixture {
        async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
            self.pages
                .get(path)
                .cloned()
                .ok_or(ObservationError::InvalidResponse)
        }

        async fn threads(&self, _: Value) -> Result<Value, ObservationError> {
            Ok(self.threads.clone())
        }
    }

    fn fixture() -> Fixture {
        let root = "/repos/example/project";
        let pages = [
            (String::from(root), json!({"default_branch": "main"}), false),
            (format!("{root}/branches?per_page=100&page=1"), json!([
                {"name": "main", "commit": {"sha": HEAD}}
            ]), false),
            // A short page can still have a next link; its length is not exhaustion.
            (format!("{root}/pulls?state=open&per_page=100&page=1"), json!([]), true),
            (format!("{root}/pulls?state=open&per_page=100&page=2"), json!([{"number": 1}]), false),
            (format!("{root}/pulls/1"), json!({
                "number": 1, "title": "A pull request", "body": null, "draft": false,
                "user": {"login": "author"}, "labels": [{"name": "ready"}],
                "state": "open", "merged_at": null, "mergeable": true,
                "head": {"sha": HEAD, "ref": "topic", "repo": {"full_name": "example/project"}},
                "base": {"ref": "main"}
            }), false),
            (format!("{root}/commits/{HEAD}/check-suites?filter=all&per_page=100&page=1"), json!({
                "check_suites": [{"id": 2, "status": "completed", "updated_at": "suite-completion", "conclusion": "success"}]
            }), false),
            (format!("{root}/commits/{HEAD}/check-runs?filter=all&per_page=100&page=1"), json!({
                "check_runs": [{"id": 3, "status": "completed", "completed_at": "run-completion", "name": "tests", "conclusion": "success"}]
            }), false),
            (format!("{root}/pulls/1/reviews?per_page=100&page=1"), json!([
                {"id": 4, "state": "APPROVED", "user": {"login": "reviewer"}, "commit_id": HEAD}
            ]), false),
            (format!("{root}/issues/1/comments?per_page=100&page=1"), json!([]), false),
            (format!("{root}/pulls/1/comments?per_page=100&page=1"), json!([]), false),
            (format!("{root}/issues/1/reactions?per_page=100&page=1"), json!([
                {"user": {"login": "reviewer"}, "content": "+1"},
                {"user": {"login": "unconfigured"}, "content": "-1"}
            ]), false),
            (format!("{root}/actions/runs?head_sha={HEAD}&status=completed&per_page=100&page=1"), json!({
                "total_count": 1, "workflow_runs": [{"id": 6, "workflow_id": 5, "name": "CI", "run_attempt": 1, "head_branch": "main",
                    "head_repository": {"full_name": "example/project"},
                    "status": "completed", "conclusion": "success"}]
            }), false),
        ].into_iter().map(|(path, body, next)| (path, (body, next))).collect();
        Fixture {
            pages,
            threads: json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [{
                    "id": "thread-one",
                    "isResolved": true,
                    "resolvedBy": {"login": "resolver"},
                    "comments": {"nodes": [{"author": {"login": "thread-author"}}]}
                }],
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            }}}}}),
        }
    }

    #[test]
    fn database_attempt_display_keeps_diagnostics_private() {
        // The arbitrary diagnostic stands in for unredacted SQLx/provider details.
        let diagnostic = "private database diagnostic";
        let error = RepositoryAttemptError::<std::convert::Infallible>::Store(
            StoreError::Database(sqlx::Error::Protocol(diagnostic.to_owned())),
        );
        assert!(format!("{error:?}").contains(diagnostic));
        assert_eq!(
            error.to_string(),
            "repository-watch store: repository-watch module database operation failed"
        );
    }

    #[tokio::test]
    async fn workflow_search_total_above_githubs_cap_rejects_a_truncated_observation() {
        let mut io = fixture();
        let path = format!(
            "/repos/example/project/actions/runs?head_sha={HEAD}&status=completed&per_page=100&page=1"
        );
        io.pages.get_mut(&path).expect("workflow page").0["total_count"] =
            json!(WORKFLOW_RUN_SEARCH_LIMIT + 1);
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let error = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect_err("capped search is incomplete");
        assert_eq!(
            error.to_string(),
            format!("{path}: workflow run search exceeds GitHub's 1000-result limit")
        );
    }

    #[tokio::test]
    async fn workflow_search_exactly_at_githubs_cap_keeps_the_latest_completion() {
        let mut io = fixture();
        let root = "/repos/example/project";
        let path = format!("{root}/actions/runs?head_sha={HEAD}&status=completed");
        let template =
            io.pages.get(&page_path(&path, 1)).expect("workflow page").0["workflow_runs"][0]
                .clone();
        let runs = (1..=WORKFLOW_RUN_SEARCH_LIMIT)
            .map(|id| {
                let mut run = template.clone();
                run["id"] = json!(id);
                run
            })
            .collect::<Vec<_>>();
        let chunks = runs.chunks(usize::from(PAGE_SIZE));
        let page_count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            io.pages.insert(
                page_path(&path, (index + 1) as u64),
                (
                    json!({"total_count": WORKFLOW_RUN_SEARCH_LIMIT, "workflow_runs": chunk}),
                    index + 1 < page_count,
                ),
            );
        }
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let observed = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("complete search at the cap");
        let workflows = observed.observation.state().workflow_runs();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].id().get(), WORKFLOW_RUN_SEARCH_LIMIT);
    }

    #[tokio::test]
    async fn duplicate_check_runs_preserve_the_aggregate_failure_cause() {
        let mut io = fixture();
        let path = "/repos/example/project/commits/1111111111111111111111111111111111111111/check-runs?filter=all&per_page=100&page=1";
        let page = io.pages.get_mut(path).expect("check-run page");
        page.1 = true;
        let repeated = page.0.clone();
        io.pages
            .insert(path.replace("&page=1", "&page=2"), (repeated, false));
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let error = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect_err("repeated check run is rejected");
        let ObservationError::InvalidState {
            repository: actual_repository,
            pull_request: Some(number),
            source: RepoWatchRepositoryStateError::DuplicateCheckRun(id),
        } = &error
        else {
            panic!("aggregate failure keeps its typed cause: {error:?}");
        };
        assert_eq!(actual_repository, &repository);
        assert_eq!(number.get(), 1);
        assert_eq!(id.get(), 3);
        assert!(error.to_string().contains("duplicate check run 3"));
    }

    #[tokio::test]
    async fn githubs_commit_check_run_suite_limit_rejects_an_incomplete_inventory() {
        let mut io = fixture();
        let root = "/repos/example/project";
        let suite_path = format!("{root}/commits/{HEAD}/check-suites?filter=all");
        let suites = (1..=COMMIT_CHECK_SUITE_LIMIT + 1)
            .map(|id| json!({"id": id, "status": "queued"}))
            .collect::<Vec<_>>();
        let chunks = suites.chunks(usize::from(PAGE_SIZE));
        let page_count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            io.pages.insert(
                page_path(&suite_path, (index + 1) as u64),
                (json!({"check_suites": chunk}), index + 1 < page_count),
            );
        }
        let head = CommitSha::try_new(HEAD.to_owned()).expect("head");
        let error = fetch_checks(&io, root, &head)
            .await
            .expect_err("GitHub would truncate the inventory");
        assert!(
            error
                .to_string()
                .contains("commit check-run inventory exceeds GitHub's 1000-suite limit")
        );
        assert!(
            error
                .to_string()
                .contains(&page_path(&suite_path, page_count as u64))
        );
    }

    #[tokio::test]
    async fn workflow_reads_only_retained_heads_and_keeps_latest_attempt() {
        let mut io = fixture();
        let branch_path = "/repos/example/project/branches?per_page=100&page=1";
        // The unrelated branch has no workflow history and must never be searched.
        io.pages.get_mut(branch_path).expect("branch page").0.as_array_mut().expect("branches").push(
            json!({"name": "unrelated", "commit": {"sha": "2222222222222222222222222222222222222222"}})
        );
        let path = format!(
            "/repos/example/project/actions/runs?head_sha={HEAD}&status=completed&per_page=100&page=1"
        );
        let first = io.pages.get_mut(&path).expect("workflow page");
        let mut older = first.0["workflow_runs"][0].clone();
        first.0["workflow_runs"][0]["run_attempt"] = json!(2);
        older["run_attempt"] = json!(1);
        first.1 = true;
        first.0["total_count"] = json!(2);
        io.pages.insert(
            path.replace("&page=1", "&page=2"),
            (json!({"total_count": 2, "workflow_runs": [older]}), false),
        );
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let observed = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("bounded observation");
        assert_eq!(observed.observation.state().workflow_runs().len(), 1);
        assert_eq!(
            observed.observation.state().workflow_runs()[0]
                .attempt()
                .get(),
            2
        );
    }

    #[tokio::test]
    async fn an_endless_provider_exhausts_the_observation_budget_without_an_extra_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Endless {
            calls: AtomicUsize,
        }
        impl GitHubObservationRead for Endless {
            async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(if path == "/repos/example/project" {
                    (json!({"default_branch": "main"}), false)
                } else {
                    (json!([]), true)
                })
            }
            async fn threads(&self, _: Value) -> Result<Value, ObservationError> {
                panic!("branch pagination never reaches GraphQL")
            }
        }
        let io = Endless {
            calls: AtomicUsize::new(0),
        };
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let error = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect_err("budget rejects incomplete observation");
        assert_eq!(io.calls.load(Ordering::Relaxed), MAX_OBSERVATION_REQUESTS);
        assert!(
            error
                .to_string()
                .contains("observation request budget exhausted")
        );
        assert!(error.to_string().contains(&format!(
            "/branches?per_page=100&page={MAX_OBSERVATION_REQUESTS}"
        )));
    }

    #[tokio::test]
    async fn rest_and_graphql_share_the_same_observation_request_budget() {
        let io = fixture();
        let budget = ObservationReadBudget {
            io: &io,
            requests: std::sync::atomic::AtomicUsize::new(MAX_OBSERVATION_REQUESTS - 1),
        };
        budget
            .threads(json!({}))
            .await
            .expect("last admitted request");
        let error = budget
            .page("/repos/example/project")
            .await
            .expect_err("REST cannot exceed the shared budget");
        assert!(
            error
                .to_string()
                .contains("observation request budget exhausted")
        );
    }
    #[tokio::test]
    async fn invalid_branch_reports_its_original_page_after_pagination() {
        let mut io = fixture();
        let path = "/repos/example/project/branches?per_page=100&page=1";
        io.pages.get_mut(path).expect("branch page").0[0]["commit"]["sha"] = json!("invalid");
        io.pages.get_mut(path).expect("branch page").1 = true;
        io.pages.insert(
            String::from("/repos/example/project/branches?per_page=100&page=2"),
            (json!([]), false),
        );
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let error = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect_err("invalid branch");
        assert_eq!(
            error.to_string(),
            format!("{path}: repository-watch provider observation is invalid")
        );
    }

    #[tokio::test]
    async fn complete_observation_follows_pagination_and_keeps_only_configured_reactions() {
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let reviewer = RepoWatchAuthorLogin::try_new(String::from("reviewer")).expect("reviewer");
        let observed = fetch_observation(
            &fixture(),
            &repository,
            std::slice::from_ref(&reviewer),
            None,
            &[],
        )
        .await
        .expect("complete observation");
        assert_eq!(observed.default_head.as_str(), HEAD);
        assert_eq!(observed.observation.state().pull_requests().len(), 1);
        let pull = &observed.observation.state().pull_requests()[0];
        assert_eq!(
            pull.completed_check_runs()[0].conclusion(),
            CheckConclusion::Success
        );
        assert_eq!(pull.reviews()[0].state(), Some(ReviewState::Approved));
        assert_eq!(pull.threads()[0].state(), RepoWatchThreadState::Resolved);
        assert_eq!(pull.threads()[0].author().as_str(), "thread-author");
        assert_eq!(
            pull.threads()[0]
                .resolver()
                .expect("resolved thread retains its resolver")
                .as_str(),
            "resolver"
        );
        assert_eq!(pull.reactions().len(), 1);
        assert_eq!(pull.reactions()[0].reactor(), &reviewer);
        assert_eq!(
            observed.observation.state().workflow_runs()[0]
                .workflow()
                .as_str(),
            "CI"
        );
    }

    #[tokio::test]
    async fn graphql_errors_reject_a_partial_observation() {
        let mut io = fixture();
        io.threads["errors"] = json!([{"message": "partial provider response"}]);
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let result = fetch_observation(&io, &repository, &[], None, &[]).await;
        let error = result.expect_err("partial GraphQL observation rejected");
        assert!(
            error
                .to_string()
                .contains("/graphql repository=example/project pull_request=1 page=1")
        );
    }
    #[tokio::test]
    async fn workflow_repository_comparison_uses_canonical_slugs() {
        let mut io = fixture();
        io.pages
            .get_mut("/repos/example/project/actions/runs?head_sha=1111111111111111111111111111111111111111&status=completed&per_page=100&page=1")
            .expect("workflow page")
            .0["workflow_runs"][0]["head_repository"]["full_name"] = json!("Example/Project");
        let repository =
            RepositorySlug::try_new(String::from("EXAMPLE/Project")).expect("repository");
        let observed = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("observation");
        assert_eq!(observed.observation.state().workflow_runs().len(), 1);
    }

    #[tokio::test]
    async fn terminal_pulls_preserve_comparison_without_reading_discussions_or_dead_heads() {
        let mut io = fixture();
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let reviewer = RepoWatchAuthorLogin::try_new(String::from("reviewer")).expect("reviewer");
        let previous =
            fetch_observation(&io, &repository, std::slice::from_ref(&reviewer), None, &[])
                .await
                .expect("open observation");
        io.pages
            .get_mut("/repos/example/project/pulls?state=open&per_page=100&page=1")
            .expect("open page")
            .0 = json!([]);
        io.pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("detail")
            .0["state"] = json!("closed");
        // Keep the default branch alive at a different SHA; the closed head must not be searched.
        let default_head = "2222222222222222222222222222222222222222";
        io.pages
            .get_mut("/repos/example/project/branches?per_page=100&page=1")
            .expect("branches")
            .0 = json!([
            {"name": "main", "commit": {"sha": default_head}},
            {"name": "feature", "commit": {"sha": HEAD}}
        ]);
        io.pages.retain(|path, _| {
            !path.contains("/commits/")
                && !path.contains("/reviews")
                && !path.contains("/reactions")
                && !path.contains("/comments")
                && !path.contains("/actions/runs")
        });
        io.threads = Value::Null;
        io.pages.insert(format!("/repos/example/project/actions/runs?head_sha={default_head}&status=completed&per_page=100&page=1"), (json!({"total_count": 0, "workflow_runs": []}), false));
        let observed = fetch_observation(
            &io,
            &repository,
            std::slice::from_ref(&reviewer),
            Some(&previous.observation),
            &[],
        )
        .await
        .expect("terminal observation skips dead surfaces");
        let terminal = &observed.observation.state().pull_requests()[0];
        assert_eq!(terminal.lifecycle(), RepoWatchPullRequestLifecycle::Closed);
        assert_eq!(
            terminal.reactions(),
            previous.observation.state().pull_requests()[0].reactions()
        );
        assert_eq!(
            terminal.reviews(),
            previous.observation.state().pull_requests()[0].reviews()
        );
    }

    #[tokio::test]
    async fn invalid_merge_time_rejects_the_observation() {
        let mut io = fixture();
        let detail = &mut io
            .pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("detail")
            .0;
        detail["state"] = json!("closed");
        detail["merged_at"] = json!("2026-02-30T00:00:00Z");
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let error = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect_err("invalid calendar date");
        assert!(error.to_string().contains("/repos/example/project/pulls/1"));
    }

    #[tokio::test]
    async fn compacted_merged_numbers_do_not_cause_provider_reads() {
        use signalbox_session_ownership::RepoWatchMergedPullRequestBaselineInputV1;
        let mut io = fixture();
        *io.pages
            .get_mut("/repos/example/project/pulls?state=open&per_page=100&page=1")
            .expect("pull page") = (json!([]), false);
        let detail = &mut io
            .pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("pull detail")
            .0;
        detail["state"] = json!("closed");
        detail["merged_at"] = json!("2026-09-06T00:00:00Z");
        let compacted = RepoWatchMergedPullRequestBaselineV1::try_new(
            RepoWatchMergedPullRequestBaselineInputV1 {
                head_repository: RepositorySlug::try_new(String::from("example/project"))
                    .expect("head repository"),
                number: PullRequestNumber::new(
                    std::num::NonZeroU64::new(1).expect("positive fixture number"),
                ),
                head_sha: CommitSha::try_new(HEAD.to_owned()).expect("head"),
                signal_reviewers: Vec::new(),
                labels: Vec::new(),
                mergeable_state: MergeableState::Unknown,
                completed_check_suites: Vec::new(),
                completed_check_runs: Vec::new(),
                review_ids: Vec::new(),
                threads: Vec::new(),
                reactions: Vec::new(),
            },
        )
        .expect("compacted baseline");
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        io.pages.remove("/repos/example/project/pulls/1");
        let observed = fetch_observation(&io, &repository, &[], None, &[compacted])
            .await
            .expect("compact subjects need no provider reads");
        assert!(observed.observation.state().pull_requests().is_empty());
    }

    #[tokio::test]
    async fn historical_workflow_without_a_head_repository_does_not_block_watched_runs() {
        let mut io = fixture();
        io.pages
            .get_mut("/repos/example/project/actions/runs?head_sha=1111111111111111111111111111111111111111&status=completed&per_page=100&page=1")
            .expect("workflow page")
            .0["workflow_runs"]
            .as_array_mut()
            .expect("runs")
            .insert(
                0,
                json!({"status": "completed", "head_repository": null, "head_branch": "main"}),
            );
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let observed = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("complete observation");
        assert_eq!(observed.observation.state().workflow_runs().len(), 1);
        assert_eq!(
            observed.observation.state().workflow_runs()[0].id().get(),
            6
        );
    }

    #[tokio::test]
    async fn historical_workflow_without_a_head_branch_does_not_block_watched_runs() {
        let mut io = fixture();
        io.pages
            .get_mut("/repos/example/project/actions/runs?head_sha=1111111111111111111111111111111111111111&status=completed&per_page=100&page=1")
            .expect("workflow page")
            .0["workflow_runs"]
            .as_array_mut()
            .expect("runs")
            .insert(
                0,
                json!({"status": "completed", "head_repository": {"full_name": "example/project"}, "head_branch": null}),
            );
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let observed = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("complete observation");
        assert_eq!(observed.observation.state().workflow_runs().len(), 1);
        assert_eq!(
            observed.observation.state().workflow_runs()[0].id().get(),
            6
        );
    }

    #[tokio::test]
    async fn an_anonymous_reaction_does_not_hide_another_reviewers_removal() {
        use signalbox_session_ownership::{
            ReactionChange, RepoWatchEventIdentityFrontierV1, RepoWatchEventKindV1,
            UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
        };
        let mut io = fixture();
        let reaction_path = "/repos/example/project/issues/1/reactions?per_page=100&page=1";
        io.pages.get_mut(reaction_path).expect("reaction page").0 = json!([
            {"user": {"login": "reviewer"}, "content": "+1"},
            {"user": {"login": "other-reviewer"}, "content": "-1"}
        ]);
        let reviewers = [
            RepoWatchAuthorLogin::try_new(String::from("reviewer")).expect("reviewer"),
            RepoWatchAuthorLogin::try_new(String::from("other-reviewer")).expect("other reviewer"),
        ];
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let prior = fetch_observation(&io, &repository, &reviewers, None, &[])
            .await
            .expect("prior observation");
        io.pages.get_mut(reaction_path).expect("reaction page").0 =
            json!([{"user": null, "content": "+1"}]);
        let observed =
            fetch_observation(&io, &repository, &reviewers, Some(&prior.observation), &[])
                .await
                .expect("current observation");
        assert!(
            observed.observation.state().pull_requests()[0]
                .reactions()
                .is_empty()
        );
        let events = derive_repo_watch_events(
            &repository,
            Some(&prior.observation),
            &observed.observation,
            &mut RepoWatchEventIdentityFrontierV1::default(),
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .expect("diff");
        let removal = RepoWatchEventKindV1::ReactionChanged {
            subject: ReactionSubject::PullRequestBody,
            reactor: reviewers[1].clone(),
            content: ReactionContent::try_new(String::from("-1")).expect("removed content"),
            change: ReactionChange::Removed,
        };
        assert!(
            events
                .iter()
                .any(|occurrence| occurrence.event().kind() == &removal)
        );
    }

    #[tokio::test]
    async fn a_compacted_merged_pull_request_retains_its_deleted_fork_identity() {
        let mut io = fixture();
        let fork = RepositorySlug::try_new(String::from("example/fork")).expect("fork");
        let detail = &mut io
            .pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("pull detail")
            .0;
        detail["state"] = json!("closed");
        detail["merged_at"] = json!("2026-09-06T00:00:00Z");
        detail["head"]["repo"]["full_name"] = json!(fork.as_str());
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let prior = fetch_observation(&io, &repository, &[], None, &[])
            .await
            .expect("merged observation");
        let compact = RepoWatchMergedPullRequestBaselineV1::from_merged_state(
            &prior.observation.state().pull_requests()[0],
            &[],
        )
        .expect("compact state")
        .expect("merged baseline");
        let empty = RepoWatchObservation::new(Vec::new(), RepoWatchRepositoryState::default());
        let stored = crate::baseline::observation_payload(
            &empty,
            &[crate::ingest::MergedPullRequestBaseline {
                state: compact,
                merged_at: prior.merged_at[&prior.observation.state().pull_requests()[0]
                    .context()
                    .number()],
            }],
        );
        let restored = crate::observation_decode::merged_baselines(&stored)
            .expect("restored compact baseline");
        assert_eq!(restored[0].state.head_repository(), &fork);
        *io.pages
            .get_mut("/repos/example/project/pulls?state=open&per_page=100&page=1")
            .expect("pull page") = (json!([]), false);
        io.pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("pull detail")
            .0["head"]["repo"] = Value::Null;
        io.pages.remove("/repos/example/project/pulls/1");
        let observed = fetch_observation(&io, &repository, &[], None, &[restored[0].state.clone()])
            .await
            .expect("deleted fork requires no provider read");
        assert!(observed.observation.state().pull_requests().is_empty());
    }
}
