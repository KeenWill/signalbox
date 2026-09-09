//! Durable progress within a bounded repository reconciliation.

use super::*;
use crate::ingest::{IngestBaseline, RepositoryObservation};
use crate::provider::{fetch_poll_pull, fetch_poll_repository, fetch_poll_workflows};
use signalbox_session_ownership::PullRequestNumber;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Cursor {
    // None discovers the repository; an empty list reconciles workflows.
    pulls: Option<Vec<PullRequestNumber>>,
    pages: BTreeMap<String, Value>,
    threads: BTreeMap<String, Value>,
    retained: BTreeSet<String>,
}

impl Cursor {
    fn decode(value: Value) -> Option<Self> {
        Some(Self {
            pulls: if value["pulls"].is_null() {
                None
            } else {
                Some(crate::observation_decode::array(&value["pulls"], |v| {
                    Some(PullRequestNumber::new(crate::observation_decode::positive(
                        v,
                    )?))
                })?)
            },
            pages: serde_json::from_value(value.get("pages")?.clone()).ok()?,
            threads: serde_json::from_value(value.get("threads")?.clone()).ok()?,
            retained: serde_json::from_value(value.get("retained")?.clone()).ok()?,
        })
    }
    fn encode(&self) -> Value {
        json!({"pulls":self.pulls.as_ref().map(|pulls| pulls.iter().map(|p|p.get()).collect::<Vec<_>>()),
            "pages":self.pages,"threads":self.threads,"retained":self.retained})
    }
    async fn load(
        store: &RepoWatchStore,
        repository: &RepositorySlug,
    ) -> Result<Self, ObservationError> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT cursor::text FROM poll_cursor WHERE repository=$1")
                .bind(repository.as_str())
                .fetch_optional(&store.pool)
                .await
                .map_err(StoreError::from)
                .map_err(ObservationError::Cache)?;
        value
            .map(|text| {
                serde_json::from_str(&text)
                    .ok()
                    .and_then(Self::decode)
                    .ok_or(ObservationError::Cache(StoreError::InvalidPollCache))
            })
            .unwrap_or_else(|| Ok(Self::default()))
    }
    async fn save(
        &self,
        store: &RepoWatchStore,
        repository: &RepositorySlug,
    ) -> Result<(), ObservationError> {
        sqlx::query("INSERT INTO poll_cursor (repository,cursor) VALUES ($1,$2::jsonb) ON CONFLICT(repository) DO UPDATE SET cursor=EXCLUDED.cursor")
            .bind(repository.as_str()).bind(self.encode().to_string()).execute(&store.pool).await.map_err(StoreError::from).map_err(ObservationError::Cache)?;
        Ok(())
    }
}

struct ResumableRead<'a, T> {
    cached: CachedObservationRead<'a, T>,
    cursor: Mutex<Cursor>,
    requests: AtomicUsize,
    limit: usize,
}

impl<T> ResumableRead<'_, T> {
    fn reserve(&self) -> Result<(), ObservationError> {
        self.requests
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < self.limit).then_some(n + 1)
            })
            .map(|_| ())
            .map_err(|_| ObservationError::RequestBudgetExceeded { limit: self.limit })
    }
}

impl<T: ConditionalObservationRead> GitHubObservationRead for ResumableRead<'_, T> {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        let resource = Resource::parse(self.cached.repository, path)
            .ok_or(ObservationError::InvalidResponse)?;
        let mut cursor = self.cursor.lock().await;
        if let Some(value) = cursor.pages.get(path) {
            let snapshot = Snapshot::decode(&resource, value["snapshot"].clone())
                .ok_or(ObservationError::Cache(StoreError::InvalidPollCache))?;
            let next = value["next"]
                .as_bool()
                .ok_or(ObservationError::Cache(StoreError::InvalidPollCache))?;
            return Ok((snapshot.provider_value(self.cached.repository), next));
        }
        self.reserve()?;
        let (value, next) = self.cached.page(path).await?;
        let snapshot = Snapshot::capture(
            &resource,
            &value,
            self.cached.repository,
            self.cached.reviewers,
        )
        .ok_or(ObservationError::InvalidResponse)?;
        cursor.pages.insert(
            path.to_owned(),
            json!({"snapshot":snapshot.encode(),"next":next}),
        );
        Ok((value, next))
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        let key = request["variables"].to_string();
        let mut cursor = self.cursor.lock().await;
        if let Some(snapshot) = cursor.threads.get(&key) {
            return Ok(json!({"data":{"repository":{"pullRequest":{"reviewThreads":snapshot}}}}));
        }
        self.reserve()?;
        let value = self.cached.threads(request).await?;
        if value
            .get("errors")
            .is_some_and(|errors| errors.as_array().is_none_or(|errors| !errors.is_empty()))
        {
            return Err(ObservationError::InvalidResponse);
        }
        let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
        let nodes = crate::observation_decode::array(&connection["nodes"], |node| {
            Some(json!({
                "id":node["id"].as_str()?,"isResolved":node["isResolved"].as_bool()?
            }))
        })
        .ok_or(ObservationError::InvalidResponse)?;
        let next = connection["pageInfo"]["hasNextPage"]
            .as_bool()
            .ok_or(ObservationError::InvalidResponse)?;
        let after = connection["pageInfo"]["endCursor"].as_str();
        if next && after.is_none() {
            return Err(ObservationError::InvalidResponse);
        }
        let snapshot = json!({"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":after}});
        cursor.threads.insert(key, snapshot.clone());
        Ok(json!({"data":{"repository":{"pullRequest":{"reviewThreads":snapshot}}}}))
    }
}

enum Step {
    Discovered(RepositoryObservation, Vec<PullRequestNumber>),
    Pull(RepositoryObservation),
    Workflows(RepositoryObservation),
}

