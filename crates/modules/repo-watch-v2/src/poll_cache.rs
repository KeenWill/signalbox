//! Accepted REST transport snapshots, isolated from events and rule evaluation.

use std::{collections::BTreeSet, future::Future, num::NonZeroU64};

use serde_json::{Value, json};
use signalbox_session_ownership::{
    CommitSha, RepoWatchAuthorLogin, RepoWatchPullRequestLifecycle, RepositorySlug,
};
use sqlx::Row;
use tokio::sync::Mutex;

use crate::{
    EventProducer, FrontierEventAdmission, RepoWatchStore, StoreError,
    github::{ConditionalPage, GitHubClient, HttpValidators},
    provider::{GitHubObservationRead, ObservationError, PAGE_SIZE, fetch_observation},
};

/// Conditional transport for complete repository observations.
pub trait ConditionalObservationRead: Send + Sync {
    fn conditional_page(
        &self,
        path: &str,
        validators: Option<&HttpValidators>,
    ) -> impl Future<Output = Result<ConditionalPage, ObservationError>> + Send;
    fn threads(
        &self,
        request: Value,
    ) -> impl Future<Output = Result<Value, ObservationError>> + Send;
}

impl ConditionalObservationRead for GitHubClient {
    async fn conditional_page(
        &self,
        path: &str,
        validators: Option<&HttpValidators>,
    ) -> Result<ConditionalPage, ObservationError> {
        self.conditional_page(path, validators)
            .await
            .map_err(ObservationError::Transport)
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        GitHubObservationRead::threads(self, request).await
    }
}

#[derive(Clone, Debug)]
enum Resource {
    Metadata,
    Branches(NonZeroU64),
    Pulls(NonZeroU64),
    Pull(NonZeroU64),
    Suites(CommitSha, NonZeroU64),
    Runs(CommitSha, NonZeroU64),
    Reviews(NonZeroU64, NonZeroU64),
    Comments(bool, NonZeroU64, NonZeroU64),
    Reactions(ReactionResource, NonZeroU64, NonZeroU64),
    Workflows(CommitSha, NonZeroU64),
}

#[derive(Clone, Debug)]
enum ReactionResource {
    Body,
    IssueComment,
    ReviewComment,
}

impl Resource {
    fn parse(repository: &RepositorySlug, path: &str) -> Option<Self> {
        let root = format!("/repos/{}", repository.as_str());
        let suffix = path.strip_prefix(&root)?;
        let resource = if suffix.is_empty() {
            Self::Metadata
        } else {
            let (route, query) = suffix.split_once('?').unwrap_or((suffix, ""));
            let parts = route.split('/').skip(1).collect::<Vec<_>>();
            let number = |s: &str| s.parse::<NonZeroU64>().ok();
            let page = || number(query.rsplit_once("page=")?.1);
            match parts.as_slice() {
                ["branches"] => Self::Branches(page()?),
                ["pulls"] => Self::Pulls(page()?),
                ["pulls", id] => Self::Pull(number(id)?),
                ["commits", sha, "check-suites"] => {
                    Self::Suites(CommitSha::try_new((*sha).to_owned()).ok()?, page()?)
                }
                ["commits", sha, "check-runs"] => {
                    Self::Runs(CommitSha::try_new((*sha).to_owned()).ok()?, page()?)
                }
                ["pulls", id, "reviews"] => Self::Reviews(number(id)?, page()?),
                [kind @ ("issues" | "pulls"), id, "comments"] => {
                    Self::Comments(*kind == "pulls", number(id)?, page()?)
                }
                ["issues", id, "reactions"] => {
                    Self::Reactions(ReactionResource::Body, number(id)?, page()?)
                }
                [kind @ ("issues" | "pulls"), "comments", id, "reactions"] => Self::Reactions(
                    if *kind == "pulls" {
                        ReactionResource::ReviewComment
                    } else {
                        ReactionResource::IssueComment
                    },
                    number(id)?,
                    page()?,
                ),
                ["actions", "runs"] => Self::Workflows(
                    CommitSha::try_new(
                        query
                            .strip_prefix("head_sha=")?
                            .split('&')
                            .next()?
                            .to_owned(),
                    )
                    .ok()?,
                    page()?,
                ),
                _ => return None,
            }
        };
        (resource.path(repository) == path).then_some(resource)
    }

