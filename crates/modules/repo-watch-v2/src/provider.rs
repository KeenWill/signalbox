//! GitHub observation composition for the repository task.

use std::{collections::BTreeSet, error::Error, fmt, future::Future};

use serde_json::{Value, json};
use signalbox_ownership_seam::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, MergeableState,
    OffsetDateTime, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionContent, ReactionSubject, RepoWatchAuthorLogin,
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchMergedPullRequestBaselineV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchReactionObservation, RepoWatchRepositoryState, RepoWatchRepositoryStateInput,
    RepoWatchReviewObservation, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWorkflowRunAttempt, RepoWatchWorkflowRunObservation, RepositorySlug, ReviewState,
    ReviewThreadId, WorkflowName,
};

use crate::{
    EventProducer, FrontierEventAdmission, RepoWatchStore, StoreError,
    github::GitHubClient,
    ingest::{RepositoryObservation, RepositoryTask},
    observation_decode::{array, conclusion, object_id, positive, text},
};

// GitHub's maximum REST page size, used only to select complete provider pages.
const PAGE_SIZE: u16 = 100;
const THREADS_QUERY: &str = r#"
query RepositoryWatchReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        nodes { id isResolved }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

/// External failures contain no request, credential, or provider-body details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationError {
    Transport,
    InvalidResponse,
}

impl fmt::Display for ObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transport => "repository-watch provider transport failed",
            Self::InvalidResponse => "repository-watch provider observation is invalid",
        })
    }
}
impl Error for ObservationError {}

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
            .map_err(|_| ObservationError::Transport)?;
        Ok((
            serde_json::from_slice(&body).map_err(|_| ObservationError::InvalidResponse)?,
            next,
        ))
    }

    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        let body = self
            .graphql(request.to_string().into_bytes())
            .await
            .map_err(|_| ObservationError::Transport)?;
        serde_json::from_slice(&body).map_err(|_| ObservationError::InvalidResponse)
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

impl<Loader: RepositoryClientLoader> RepositoryTask for GitHubRepositoryTask<Loader> {
    type Error = RepositoryAttemptError<Loader::Error>;

    async fn poll(&mut self, producer: EventProducer) -> Result<(), Self::Error> {
        let baseline = self
            .store
            .ingest_baseline(&self.repository)
            .await
            .map_err(RepositoryAttemptError::Store)?;
        let client = self
            .clients
            .load_client()
            .await
            .map_err(RepositoryAttemptError::Client)?;
        let observed = fetch_observation(
            &client,
            &self.repository,
            &self.signal_reviewers,
            baseline.observation.as_ref(),
            &baseline.merged_baselines,
        )
        .await
        .map_err(RepositoryAttemptError::Observation)?;
        match self
            .store
            .ingest_observation(&baseline, &observed, producer)
            .await
            .map_err(RepositoryAttemptError::Store)?
        {
            FrontierEventAdmission::Committed { .. } | FrontierEventAdmission::Unchanged => Ok(()),
            FrontierEventAdmission::Stale | FrontierEventAdmission::ConflictingReuse => {
                Err(RepositoryAttemptError::FrontierConflict)
            }
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
    let root = format!("/repos/{}", repository.as_str());
    let (metadata, _) = io.page(&root).await?;
    let default_branch =
        admit(BranchName::try_new(required_text(&metadata["default_branch"])?).ok())?;
    let branch_heads = admit(array(
        &Value::Array(pages(io, &format!("{root}/branches"), None).await?),
        |v| {
            Some(RepoWatchBranchHead::new(
                BranchName::try_new(text(&v["name"])?).ok()?,
                CommitSha::try_new(text(&v["commit"]["sha"])?).ok()?,
            ))
        },
    ))?;
    let default_head = admit(
        branch_heads
            .iter()
            .find(|branch| branch.branch() == &default_branch),
    )?
    .head()
    .clone();
    let mut numbers = pages(io, &format!("{root}/pulls?state=open"), None)
        .await?
        .iter()
        .map(|v| admit(positive(&v["number"]).map(PullRequestNumber::new)))
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
    numbers.extend(
        merged_baselines
            .iter()
            .map(RepoWatchMergedPullRequestBaselineV1::number),
    );
    let mut pulls = Vec::new();
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
        pulls.push(fetch_pull(io, &root, repository, number, reviewers, prior, merged).await?);
    }
    let workflow_runs = fetch_workflows(io, &root, repository, &branch_heads, previous).await?;
    let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests: pulls,
        branch_heads,
        workflow_runs,
    })
    .map_err(|_| ObservationError::InvalidResponse)?;
    Ok(RepositoryObservation {
        repository: repository.clone(),
        default_branch,
        default_head,
        observation: RepoWatchObservation::new(reviewers.to_vec(), state),
        observed_at: OffsetDateTime::now_utc(),
    })
}