async fn step(
    io: &ResumableRead<'_, impl ConditionalObservationRead>,
    baseline: &IngestBaseline,
) -> Result<Step, ObservationError> {
    let pulls = io.cursor.lock().await.pulls.clone();
    let cached = &io.cached;
    match pulls {
        None => fetch_poll_repository(io, cached.repository, cached.reviewers, baseline)
            .await
            .map(|(observation, pulls)| Step::Discovered(observation, pulls)),
        Some(pulls) if !pulls.is_empty() => {
            fetch_poll_pull(io, cached.repository, cached.reviewers, baseline, pulls[0])
                .await
                .map(Step::Pull)
        }
        Some(_) => fetch_poll_workflows(io, cached.repository, cached.reviewers, baseline)
            .await
            .map(Step::Workflows),
    }
}

/// Spends at most the request budget, admitting completed subjects and retaining unfinished reads.
/// Returns true when repository reconciliation completes and clears its cursor.
pub async fn poll_with_cache(
    io: &impl ConditionalObservationRead,
    store: &RepoWatchStore,
    repository: &RepositorySlug,
    reviewers: &[RepoWatchAuthorLogin],
    retention: std::time::Duration,
    request_budget: std::num::NonZeroUsize,
) -> Result<bool, ObservationError> {
    let response = io.conditional_page("/rate_limit", None).await?;
    let ConditionalPage::Modified { body, .. } = response else {
        return Err(ObservationError::InvalidResponse);
    };
    let quota: Value =
        serde_json::from_slice(&body).map_err(|_| ObservationError::InvalidResponse)?;
    let remaining = quota["resources"]["core"]["remaining"]
        .as_u64()
        .ok_or(ObservationError::InvalidResponse)?;
    if remaining < request_budget.get() as u64 {
        return Err(ObservationError::RestBudgetUnavailable {
            required: request_budget.get(),
            remaining,
        });
    }
    let cursor = Cursor::load(store, repository).await?;
    let io = ResumableRead {
        cached: CachedObservationRead::new(io, store, repository, reviewers),
        cursor: Mutex::new(cursor),
        requests: AtomicUsize::new(1),
        limit: request_budget.get(),
    };
    loop {
        let baseline = store
            .ingest_baseline(repository)
            .await
            .map_err(ObservationError::Cache)?;
        let result = step(&io, &baseline).await;
        let mut cursor = io.cursor.lock().await;
        cursor
            .retained
            .extend(io.cached.retained.lock().await.iter().cloned());
        match result {
            Err(error) if budget_exhausted(&error) => {
                cursor.save(store, repository).await?;
                store
                    .retain_poll_pages(
                        repository,
                        reviewers,
                        std::mem::take(&mut *io.cached.pending.lock().await),
                        cursor.retained.clone(),
                        false,
                    )
                    .await
                    .map_err(ObservationError::Cache)?;
                tracing::info!(repository=repository.as_str(),producer=?EventProducer::Poll,requests=io.requests.load(Ordering::Relaxed),outcome="partial","repository-watch observation completed");
                return Ok(false);
            }
            Err(error) => return Err(error),
            Ok(step) => {
                let observed = match &step {
                    Step::Discovered(observed, _)
                    | Step::Pull(observed)
                    | Step::Workflows(observed) => observed,
                };
                let admission = store
                    .ingest_observation(&baseline, observed, EventProducer::Poll, retention)
                    .await
                    .map_err(ObservationError::Cache)?;
                if !matches!(
                    admission,
                    FrontierEventAdmission::Committed { .. } | FrontierEventAdmission::Unchanged
                ) {
                    return Err(ObservationError::Cache(
                        StoreError::InvalidComparisonBaseline,
                    ));
                }
                let complete = matches!(step, Step::Workflows(_));
                match step {
                    Step::Discovered(_, pulls) => cursor.pulls = Some(pulls),
                    Step::Pull(_) => {
                        if let Some(pulls) = &mut cursor.pulls {
                            pulls.remove(0);
                        }
                    }
                    Step::Workflows(_) => {}
                }
                cursor.pages.clear();
                cursor.threads.clear();
                store
                    .retain_poll_pages(
                        repository,
                        reviewers,
                        std::mem::take(&mut *io.cached.pending.lock().await),
                        cursor.retained.clone(),
                        complete,
                    )
                    .await
                    .map_err(ObservationError::Cache)?;
                if complete {
                    sqlx::query("DELETE FROM poll_cursor WHERE repository=$1")
                        .bind(repository.as_str())
                        .execute(&store.pool)
                        .await
                        .map_err(StoreError::from)
                        .map_err(ObservationError::Cache)?;
                    tracing::info!(repository=repository.as_str(),producer=?EventProducer::Poll,requests=io.requests.load(Ordering::Relaxed),outcome="succeeded","repository-watch observation completed");
                    return Ok(true);
                }
                cursor.save(store, repository).await?;
            }
        }
    }
}

fn budget_exhausted(error: &ObservationError) -> bool {
    match error {
        ObservationError::RequestBudgetExceeded { .. } => true,
        ObservationError::Request { source, .. } => budget_exhausted(source),
        _ => false,
    }
}

pub(super) async fn webhook_observed(
    store: &RepoWatchStore,
    repository: &RepositorySlug,
    number: PullRequestNumber,
) -> Result<(), ObservationError> {
    let mut cursor = Cursor::load(store, repository).await?;
    if let Some(pulls) = &mut cursor.pulls {
        let current = pulls.first() == Some(&number);
        pulls.retain(|pull| *pull != number);
        if current {
            cursor.pages.clear();
            cursor.threads.clear();
        }
        cursor.save(store, repository).await?;
    }
    Ok(())
}