    fn path(&self, repository: &RepositorySlug) -> String {
        let root = format!("/repos/{}", repository.as_str());
        let page = |route: String, n: NonZeroU64| {
            format!(
                "{root}{route}{}per_page={PAGE_SIZE}&page={n}",
                if route.contains('?') { '&' } else { '?' }
            )
        };
        match self {
            Self::Metadata => root,
            Self::Branches(n) => page("/branches".into(), *n),
            Self::Pulls(n) => page("/pulls?state=open".into(), *n),
            Self::Pull(n) => format!("{root}/pulls/{n}"),
            Self::Suites(sha, n) => page(
                format!("/commits/{}/check-suites?filter=all", sha.as_str()),
                *n,
            ),
            Self::Runs(sha, n) => page(
                format!("/commits/{}/check-runs?filter=all", sha.as_str()),
                *n,
            ),
            Self::Reviews(id, n) => page(format!("/pulls/{id}/reviews"), *n),
            Self::Comments(review, id, n) => page(
                format!(
                    "/{}/{id}/comments",
                    if *review { "pulls" } else { "issues" }
                ),
                *n,
            ),
            Self::Reactions(kind, id, n) => page(
                format!(
                    "/{}/{id}/reactions",
                    match kind {
                        ReactionResource::Body => "issues",
                        ReactionResource::IssueComment => "issues/comments",
                        ReactionResource::ReviewComment => "pulls/comments",
                    }
                ),
                *n,
            ),
            Self::Workflows(sha, n) => page(
                format!("/actions/runs?head_sha={}&status=completed", sha.as_str()),
                *n,
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct HeadSnapshot {
    sha: String,
    branch: String,
    repository: Option<String>,
    base: String,
}
impl HeadSnapshot {
    fn encode(&self) -> Value {
        json!({
            "sha": self.sha,
            "branch": self.branch,
            "repository": self.repository,
            "base": self.base,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            sha: serde_json::from_value(value.get("sha")?.clone()).ok()?,
            branch: serde_json::from_value(value.get("branch")?.clone()).ok()?,
            repository: serde_json::from_value(value.get("repository")?.clone()).ok()?,
            base: serde_json::from_value(value.get("base")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct PullSnapshot {
    number: u64,
    title: String,
    body: String,
    draft: bool,
    author: Option<String>,
    labels: Vec<String>,
    open: bool,
    merged_at: Option<String>,
    mergeable: Option<bool>,
    head: HeadSnapshot,
}
impl PullSnapshot {
    fn encode(&self) -> Value {
        json!({
            "number": self.number,
            "title": self.title,
            "body": self.body,
            "draft": self.draft,
            "author": self.author,
            "labels": self.labels,
            "open": self.open,
            "merged_at": self.merged_at,
            "mergeable": self.mergeable,
            "head": self.head.encode(),
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            number: serde_json::from_value(value.get("number")?.clone()).ok()?,
            title: serde_json::from_value(value.get("title")?.clone()).ok()?,
            body: serde_json::from_value(value.get("body")?.clone()).ok()?,
            draft: serde_json::from_value(value.get("draft")?.clone()).ok()?,
            author: serde_json::from_value(value.get("author")?.clone()).ok()?,
            labels: serde_json::from_value(value.get("labels")?.clone()).ok()?,
            open: serde_json::from_value(value.get("open")?.clone()).ok()?,
            merged_at: serde_json::from_value(value.get("merged_at")?.clone()).ok()?,
            mergeable: serde_json::from_value(value.get("mergeable")?.clone()).ok()?,
            head: HeadSnapshot::decode(value.get("head")?)?,
        })
    }
}

#[derive(Clone, Debug)]
struct CompletionSnapshot {
    generation: String,
    conclusion: String,
}
impl CompletionSnapshot {
    fn encode(&self) -> Value {
        json!({
            "generation": self.generation,
            "conclusion": self.conclusion,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            generation: serde_json::from_value(value.get("generation")?.clone()).ok()?,
            conclusion: serde_json::from_value(value.get("conclusion")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct SuiteSnapshot {
    id: u64,
    completion: Option<CompletionSnapshot>,
}
impl SuiteSnapshot {
    fn encode(&self) -> Value {
        json!({
            "id": self.id,
            "completion": self.completion.as_ref().map(CompletionSnapshot::encode),
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            id: serde_json::from_value(value.get("id")?.clone()).ok()?,
            completion: if value.get("completion")?.is_null() {
                None
            } else {
                Some(CompletionSnapshot::decode(&value["completion"])?)
            },
        })
    }
}

#[derive(Clone, Debug)]
struct RunSnapshot {
    id: u64,
    generation: String,
    name: String,
    conclusion: String,
}
impl RunSnapshot {
    fn encode(&self) -> Value {
        json!({
            "id": self.id,
            "generation": self.generation,
            "name": self.name,
            "conclusion": self.conclusion,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            id: serde_json::from_value(value.get("id")?.clone()).ok()?,
            generation: serde_json::from_value(value.get("generation")?.clone()).ok()?,
            name: serde_json::from_value(value.get("name")?.clone()).ok()?,
            conclusion: serde_json::from_value(value.get("conclusion")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct ReviewSnapshot {
    id: u64,
    reviewer: Option<String>,
    state: String,
    commit: Option<String>,
}
impl ReviewSnapshot {
    fn encode(&self) -> Value {
        json!({
            "id": self.id,
            "reviewer": self.reviewer,
            "state": self.state,
            "commit": self.commit,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            id: serde_json::from_value(value.get("id")?.clone()).ok()?,
            reviewer: serde_json::from_value(value.get("reviewer")?.clone()).ok()?,
            state: serde_json::from_value(value.get("state")?.clone()).ok()?,
            commit: serde_json::from_value(value.get("commit")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct WorkflowSnapshot {
    id: u64,
    workflow: u64,
    attempt: u64,
    branch: String,
    name: String,
    conclusion: String,
}
impl WorkflowSnapshot {
    fn encode(&self) -> Value {
        json!({
            "id": self.id,
            "workflow": self.workflow,
            "attempt": self.attempt,
            "branch": self.branch,
            "name": self.name,
            "conclusion": self.conclusion,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            id: serde_json::from_value(value.get("id")?.clone()).ok()?,
            workflow: serde_json::from_value(value.get("workflow")?.clone()).ok()?,
            attempt: serde_json::from_value(value.get("attempt")?.clone()).ok()?,
            branch: serde_json::from_value(value.get("branch")?.clone()).ok()?,
            name: serde_json::from_value(value.get("name")?.clone()).ok()?,
            conclusion: serde_json::from_value(value.get("conclusion")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct BranchSnapshot {
    branch: String,
    head: String,
}
impl BranchSnapshot {
    fn encode(&self) -> Value {
        json!({
            "branch": self.branch,
            "head": self.head,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            branch: serde_json::from_value(value.get("branch")?.clone()).ok()?,
            head: serde_json::from_value(value.get("head")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
struct ReactionSnapshot {
    reviewer: String,
    content: String,
}
impl ReactionSnapshot {
    fn encode(&self) -> Value {
        json!({
            "reviewer": self.reviewer,
            "content": self.content,
        })
    }
    fn decode(value: &Value) -> Option<Self> {
        Some(Self {
            reviewer: serde_json::from_value(value.get("reviewer")?.clone()).ok()?,
            content: serde_json::from_value(value.get("content")?.clone()).ok()?,
        })
    }
}

#[derive(Clone, Debug)]
enum Snapshot {
    Metadata(String),
    Branches(Vec<BranchSnapshot>),
    Pulls(Vec<u64>),
    Pull(PullSnapshot),
    Suites(Vec<SuiteSnapshot>),
    Runs(Vec<RunSnapshot>),
    Reviews(Vec<ReviewSnapshot>),
    Comments(Vec<u64>),
    Reactions(Vec<ReactionSnapshot>),
    Workflows(u64, Vec<WorkflowSnapshot>),
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}
fn id(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| *n > 0)
}

impl Snapshot {
    fn capture(
        resource: &Resource,
        value: &Value,
        repository: &RepositorySlug,
        reviewers: &[RepoWatchAuthorLogin],
    ) -> Option<Self> {
        Some(match resource {
            Resource::Metadata => Self::Metadata(string(&value["default_branch"])?),
            Resource::Branches(_) => Self::Branches(
                value
                    .as_array()?
                    .iter()
                    .map(|v| {
                        Some(BranchSnapshot {
                            branch: string(&v["name"])?,
                            head: string(&v["commit"]["sha"])?,
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
            Resource::Pulls(_) => Self::Pulls(
                value
                    .as_array()?
                    .iter()
                    .map(|v| id(&v["number"]))
                    .collect::<Option<_>>()?,
            ),
            Resource::Pull(_) => Self::Pull(PullSnapshot {
                number: id(&value["number"])?,
                title: string(&value["title"])?,
                body: if value["body"].is_null() {
                    String::new()
                } else {
                    string(&value["body"])?
                },
                draft: value["draft"].as_bool()?,
                author: if value["user"].is_null() {
                    None
                } else {
                    Some(string(&value["user"]["login"])?)
                },
                labels: value["labels"]
                    .as_array()?
                    .iter()
                    .map(|v| string(&v["name"]))
                    .collect::<Option<_>>()?,
                open: match value["state"].as_str()? {
                    "open" => true,
                    "closed" => false,
                    _ => return None,
                },
                merged_at: if value["merged_at"].is_null() {
                    None
                } else {
                    let timestamp = string(&value["merged_at"])?;
                    crate::provider::github_timestamp(&timestamp)?;
                    Some(timestamp)
                },
                mergeable: value["mergeable"].as_bool(),
                head: HeadSnapshot {
                    sha: string(&value["head"]["sha"])?,
                    branch: string(&value["head"]["ref"])?,
                    repository: if value["head"]["repo"].is_null() {
                        None
                    } else {
                        Some(string(&value["head"]["repo"]["full_name"])?)
                    },
                    base: string(&value["base"]["ref"])?,
                },
            }),
            Resource::Suites(_, _) => Self::Suites(
                value["check_suites"]
                    .as_array()?
                    .iter()
                    .map(|v| {
                        Some(SuiteSnapshot {
                            id: id(&v["id"])?,
                            completion: if v["status"] == "completed" {
                                Some(CompletionSnapshot {
                                    generation: string(&v["updated_at"])?,
                                    conclusion: string(&v["conclusion"])?,
                                })
                            } else {
                                None
                            },
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
            Resource::Runs(_, _) => Self::Runs(
                value["check_runs"]
                    .as_array()?
                    .iter()
                    .filter(|v| v["status"] == "completed")
                    .map(|v| {
                        Some(RunSnapshot {
                            id: id(&v["id"])?,
                            generation: string(&v["completed_at"])?,
                            name: string(&v["name"])?,
                            conclusion: string(&v["conclusion"])?,
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
            Resource::Reviews(_, _) => Self::Reviews(
                value
                    .as_array()?
                    .iter()
                    .filter(|v| v["state"] != "PENDING")
                    .map(|v| {
                        Some(ReviewSnapshot {
                            id: id(&v["id"])?,
                            reviewer: if v["user"].is_null() {
                                None
                            } else {
                                Some(string(&v["user"]["login"])?)
                            },
                            state: string(&v["state"])?,
                            commit: string(&v["commit_id"]),
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
            Resource::Comments(_, _, _) => Self::Comments(
                value
                    .as_array()?
                    .iter()
                    .map(|v| id(&v["id"]))
                    .collect::<Option<_>>()?,
            ),
            Resource::Reactions(_, _, _) => Self::Reactions(
                value
                    .as_array()?
                    .iter()
                    .filter_map(|v| {
                        let login = v["user"]["login"].as_str()?;
                        reviewers
                            .iter()
                            .find(|r| r.as_str().eq_ignore_ascii_case(login))
                            .map(|reviewer| {
                                Some(ReactionSnapshot {
                                    reviewer: reviewer.as_str().to_ascii_lowercase(),
                                    content: string(&v["content"])?,
                                })
                            })
                    })
                    .collect::<Option<_>>()?,
            ),
            Resource::Workflows(_, _) => Self::Workflows(
                value["total_count"].as_u64()?,
                value["workflow_runs"]
                    .as_array()?
                    .iter()
                    .filter(|v| {
                        v["status"] == "completed"
                            && !v["head_repository"].is_null()
                            && !v["head_branch"].is_null()
                    })
                    .filter(|v| {
                        v["head_repository"]["full_name"]
                            .as_str()
                            .and_then(|name| RepositorySlug::try_new(name.to_owned()).ok())
                            .as_ref()
                            == Some(repository)
                    })
                    .map(|v| {
                        Some(WorkflowSnapshot {
                            id: id(&v["id"])?,
                            workflow: id(&v["workflow_id"])?,
                            attempt: id(&v["run_attempt"])?,
                            branch: string(&v["head_branch"])?,
                            name: string(&v["name"])?,
                            conclusion: string(&v["conclusion"])?,
                        })
                    })
                    .collect::<Option<_>>()?,
            ),
        })
    }

    fn encode(&self) -> Value {
        match self {
            Self::Metadata(v) => json!(["metadata", v]),
            Self::Branches(v) => json!([
                "branches",
                v.iter().map(BranchSnapshot::encode).collect::<Vec<_>>()
            ]),
            Self::Pulls(v) => json!(["pulls", v]),
            Self::Pull(v) => json!(["pull", v.encode()]),
            Self::Suites(v) => json!([
                "suites",
                v.iter().map(SuiteSnapshot::encode).collect::<Vec<_>>()
            ]),
            Self::Runs(v) => json!([
                "runs",
                v.iter().map(RunSnapshot::encode).collect::<Vec<_>>()
            ]),
            Self::Reviews(v) => json!([
                "reviews",
                v.iter().map(ReviewSnapshot::encode).collect::<Vec<_>>()
            ]),
            Self::Comments(v) => json!(["comments", v]),
            Self::Reactions(v) => json!([
                "reactions",
                v.iter().map(ReactionSnapshot::encode).collect::<Vec<_>>()
            ]),
            Self::Workflows(total, v) => json!([
                "workflows",
                [
                    total,
                    v.iter().map(WorkflowSnapshot::encode).collect::<Vec<_>>()
                ]
            ]),
        }
    }

    fn decode(resource: &Resource, value: Value) -> Option<Self> {
        if value.as_array()?.len() != 2 {
            return None;
        }
        let tag = value[0].as_str()?;
        let data = value[1].clone();
        Some(match (resource, tag) {
            (Resource::Metadata, "metadata") => Self::Metadata(serde_json::from_value(data).ok()?),
            (Resource::Branches(_), "branches") => Self::Branches(
                crate::observation_decode::array(&data, BranchSnapshot::decode)?,
            ),
            (Resource::Pulls(_), "pulls") => Self::Pulls(serde_json::from_value(data).ok()?),
            (Resource::Pull(_), "pull") => Self::Pull(PullSnapshot::decode(&data)?),
            (Resource::Suites(_, _), "suites") => Self::Suites(crate::observation_decode::array(
                &data,
                SuiteSnapshot::decode,
            )?),
            (Resource::Runs(_, _), "runs") => Self::Runs(crate::observation_decode::array(
                &data,
                RunSnapshot::decode,
            )?),
            (Resource::Reviews(_, _), "reviews") => Self::Reviews(
                crate::observation_decode::array(&data, ReviewSnapshot::decode)?,
            ),
            (Resource::Comments(_, _, _), "comments") => {
                Self::Comments(serde_json::from_value(data).ok()?)
            }
            (Resource::Reactions(_, _, _), "reactions") => Self::Reactions(
                crate::observation_decode::array(&data, ReactionSnapshot::decode)?,
            ),
            (Resource::Workflows(_, _), "workflows") => {
                let total = data[0].as_u64()?;
                let runs = crate::observation_decode::array(&data[1], WorkflowSnapshot::decode)?;
                Self::Workflows(total, runs)
            }
            _ => return None,
        })
    }

    fn provider_value(&self, repository: &RepositorySlug) -> Value {
        match self {
            Self::Metadata(branch) => json!({"default_branch":branch}),
            Self::Branches(items) => json!(items.iter().map(|BranchSnapshot { branch: name, head: sha }| json!({"name":name,"commit":{"sha":sha}})).collect::<Vec<_>>()),
            Self::Pulls(items) => json!(items.iter().map(|number| json!({"number":number})).collect::<Vec<_>>()),
            Self::Pull(PullSnapshot { number,title,body,draft,author,labels,open,merged_at,mergeable,head:HeadSnapshot { sha,branch:head,repository:head_repository,base } }) => json!({
                "number":number,"title":title,"body":body,"draft":draft,"user":author.as_ref().map(|login| json!({"login":login})),"labels":labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>(),
                "state":if *open {"open"} else {"closed"},"merged_at":merged_at,"mergeable":mergeable,
                "head":{"sha":sha,"ref":head,"repo":head_repository.as_ref().map(|name| json!({"full_name":name}))},"base":{"ref":base},
            }),
            Self::Suites(items) => json!({"check_suites":items.iter().map(|SuiteSnapshot { id,completion }| match completion { Some(CompletionSnapshot { generation,conclusion }) => json!({"id":id,"status":"completed","updated_at":generation,"conclusion":conclusion}),None => json!({"id":id,"status":"pending"}) }).collect::<Vec<_>>() }),
            Self::Runs(items) => json!({"check_runs":items.iter().map(|RunSnapshot { id,generation,name,conclusion }| json!({"id":id,"status":"completed","completed_at":generation,"name":name,"conclusion":conclusion})).collect::<Vec<_>>() }),
            Self::Reviews(items) => json!(items.iter().map(|ReviewSnapshot { id,reviewer,state,commit }| json!({"id":id,"user":reviewer.as_ref().map(|login| json!({"login":login})),"state":state,"commit_id":commit})).collect::<Vec<_>>()),
            Self::Comments(items) => json!(items.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()),
            Self::Reactions(items) => json!(items.iter().map(|ReactionSnapshot { reviewer:login,content }| json!({"user":{"login":login},"content":content})).collect::<Vec<_>>()),
            Self::Workflows(total,items) => json!({"total_count":total,"workflow_runs":items.iter().map(|WorkflowSnapshot { id,workflow,attempt,branch,name,conclusion }| json!({"id":id,"workflow_id":workflow,"run_attempt":attempt,"head_branch":branch,"head_repository":{"full_name":repository.as_str()},"name":name,"conclusion":conclusion,"status":"completed"})).collect::<Vec<_>>() }),
        }
    }
}

struct AcceptedPage {
    validators: HttpValidators,
    has_next: bool,
    snapshot: Snapshot,
}

fn reviewer_set(reviewers: &[RepoWatchAuthorLogin]) -> String {
    json!(
        reviewers
            .iter()
            .map(|r| r.as_str().to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
    )
    .to_string()
}

impl RepoWatchStore {
    /// Invalidates transport validators and snapshots before composing a poller under a changed reviewer set.
    pub async fn prepare_poll_cache(
        &self,
        repository: &RepositorySlug,
        reviewers: &[RepoWatchAuthorLogin],
    ) -> Result<(), StoreError> {
        let reviewers = reviewer_set(reviewers);
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO poll_cache_reviewers (repository, reviewers) VALUES ($1,$2::jsonb) ON CONFLICT DO NOTHING").bind(repository.as_str()).bind(&reviewers).execute(&mut *tx).await?;
        let same: bool = sqlx::query_scalar(
            "SELECT reviewers = $2::jsonb FROM poll_cache_reviewers WHERE repository=$1 FOR UPDATE",
        )
        .bind(repository.as_str())
        .bind(&reviewers)
        .fetch_one(&mut *tx)
        .await?;
        if !same {
            sqlx::query("DELETE FROM poll_cache_page WHERE repository=$1")
                .bind(repository.as_str())
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE poll_cache_reviewers SET reviewers=$2::jsonb WHERE repository=$1")
                .bind(repository.as_str())
                .bind(&reviewers)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn cached_page(
        &self,
        repository: &RepositorySlug,
        reviewers: &[RepoWatchAuthorLogin],
        resource: &Resource,
    ) -> Result<Option<AcceptedPage>, StoreError> {
        let row = sqlx::query("SELECT p.etag,p.last_modified,p.has_next,p.snapshot::text AS snapshot FROM poll_cache_page p JOIN poll_cache_reviewers r USING(repository) WHERE p.repository=$1 AND p.resource_key=$2 AND r.reviewers=$3::jsonb")
            .bind(repository.as_str()).bind(resource.path(repository)).bind(reviewer_set(reviewers)).fetch_optional(&self.pool).await?;
        row.map(|row| {
            let etag = row.try_get("etag")?;
            let last_modified = row.try_get("last_modified")?;
            let has_next = row.try_get("has_next")?;
            let snapshot: String = row.try_get("snapshot")?;
            let snapshot = Snapshot::decode(
                resource,
                serde_json::from_str(&snapshot).map_err(|_| StoreError::InvalidPollCache)?,
            )
            .ok_or(StoreError::InvalidPollCache)?;
            if let Snapshot::Reactions(items) = &snapshot
                && items.iter().any(
                    |ReactionSnapshot {
                         reviewer: login, ..
                     }| {
                        !reviewers
                            .iter()
                            .any(|r| r.as_str().eq_ignore_ascii_case(login))
                    },
                )
            {
                return Err(StoreError::InvalidPollCache);
            }
            Ok(AcceptedPage {
                validators: HttpValidators {
                    etag,
                    last_modified,
                },
                has_next,
                snapshot,
            })
        })
        .transpose()
    }

    async fn retain_poll_pages(
        &self,
        repository: &RepositorySlug,
        reviewers: &[RepoWatchAuthorLogin],
        pages: Vec<(Resource, Option<AcceptedPage>)>,
        retained: BTreeSet<String>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let same: Option<bool> = sqlx::query_scalar(
            "SELECT reviewers=$2::jsonb FROM poll_cache_reviewers WHERE repository=$1 FOR UPDATE",
        )
        .bind(repository.as_str())
        .bind(reviewer_set(reviewers))
        .fetch_optional(&mut *tx)
        .await?;
        if same != Some(true) {
            return Err(StoreError::InvalidPollCache);
        }
        for (resource, page) in pages {
            if !retained.contains(&resource.path(repository)) {
                continue;
            }
            if let Some(page) = page {
                sqlx::query("INSERT INTO poll_cache_page (repository,resource_key,etag,last_modified,has_next,snapshot) VALUES ($1,$2,$3,$4,$5,$6::jsonb) ON CONFLICT(repository,resource_key) DO UPDATE SET etag=EXCLUDED.etag,last_modified=EXCLUDED.last_modified,has_next=EXCLUDED.has_next,snapshot=EXCLUDED.snapshot")
                    .bind(repository.as_str()).bind(resource.path(repository)).bind(page.validators.etag).bind(page.validators.last_modified).bind(page.has_next).bind(page.snapshot.encode().to_string()).execute(&mut *tx).await?;
            } else {
                sqlx::query("DELETE FROM poll_cache_page WHERE repository=$1 AND resource_key=$2")
                    .bind(repository.as_str())
                    .bind(resource.path(repository))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        sqlx::query(
            "DELETE FROM poll_cache_page WHERE repository=$1 AND NOT (resource_key = ANY($2))",
        )
        .bind(repository.as_str())
        .bind(retained.into_iter().collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) struct CachedObservationRead<'a, T> {
    pub io: &'a T,
    pub store: &'a RepoWatchStore,
    pub repository: &'a RepositorySlug,
    pub reviewers: &'a [RepoWatchAuthorLogin],
    pending: Mutex<Vec<(Resource, Option<AcceptedPage>)>>,
    retained: Mutex<BTreeSet<String>>,
}

impl<'a, T> CachedObservationRead<'a, T> {
    pub(crate) fn new(
        io: &'a T,
        store: &'a RepoWatchStore,
        repository: &'a RepositorySlug,
        reviewers: &'a [RepoWatchAuthorLogin],
    ) -> Self {
        Self {
            io,
            store,
            repository,
            reviewers,
            pending: Mutex::new(Vec::new()),
            retained: Mutex::new(BTreeSet::new()),
        }
    }
    pub(crate) async fn retain(&self) -> Result<(), StoreError> {
        self.store
            .retain_poll_pages(
                self.repository,
                self.reviewers,
                std::mem::take(&mut *self.pending.lock().await),
                std::mem::take(&mut *self.retained.lock().await),
            )
            .await
    }
}

impl<T: ConditionalObservationRead> GitHubObservationRead for CachedObservationRead<'_, T> {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        let resource =
            Resource::parse(self.repository, path).ok_or(ObservationError::InvalidResponse)?;
        let pull = matches!(resource, Resource::Pull(_));
        let cached = self
            .store
            .cached_page(self.repository, self.reviewers, &resource)
            .await
            .map_err(ObservationError::Cache)?;
        let (value, has_next) = match self
            .io
            .conditional_page(path, cached.as_ref().map(|p| &p.validators))
            .await?
        {
            ConditionalPage::Unchanged => {
                let page = cached.ok_or(ObservationError::InvalidResponse)?;
                (page.snapshot.provider_value(self.repository), page.has_next)
            }
            ConditionalPage::Modified {
                body,
                has_next,
                validators,
            } => {
                let value: Value =
                    serde_json::from_slice(&body).map_err(|_| ObservationError::InvalidResponse)?;
                let page = if validators.etag.is_some() || validators.last_modified.is_some() {
                    Snapshot::capture(&resource, &value, self.repository, self.reviewers).map(
                        |snapshot| AcceptedPage {
                            validators,
                            has_next,
                            snapshot,
                        },
                    )
                } else {
                    None
                };
                self.pending.lock().await.push((resource, page));
                (value, has_next)
            }
        };
        if !pull || value["state"].as_str() == Some("open") {
            self.retained.lock().await.insert(path.to_owned());
        }
        Ok((value, has_next))
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        self.io.threads(request).await
    }
}

/// Accepts one complete observation before retaining its conditional transport state.
/// The caller prepares the reviewer set before composing the repository poller.
pub async fn poll_with_cache(
    io: &impl ConditionalObservationRead,
    store: &RepoWatchStore,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    producer: EventProducer,
    retention: std::time::Duration,
) -> Result<FrontierEventAdmission, ObservationError> {
    let started = std::time::Instant::now();
    let baseline = store
        .ingest_baseline(repository)
        .await
        .map_err(ObservationError::Cache)?;
    let cached = CachedObservationRead::new(io, store, repository, reviewers);
    let counted = ObservationReadCounts {
        io: &cached,
        requests: std::sync::atomic::AtomicUsize::new(0),
        comments: std::sync::atomic::AtomicUsize::new(0),
    };
    let observed = fetch_observation(
        &counted,
        repository,
        reviewers,
        baseline.observation.as_ref(),
        &baseline
            .merged_baselines
            .iter()
            .map(|entry| entry.state.clone())
            .collect::<Vec<_>>(),
    )
    .await?;
    let admission = store
        .ingest_observation(&baseline, &observed, producer, retention)
        .await
        .map_err(ObservationError::Cache)?;
    if matches!(
        admission,
        FrontierEventAdmission::Committed { .. } | FrontierEventAdmission::Unchanged
    ) {
        cached.retain().await.map_err(ObservationError::Cache)?;
        let state = observed.observation.state();
        tracing::info!(
            repository = repository.as_str(),
            ?producer,
            open_pull_requests = state
                .pull_requests()
                .iter()
                .filter(|p| p.lifecycle() == RepoWatchPullRequestLifecycle::Open)
                .count(),
            terminal_pull_requests = state
                .pull_requests()
                .iter()
                .filter(|p| p.lifecycle() != RepoWatchPullRequestLifecycle::Open)
                .count(),
            previous_merged_baselines = baseline.merged_baselines.len(),
            branches = state.branch_heads().len(),
            workflow_runs = state.workflow_runs().len(),
            requests = counted.requests.load(std::sync::atomic::Ordering::Relaxed),
            comments = counted.comments.load(std::sync::atomic::Ordering::Relaxed),
            elapsed_ms = started.elapsed().as_millis(),
            "repository-watch observation completed"
        );
    }
    Ok(admission)
}
struct ObservationReadCounts<'a, T> {
    io: &'a T,
    requests: std::sync::atomic::AtomicUsize,
    comments: std::sync::atomic::AtomicUsize,
}

impl<T: GitHubObservationRead> GitHubObservationRead for ObservationReadCounts<'_, T> {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = self.io.page(path).await?;
        if path
            .split('?')
            .next()
            .is_some_and(|path| path.ends_with("/comments"))
        {
            self.comments.fetch_add(
                result.0.as_array().map_or(0, Vec::len),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        Ok(result)
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.io.threads(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_reject_noncanonical_and_unbounded_resource_paths() {
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        for path in [
            "/repos/example/project/branches?per_page=100&page=0",
            "/repos/example/project/branches?per_page=100&page=01",
            "/repos/example/project/branches?per_page=100&page=18446744073709551616",
            "/repos/example/project/branches?per_page=100&page=1&token=secret",
            "/repos/example/project/pulls/1?ignored=1",
            "/repos/other/project",
        ] {
            assert!(Resource::parse(&repository, path).is_none(), "{path}");
        }
    }

    #[test]
    fn cached_pull_details_preserve_the_provider_merge_time() {
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let resource = Resource::Pull(NonZeroU64::new(1).expect("fixture PR"));
        let response = json!({
            "number": 1, "title": "Merged change", "body": "", "draft": false, "user": null, "labels": [],
            "state": "closed", "merged_at": "2026-09-06T12:34:56Z", "mergeable": null,
            "head": {"sha": "1111111111111111111111111111111111111111", "ref": "feature", "repo": {"full_name": "example/project"}},
            "base": {"ref": "main"}
        });
        let captured = Snapshot::capture(&resource, &response, &repository, &[]).expect("snapshot");
        let restarted = Snapshot::decode(&resource, captured.encode()).expect("restart");
        assert_eq!(
            restarted.provider_value(&repository)["merged_at"],
            response["merged_at"],
            "a conditional response must retain the merge-time expiration anchor"
        );
    }

    #[test]
    fn accepted_reactions_retain_only_selected_actors_and_normalized_fields() {
        let repository =
            RepositorySlug::try_new(String::from("example/project")).expect("repository");
        let reviewers =
            [RepoWatchAuthorLogin::try_new(String::from("reviewer")).expect("reviewer")];
        let resource = Resource::parse(
            &repository,
            "/repos/example/project/issues/1/reactions?per_page=100&page=1",
        )
        .expect("resource");
        let response = json!([
            {"user":{"login":"REVIEWER","unneeded":"discard"},"content":"eyes","extra":"discard"},
            {"user":{"login":"outsider"},"content":"-1"},
        ]);
        let snapshot = Snapshot::capture(&resource, &response, &repository, &reviewers)
            .expect("accepted reactions");
        assert_eq!(
            snapshot.encode(),
            json!(["reactions", [{"reviewer":"reviewer", "content":"eyes"}]])
        );
        assert!(Snapshot::decode(&Resource::Metadata, snapshot.encode()).is_none());
    }
}
