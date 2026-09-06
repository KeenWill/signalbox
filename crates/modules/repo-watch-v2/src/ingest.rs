//! Serialized repository observation and generation-fenced ingest.

use std::{future::Future, sync::Arc, time::Duration};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::Value;
use signalbox_ownership_seam::{
    BranchName, CommitSha, OffsetDateTime, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierV1, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepositorySlug, UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
};
use tokio::{
    sync::{Notify, watch},
    time::{Instant, sleep_until},
};

use crate::{
    EventCandidate, EventProducer, FrontierEventAdmission, PullRequestLifecycle, PullRequestState,
    RepoWatchStore, RepositoryProjection, RepositoryState, StoreError, observation_decode,
};

/// One complete external observation to commit as a repository projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryObservation {
    pub repository: RepositorySlug,
    pub default_branch: BranchName,
    pub default_head: CommitSha,
    pub observation: RepoWatchObservation,
    pub observed_at: OffsetDateTime,
}

/// Durable comparison input loaded before one repository fetch.
#[derive(Clone, Debug)]
pub struct IngestBaseline {
    pub generation: u64,
    pub observation: Option<RepoWatchObservation>,
    pub frontier: RepoWatchEventIdentityFrontierV1,
}

impl RepoWatchStore {
    /// Loads the baseline and complete frontier from one consistent database snapshot.
    pub async fn ingest_baseline(
        &self,
        repository: &RepositorySlug,
    ) -> Result<IngestBaseline, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let row: Option<(Decimal, String)> = sqlx::query_as(
            "SELECT frontier_generation, comparison_baseline::text
               FROM repository_state WHERE repository = $1",
        )
        .bind(repository.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let entries: Vec<(Vec<u8>, Decimal, Option<Decimal>)> = sqlx::query_as(
            "SELECT stream_identity, sequence, pull_request_number
               FROM frontier WHERE repository = $1 ORDER BY stream_identity",
        )
        .bind(repository.as_str())
        .fetch_all(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let (generation, observation) = match row {
            Some((generation, observation)) => {
                let value: Value = serde_json::from_str(&observation)
                    .map_err(|_| StoreError::InvalidComparisonBaseline)?;
                (
                    generation
                        .to_u64()
                        .ok_or(StoreError::InvalidFrontierGeneration)?,
                    Some(
                        observation_decode::observation(&value)
                            .ok_or(StoreError::InvalidComparisonBaseline)?,
                    ),
                )
            }
            None => (0, None),
        };
        let frontier = entries
            .into_iter()
            .map(|(identity, sequence, number)| {
                let identity = identity
                    .try_into()
                    .map_err(|_| StoreError::InvalidComparisonBaseline)?;
                let sequence = sequence
                    .to_u64()
                    .and_then(std::num::NonZeroU64::new)
                    .ok_or(StoreError::InvalidComparisonBaseline)?;
                Ok(match number {
                    Some(number) => RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                        identity,
                        sequence,
                        signalbox_ownership_seam::PullRequestNumber::new(
                            number
                                .to_u64()
                                .and_then(std::num::NonZeroU64::new)
                                .ok_or(StoreError::InvalidComparisonBaseline)?,
                        ),
                    ),
                    None => RepoWatchEventIdentityFrontierEntryV1::new(identity, sequence),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(IngestBaseline {
            generation,
            observation,
            frontier: RepoWatchEventIdentityFrontierV1::try_from_entries(frontier)
                .map_err(|_| StoreError::InvalidComparisonBaseline)?,
        })
    }

    /// Derives and atomically admits an observation against its loaded predecessor.
    pub async fn ingest_observation(
        &self,
        baseline: &IngestBaseline,
        observed: &RepositoryObservation,
        producer: EventProducer,
    ) -> Result<FrontierEventAdmission, StoreError> {
        let mut frontier = baseline.frontier.clone();
        let occurrences = derive_repo_watch_events(
            &observed.repository,
            baseline.observation.as_ref(),
            &observed.observation,
            &mut frontier,
            &mut UuidV7RepoWatchEventIdGenerator,
        )
        .map_err(|_| StoreError::InvalidComparisonBaseline)?;
        let projection = RepositoryProjection {
            repository: RepositoryState {
                repository: &observed.repository,
                default_branch: &observed.default_branch,
                default_head: &observed.default_head,
                observed_at: observed.observed_at,
            },
            pull_requests: observed
                .observation
                .state()
                .pull_requests()
                .iter()
                .map(|state| {
                    let context = state.context();
                    PullRequestState {
                        repository: &observed.repository,
                        number: context.number(),
                        lifecycle: match state.lifecycle() {
                            RepoWatchPullRequestLifecycle::Open => PullRequestLifecycle::Open,
                            RepoWatchPullRequestLifecycle::Closed => PullRequestLifecycle::Closed,
                            RepoWatchPullRequestLifecycle::Merged => PullRequestLifecycle::Merged,
                        },
                        head: context.head_sha(),
                        head_repository: context.head_repository(),
                        head_branch: context.head_branch(),
                        base_branch: context.base_branch(),
                        title: context.title(),
                        body: context.body(),
                        draft: context.draft(),
                        author: context.author(),
                        observed_at: observed.observed_at,
                    }
                })
                .collect(),
            comparison_baseline: &observed.observation,
            merged_baselines: &[],
        };
        let events = occurrences
            .iter()
            .map(|occurrence| EventCandidate {
                event: occurrence.event(),
                content_identity: occurrence.content_identity(),
            })
            .collect::<Vec<_>>();
        self.commit_frontier_candidate(
            &projection,
            baseline.generation,
            &frontier.entries().collect::<Vec<_>>(),
            &events,
            producer,
            observed.observed_at,
        )
        .await
    }
}

/// One repository attempt, including external fetch and its durable commit.
pub trait RepositoryTask: Send {
    type Error;

    fn poll(
        &mut self,
        producer: EventProducer,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Runs start-to-start polls and coalesced webhook wakes without overlapping attempts.
pub async fn run_repository_task(
    mut task: impl RepositoryTask,
    interval: Duration,
    wake: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut next_start = Instant::now();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let producer = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            () = wake.notified() => EventProducer::Webhook,
            () = sleep_until(next_start) => EventProducer::Poll,
        };
        next_start = Instant::now() + interval;
        tokio::select! {
            _ = shutdown.changed() => return,
            result = task.poll(producer) => {
                if result.is_err() { tracing::warn!("repository-watch observation failed"); }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct Attempt {
        started: mpsc::UnboundedSender<(Instant, EventProducer)>,
        finish: mpsc::UnboundedReceiver<()>,
    }

    impl RepositoryTask for Attempt {
        type Error = ();

        async fn poll(&mut self, producer: EventProducer) -> Result<(), ()> {
            self.started
                .send((Instant::now(), producer))
                .expect("test receiver is alive");
            self.finish
                .recv()
                .await
                .expect("test finishes each attempt");
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn poll_interval_is_measured_from_start_and_wakes_wait_for_the_running_attempt() {
        // The attempt occupies half the arbitrary test interval.
        let interval = Duration::from_secs(10);
        let (started, mut starts) = mpsc::unbounded_channel();
        let (finish, finishes) = mpsc::unbounded_channel();
        let (shutdown, stopped) = watch::channel(false);
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run_repository_task(
            Attempt {
                started,
                finish: finishes,
            },
            interval,
            wake.clone(),
            stopped,
        ));
        let (first, producer) = starts.recv().await.expect("initial poll starts");
        assert_eq!(producer, EventProducer::Poll);
        tokio::time::advance(interval / 2).await;
        finish.send(()).expect("finish initial poll");
        let (second, producer) = starts.recv().await.expect("next poll starts");
        assert_eq!(second - first, interval);
        assert_eq!(producer, EventProducer::Poll);
        wake.notify_one();
        wake.notify_one();
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        finish.send(()).expect("finish second poll");
        let (_, producer) = starts.recv().await.expect("serialized wake starts");
        assert_eq!(producer, EventProducer::Webhook);
        shutdown.send(true).expect("stop repository task");
        task.await.expect("repository task exits cleanly");
    }
}