async fn pages(
    io: &impl GitHubObservationRead,
    path: &str,
    member: Option<&str>,
) -> Result<Vec<Value>, ObservationError> {
    let mut result = Vec::new();
    let mut page = 1_u64;
    loop {
        let (value, next) = io.page(&page_path(path, page)).await?;
        result.extend(
            admit(member.map_or(&value, |key| &value[key]).as_array())?
                .iter()
                .cloned(),
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
) -> Result<RepoWatchPullRequestState, ObservationError> {
    let path = format!("{root}/pulls/{}", number.get());
    let (detail, _) = io.page(&path).await?;
    let context = admit(pull_context(
        &detail,
        previous
            .map(|state| state.context().head_repository())
            .or_else(|| merged.map(RepoWatchMergedPullRequestBaselineV1::head_repository)),
    ))?;
    if context.number() != number {
        return Err(ObservationError::InvalidResponse);
    }
    let lifecycle = match (detail["state"].as_str(), detail["merged_at"].is_string()) {
        (Some("open"), false) => RepoWatchPullRequestLifecycle::Open,
        (Some("closed"), false) => RepoWatchPullRequestLifecycle::Closed,
        (Some("closed"), true) => RepoWatchPullRequestLifecycle::Merged,
        _ => return Err(ObservationError::InvalidResponse),
    };
    let mergeable_state = match detail["mergeable"].as_bool() {
        Some(true) => MergeableState::Mergeable,
        Some(false) => MergeableState::Conflicting,
        None => MergeableState::Unknown,
    };
    let suites = pages(
        io,
        &format!(
            "{root}/commits/{}/check-suites?filter=all",
            context.head_sha().as_str()
        ),
        Some("check_suites"),
    )
    .await?;
    let mut completed_check_suites = Vec::new();
    let mut completed_check_runs = Vec::new();
    for suite in suites {
        let id = admit(object_id(&suite["id"]))?;
        if suite["status"] == "completed" {
            let result = admit(conclusion(&suite["conclusion"]))?;
            completed_check_suites.push(RepoWatchCheckSuiteObservation::new(
                id,
                admit(
                    RepoWatchCheckCompletionGeneration::try_new(required_text(
                        &suite["updated_at"],
                    )?)
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
        for run in pages(
            io,
            &format!("{root}/check-suites/{}/check-runs?filter=all", id.get()),
            Some("check_runs"),
        )
        .await?
        {
            if run["status"] != "completed" {
                continue;
            }
            completed_check_runs.push(admit(check_run(&run))?);
        }
    }
    let mut reviews = Vec::new();
    for review in pages(io, &format!("{path}/reviews"), None).await? {
        let state = match review["state"].as_str() {
            Some("APPROVED") => Some(ReviewState::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewState::ChangesRequested),
            Some("COMMENTED") => Some(ReviewState::Commented),
            Some("DISMISSED") => None,
            Some("PENDING") => continue,
            _ => return Err(ObservationError::InvalidResponse),
        };
        let id = admit(object_id(&review["id"]))?;
        let reviewer = if review["user"].is_null() {
            let Some(prior) = previous.and_then(|p| p.reviews().iter().find(|r| r.id() == id))
            else {
                continue;
            };
            prior.reviewer().clone()
        } else {
            admit(RepoWatchAuthorLogin::try_new(required_text(&review["user"]["login"])?).ok())?
        };
        reviews.push(RepoWatchReviewObservation::new(
            id,
            reviewer,
            state,
            admit(CommitSha::try_new(required_text(&review["commit_id"])?).ok())?,
        ));
    }
    let threads = fetch_threads(io, repository, number).await?;
    let reactions = fetch_reactions(io, root, number, reviewers).await?;
    RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context,
        lifecycle,
        mergeable_state,
        completed_check_suites,
        completed_check_runs,
        reviews,
        threads,
        reactions,
    })
    .map_err(|_| ObservationError::InvalidResponse)
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
            signalbox_ownership_seam::LabelName::try_new(text(&v["name"])?).ok()
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
    loop {
        let value = io
            .threads(json!({"query": THREADS_QUERY, "variables": {
                "owner": owner, "name": name, "number": number.get(), "after": after,
            }}))
            .await?;
        if value
            .get("errors")
            .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
        {
            return Err(ObservationError::InvalidResponse);
        }
        let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
        for thread in admit(connection["nodes"].as_array())? {
            threads.push(RepoWatchThreadObservation::new(
                admit(ReviewThreadId::try_new(required_text(&thread["id"])?).ok())?,
                if admit(thread["isResolved"].as_bool())? {
                    RepoWatchThreadState::Resolved
                } else {
                    RepoWatchThreadState::Open
                },
            ));
        }
        if !admit(connection["pageInfo"]["hasNextPage"].as_bool())? {
            return Ok(threads);
        }
        after = Value::String(required_text(&connection["pageInfo"]["endCursor"])?);
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
        let id = admit(object_id(&comment["id"]))?;
        subjects.push((
            format!("{root}/issues/comments/{}/reactions", id.get()),
            ReactionSubject::IssueComment { id },
        ));
    }
    for comment in pages(io, &format!("{root}/pulls/{}/comments", number.get()), None).await? {
        let id = admit(object_id(&comment["id"]))?;
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
                admit(ReactionContent::try_new(required_text(&value["content"])?).ok())?,
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
    let mut runs = Vec::new();
    for workflow in pages(io, &format!("{root}/actions/workflows"), Some("workflows")).await? {
        let id = admit(object_id(&workflow["id"]))?;
        let name = admit(WorkflowName::try_new(required_text(&workflow["name"])?).ok())?;
        let mut pending = branches.iter().map(|b| b.branch()).collect::<BTreeSet<_>>();
        let mut page = 1_u64;
        while !pending.is_empty() {
            let (value, next) = io
                .page(&page_path(
                    &format!("{root}/actions/workflows/{}/runs", id.get()),
                    page,
                ))
                .await?;
            for run in admit(value["workflow_runs"].as_array())? {
                if run["status"] != "completed" || run["head_repository"].is_null() {
                    continue;
                }
                let head_repository = admit(
                    RepositorySlug::try_new(required_text(&run["head_repository"]["full_name"])?)
                        .ok(),
                )?;
                if &head_repository != repository {
                    continue;
                }
                let branch = admit(BranchName::try_new(required_text(&run["head_branch"])?).ok())?;
                if !pending.remove(&branch) {
                    continue;
                }
                let candidate = RepoWatchWorkflowRunObservation::new(
                    admit(object_id(&run["id"]))?,
                    id,
                    RepoWatchWorkflowRunAttempt::new(admit(positive(&run["run_attempt"]))?),
                    branch.clone(),
                    name.clone(),
                    admit(conclusion(&run["conclusion"]))?,
                );
                let prior = previous.and_then(|p| {
                    p.state()
                        .workflow_runs()
                        .iter()
                        .find(|r| r.workflow_id() == id && r.branch() == &branch)
                });
                runs.push(
                    prior
                        .filter(|p| {
                            (p.id(), p.attempt().get())
                                > (candidate.id(), candidate.attempt().get())
                        })
                        .cloned()
                        .unwrap_or(candidate),
                );
            }
            if !next {
                break;
            }
            page = page
                .checked_add(1)
                .ok_or(ObservationError::InvalidResponse)?;
        }
        runs.extend(
            previous
                .into_iter()
                .flat_map(|p| p.state().workflow_runs())
                .filter(|r| r.workflow_id() == id && pending.contains(r.branch()))
                .cloned(),
        );
    }
    Ok(runs)
}

fn admit<T>(value: Option<T>) -> Result<T, ObservationError> {
    value.ok_or(ObservationError::InvalidResponse)
}
fn required_text(value: &Value) -> Result<String, ObservationError> {
    admit(text(value))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

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
            (format!("{root}/check-suites/2/check-runs?filter=all&per_page=100&page=1"), json!({
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
            (format!("{root}/actions/workflows?per_page=100&page=1"), json!({
                "workflows": [{"id": 5, "name": "CI"}]
            }), false),
            (format!("{root}/actions/workflows/5/runs?per_page=100&page=1"), json!({
                "workflow_runs": [{"id": 6, "run_attempt": 1, "head_branch": "main",
                    "head_repository": {"full_name": "example/project"},
                    "status": "completed", "conclusion": "success"}]
            }), false),
        ].into_iter().map(|(path, body, next)| (path, (body, next))).collect();
        Fixture {
            pages,
            threads: json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [{"id": "thread-one", "isResolved": true}],
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            }}}}}),
        }
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
        assert_eq!(result, Err(ObservationError::InvalidResponse));
    }
    #[tokio::test]
    async fn workflow_repository_comparison_uses_canonical_slugs() {
        let mut io = fixture();
        io.pages
            .get_mut("/repos/example/project/actions/workflows/5/runs?per_page=100&page=1")
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
    async fn compacted_merged_numbers_are_refetched_without_an_open_pull_request() {
        use signalbox_ownership_seam::RepoWatchMergedPullRequestBaselineInputV1;
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
        let observed = fetch_observation(&io, &repository, &[], None, &[compacted])
            .await
            .expect("refetch merged subject");
        assert_eq!(observed.observation.state().pull_requests().len(), 1);
        assert_eq!(
            observed.observation.state().pull_requests()[0].lifecycle(),
            RepoWatchPullRequestLifecycle::Merged
        );
    }

    #[tokio::test]
    async fn historical_workflow_without_a_head_repository_does_not_block_watched_runs() {
        let mut io = fixture();
        io.pages
            .get_mut("/repos/example/project/actions/workflows/5/runs?per_page=100&page=1")
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
    async fn an_anonymous_reaction_does_not_hide_another_reviewers_removal() {
        use signalbox_ownership_seam::{
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
        let stored = crate::baseline::observation_payload(&empty, &[compact]);
        let restored = crate::observation_decode::merged_baselines(&stored)
            .expect("restored compact baseline");
        assert_eq!(restored[0].head_repository(), &fork);
        *io.pages
            .get_mut("/repos/example/project/pulls?state=open&per_page=100&page=1")
            .expect("pull page") = (json!([]), false);
        io.pages
            .get_mut("/repos/example/project/pulls/1")
            .expect("pull detail")
            .0["head"]["repo"] = Value::Null;
        let observed = fetch_observation(&io, &repository, &[], None, &restored)
            .await
            .expect("deleted fork observation");
        assert_eq!(
            observed.observation.state().pull_requests()[0]
                .context()
                .head_repository(),
            &fork
        );
        assert_eq!(
            observed.observation.state().pull_requests()[0].lifecycle(),
            RepoWatchPullRequestLifecycle::Merged
        );
    }
}
