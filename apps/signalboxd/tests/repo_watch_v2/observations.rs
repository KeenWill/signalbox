//! Native observation replay and module receipt recovery with recorded provider responses.
//! Exercises docs/spec/repo-watch.md and docs/spec/workflows.md.

use super::*;
#[path = "observations/production.rs"]
mod production;
use signalbox_domain::{
    InlineFramePayload, ProgramCapability, ProgramRegistrationId, ProgramRunId,
    program_registration::{NativeProgramRegistrationRequest, ProgramExecutable, ProgramGrants},
};
use signalbox_module_repo_watch_v2::{
    observation_workflow::{ObservationInvocation, ObservationOutcome, ObservationResult},
    poll_cache::poll_with_cache,
};
use signalbox_persistence::{
    program_journal::ProgramJournalRepository, program_registration::ProgramRegistrationRepository,
};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, WorkflowHost,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
    native::{NativeCatalog, NativeValue},
};
use signalboxd::workflows::repo_watch::observe::{
    self, OBSERVE_ENTRY, OBSERVE_REVISION, ObserveAnswer, ObserveInput, ObserveRepository,
    RepositoryObserver,
};
use std::{future::Future, num::NonZeroUsize, pin::Pin};

struct RecordedObserver {
    repository: RepositorySlug,
    io: ConditionalPollFixture,
    reviewers: Vec<RepoWatchAuthorLogin>,
    budget: NonZeroUsize,
}

impl RepositoryObserver for RecordedObserver {
    fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    async fn observe(
        &mut self,
        store: RepoWatchStore,
        producer: EventProducer,
    ) -> Result<ObservationOutcome, LiveDeliveryFailure> {
        match producer {
            EventProducer::Poll => poll_with_cache(
                &self.io,
                &store,
                &self.repository,
                &self.reviewers,
                MERGED_RETENTION,
                self.budget,
            )
            .await
            .map(|complete| {
                if complete {
                    ObservationOutcome::Succeeded
                } else {
                    ObservationOutcome::Partial
                }
            })
            .map_err(|error| LiveDeliveryFailure::new(error.to_string())),
            EventProducer::Webhook => {
                signalbox_module_repo_watch_v2::poll_cache::observe_webhook_pulls(
                    &self.io,
                    &store,
                    &self.repository,
                    &self.reviewers,
                    MERGED_RETENTION,
                )
                .await
                .map(|outcome| match outcome {
                    signalbox_module_repo_watch_v2::measurements::PollOutcome::Succeeded => {
                        ObservationOutcome::Succeeded
                    }
                    _ => ObservationOutcome::Partial,
                })
                .map_err(|error| LiveDeliveryFailure::new(error.to_string()))
            }
        }
    }
}

enum AnswerDelivery {
    Deliver,
    Lose,
}
struct Effects {
    store: RepoWatchStore,
    observer: RecordedObserver,
    delivery: AnswerDelivery,
}

impl EffectExecutor for Effects {
    fn recovery(&self, _: &signalbox_domain::EffectRequest) -> EffectRecovery {
        EffectRecovery::Ambiguous
    }
    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async move {
            observe::adopt(
                &self.store,
                &ObserveInput::from_request(invocation.request)
                    .expect("checked observation request"),
            )
            .await
        })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            let result = observe::execute(
                &self.store,
                &ObserveInput::from_request(invocation.request)
                    .expect("checked observation request"),
                &mut self.observer,
            )
            .await?;
            match std::mem::replace(&mut self.delivery, AnswerDelivery::Deliver) {
                AnswerDelivery::Deliver => Ok(result),
                AnswerDelivery::Lose => Err(LiveDeliveryFailure::new(
                    "fixture loses the committed observation answer",
                )),
            }
        })
    }
}

struct NoPrimitives;
impl LiveDeliverySource for NoPrimitives {
    fn next_delivery<'a>(
        &'a mut self,
        _: &'a [signalbox_domain::RequestFrame],
    ) -> Pin<
        Box<dyn Future<Output = Result<signalbox_domain::DeliveryKind, LiveDeliveryFailure>> + 'a>,
    > {
        Box::pin(async { Err(LiveDeliveryFailure::new("unexpected primitive")) })
    }
}

struct Fixture {
    _database: TestDatabase,
    core: PgPool,
    module: PgPool,
    effects: Effects,
    host: WorkflowHost,
    journal: ProgramJournalRepository,
    registrations: ProgramRegistrationRepository,
    registration: ProgramRegistrationId,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let (database, core, url) = postgres().await?;
        sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
            .execute(&core)
            .await?;
        let module = module_pool(&url).await?;
        let store = RepoWatchStore::new(module.clone());
        let repository = RepositorySlug::try_new("example/project".into())?;
        let reviewers = vec![RepoWatchAuthorLogin::try_new("reviewer".into())?];
        store.prepare_poll_cache(&repository, &reviewers).await?;
        let mut catalog = NativeCatalog::new()?;
        catalog.insert::<ObserveRepository>(OBSERVE_ENTRY.into(), OBSERVE_REVISION.into())?;
        let ProgramExecutable::Native { binary_digest, .. } = catalog
            .executable(OBSERVE_ENTRY, OBSERVE_REVISION)
            .expect("native fixture entry")
        else {
            panic!("native entry");
        };
        let registrations = ProgramRegistrationRepository::new(core.clone());
        let id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
        registrations
            .register_native_user(
                id,
                NativeProgramRegistrationRequest {
                    name: id.into_uuid().to_string(),
                    revision: "fixture".into(),
                    entry: OBSERVE_ENTRY.into(),
                    native_revision: OBSERVE_REVISION.into(),
                    binary_digest,
                    grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
                },
            )
            .await?;
        let journal = ProgramJournalRepository::new(core.clone());
        let host = WorkflowHost::new(journal.clone()).with_native_catalog(catalog);
        Ok(Self {
            _database: database,
            core,
            module,
            host,
            journal,
            registrations,
            registration: id,
            effects: Effects {
                store,
                observer: RecordedObserver {
                    repository,
                    reviewers,
                    io: ConditionalPollFixture::new(),
                    budget: NonZeroUsize::new(1000).expect("maximum configured request allowance"),
                },
                delivery: AnswerDelivery::Deliver,
            },
        })
    }
    fn input(&self) -> ObserveInput {
        ObserveInput::new(
            self.effects.observer.repository.clone(),
            EventProducer::Poll,
        )
        .expect("poll input")
    }
    async fn start(&self, input: &ObserveInput) -> Result<ProgramRunId, Box<dyn Error>> {
        Ok(self
            .registrations
            .start_run(
                ProgramRunId::from_uuid(Uuid::now_v7()),
                self.registration,
                &input.encode()?,
            )
            .await?)
    }
    async fn run(&mut self, input: &ObserveInput) -> Result<ObserveAnswer, Box<dyn Error>> {
        let run = self.start(input).await?;
        self.host
            .execute_registered(run, &mut NoPrimitives, &mut self.effects)
            .await?;
        let journal = self.journal.load(run).await?.expect("retained run");
        let answer = ObserveAnswer::decode(
            journal
                .result()
                .expect("successful native result")
                .as_bytes(),
        )?;
        observe::acknowledge(&self.effects.store, &self.journal).await?;
        Ok(answer)
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn complete_observation_recovers_a_lost_answer_without_another_provider_read()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let input = fixture.input();
    let run = fixture.start(&input).await?;
    fixture.effects.delivery = AnswerDelivery::Lose;
    assert!(
        fixture
            .host
            .execute_registered(run, &mut NoPrimitives, &mut fixture.effects)
            .await
            .is_err()
    );
    let receipt = fixture
        .effects
        .store
        .observation_receipt(input.repository())
        .await?
        .expect("frontier receipt survives lost answer");
    let result = ObservationResult::decode(&receipt.result).expect("checked range");
    assert_eq!(result.outcome, ObservationOutcome::Succeeded);
    assert_eq!(result.after, 0);
    assert!(
        result.through > result.after,
        "completed observation accepts initial facts"
    );
    assert_eq!(
        result.generation,
        fixture
            .effects
            .store
            .ingest_baseline(input.repository())
            .await?
            .generation
    );
    let reads = fixture
        .effects
        .observer
        .io
        .requests
        .lock()
        .expect("request log")
        .len();
    fixture
        .host
        .execute_registered(run, &mut NoPrimitives, &mut fixture.effects)
        .await?;
    assert_eq!(
        fixture
            .effects
            .observer
            .io
            .requests
            .lock()
            .expect("request log")
            .len(),
        reads,
        "receipt adoption must not perform a second observation"
    );
    let journal = fixture.journal.load(run).await?.expect("run");
    assert_eq!(
        journal.result().expect("native result").as_bytes(),
        receipt.result
    );
    observe::acknowledge(&fixture.effects.store, &fixture.journal).await?;
    assert!(
        fixture
            .effects
            .store
            .observation_receipt(input.repository())
            .await?
            .is_none()
    );
    let next = fixture.input();
    assert!(matches!(
        fixture.run(&next).await?,
        ObserveAnswer::Observed(ObservationResult {
            outcome: ObservationOutcome::Succeeded,
            ..
        })
    ));
    let events: Decimal = sqlx::query_scalar("SELECT max(repository_event_ordinal) FROM gh_event")
        .fetch_one(&fixture.module)
        .await?;
    assert_eq!(
        events,
        Decimal::from(result.through),
        "unchanged provider facts create no duplicate events"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_successor_adopts_a_retired_runs_observation_before_provider_configuration()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let input = fixture.input();
    let run = fixture.start(&input).await?;
    fixture.effects.delivery = AnswerDelivery::Lose;
    assert!(
        fixture
            .host
            .execute_registered(run, &mut NoPrimitives, &mut fixture.effects)
            .await
            .is_err()
    );
    WorkflowHost::new(fixture.journal.clone())
        .execute_registered(run, &mut NoPrimitives, &mut fixture.effects)
        .await?;
    let receipt = fixture
        .effects
        .store
        .observation_receipt(input.repository())
        .await?
        .expect("retired run receipt");
    fixture.effects.observer.io.pages.clear();
    fixture
        .effects
        .observer
        .io
        .requests
        .lock()
        .expect("request log")
        .clear();
    let result = fixture.run(&input).await?;
    assert_eq!(result.encode()?, receipt.result);
    assert!(
        fixture
            .effects
            .observer
            .io
            .requests
            .lock()
            .expect("request log")
            .is_empty()
    );
    assert!(
        fixture
            .effects
            .store
            .observation_receipt(input.repository())
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_failed_subject_retains_only_completed_stage_observations() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    fixture
        .effects
        .observer
        .io
        .pages
        .remove("/repos/example/project/pulls/1");
    let input = fixture.input();
    let answer = fixture.run(&input).await?;
    assert!(matches!(
        answer,
        ObserveAnswer::Observed(ObservationResult {
            outcome: ObservationOutcome::Failed,
            through: 0,
            ..
        })
    ));
    let baseline = fixture
        .effects
        .store
        .ingest_baseline(input.repository())
        .await?;
    assert!(
        baseline.generation > 0,
        "repository discovery remains committed"
    );
    assert!(
        baseline
            .observation
            .expect("discovery baseline")
            .state()
            .pull_requests()
            .is_empty(),
        "failed pull-request data is not admitted"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_observations_reuse_conditional_pages_and_invalidate_changed_reviewers()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.input();
    fixture.run(&first).await?;
    fixture
        .effects
        .observer
        .io
        .requests
        .lock()
        .expect("request log")
        .clear();
    let second = fixture.input();
    fixture.run(&second).await?;
    assert!(
        fixture
            .effects
            .observer
            .io
            .requests
            .lock()
            .expect("request log")
            .iter()
            .filter(|request| request.path != "/rate_limit")
            .all(|request| request.etag.as_deref() == Some("\"fixture-etag\""))
    );
    fixture.effects.observer.reviewers = vec![RepoWatchAuthorLogin::try_new("outsider".into())?];
    fixture
        .effects
        .store
        .prepare_poll_cache(first.repository(), &fixture.effects.observer.reviewers)
        .await?;
    fixture
        .effects
        .observer
        .io
        .requests
        .lock()
        .expect("request log")
        .clear();
    let third = fixture.input();
    fixture.run(&third).await?;
    assert!(
        fixture
            .effects
            .observer
            .io
            .requests
            .lock()
            .expect("request log")
            .iter()
            .all(|request| request.etag.is_none())
    );
    let actors: Vec<String> = sqlx::query_scalar("SELECT DISTINCT item->>'reviewer' FROM poll_cache_page CROSS JOIN LATERAL jsonb_array_elements(snapshot->1) item WHERE resource_key LIKE '%/reactions?%'").fetch_all(&fixture.module).await?;
    assert_eq!(actors, vec!["outsider"]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_observations_keep_the_configured_request_ceiling() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    fixture.effects.observer.budget =
        NonZeroUsize::new(1).expect("one request allows only preflight");
    let input = fixture.input();
    assert_eq!(
        fixture.run(&input).await?,
        ObserveAnswer::Observed(ObservationResult {
            generation: 0,
            after: 0,
            through: 0,
            outcome: ObservationOutcome::Partial
        })
    );
    assert_eq!(
        fixture
            .effects
            .observer
            .io
            .requests
            .lock()
            .expect("request log")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .effects
            .store
            .ingest_baseline(input.repository())
            .await?
            .generation,
        0
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn unchanged_frontier_commits_retain_the_invocation_until_its_answer_is_adopted()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let input = fixture.input();
    let observed = dispatch_observation(input.repository(), 1, OffsetDateTime::now_utc());
    fixture
        .effects
        .store
        .ingest_observation(
            &fixture
                .effects
                .store
                .ingest_baseline(input.repository())
                .await?,
            &observed,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let baseline = fixture
        .effects
        .store
        .ingest_baseline(input.repository())
        .await?;
    let bound = fixture
        .effects
        .store
        .with_observation_invocation(ObservationInvocation {
            effect: input.effect(),
            input: input.encode()?,
            repository: input.repository().clone(),
        });
    assert_eq!(
        bound
            .ingest_observation(&baseline, &observed, EventProducer::Poll, MERGED_RETENTION)
            .await?,
        FrontierEventAdmission::Unchanged
    );
    let receipt = bound
        .observation_receipt(input.repository())
        .await?
        .expect("unchanged observation receipt");
    assert_eq!(receipt.effect, input.effect());
    let result = ObservationResult::decode(&receipt.result).expect("receipt range");
    assert_eq!(result.generation, baseline.generation);
    assert_eq!(result.after, result.through);
    let next = fixture.input();
    assert!(
        observe::adopt(&fixture.effects.store, &next).await.is_err(),
        "another observation must wait for receipt adoption"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn observation_leases_leave_connections_available_for_frontier_work()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let store = &fixture.effects.store;
    let other = RepositorySlug::try_new("example/other".into())?;
    let first = store.lock_observation(fixture.input().repository()).await?;
    let second = store.lock_observation(&other).await?;
    // Both module pool slots would be occupied if leases borrowed frontier connections.
    let baseline =
        tokio::time::timeout(Duration::from_secs(5), store.ingest_baseline(&other)).await??;
    assert_eq!(baseline.generation, 0);
    first.rollback().await?;
    second.rollback().await?;
    Ok(())
}
