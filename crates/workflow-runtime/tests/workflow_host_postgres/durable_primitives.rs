use super::*;
use signalbox_domain::program_primitives::{
    AwaitProgramEvent, ProgramEvent, ProgramEventSource, RandomValue, SleepUntil, UnixMillis,
};
use signalbox_domain::{ProgramCapability, program_registration::ProgramGrants};
use signalbox_workflow_runtime::primitives::{DurablePrimitives, PrimitiveClock};
use std::{cell::Cell, rc::Rc};

struct FixedClock {
    now: UnixMillis,
    draws: Rc<Cell<usize>>,
}

impl PrimitiveClock for FixedClock {
    fn now(&mut self) -> Result<UnixMillis, LiveDeliveryFailure> {
        Ok(self.now)
    }
    fn random(&mut self) -> Result<RandomValue, LiveDeliveryFailure> {
        self.draws.set(self.draws.get() + 1);
        Ok(RandomValue(u64::MAX))
    }
}

fn no_effects() -> EffectProbe {
    EffectProbe {
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn partial_replay_reuses_clock_and_random_answers_without_drawing_again()
-> Result<(), Box<dyn Error>> {
    let (_database, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let artifact = ProgramArtifact::new(
        r#"
import { defineProgram, jsonCodec, primitives } from "@signalbox/program-sdk/v1";
const string = jsonCodec((value) => {
  if (typeof value !== "string") throw new TypeError("expected a string");
  return value;
});
export default defineProgram({ input: string, output: string, async run(deadline) {
const time = await primitives.now();
const random = await primitives.random();
if (time.kind !== "answer" || time.value !== "9007199254740993") throw new Error("clock precision");
if (random.kind !== "answer" || random.value !== "18446744073709551615") throw new Error("random precision");
await primitives.sleepUntil(deadline);
return deadline;
}});
"#,
    );
    let input = br#""9007199254740994""#;
    let run = registration_with_input(
        &pool,
        ProgramGrants::new([
            ProgramCapability::Time,
            ProgramCapability::Random,
            ProgramCapability::Sleep,
        ]),
        artifact.source(),
        input,
    )
    .await?;
    let draws = Rc::new(Cell::new(0));
    let clock = FixedClock {
        now: UnixMillis(9_007_199_254_740_993),
        draws: draws.clone(),
    };
    let mut primitives = DurablePrimitives::new(journal.clone(), clock);
    let host = WorkflowHost::new(journal.clone());
    let mut effects = no_effects();
    // Interrupt only after the random answer is durable and the sleep is admitted.
    let execute = host.execute_registered(run, &mut primitives, &mut effects);
    let observe = async {
        loop {
            if !journal.outstanding_waits(run).await?.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, Box<dyn Error>>(())
    };
    tokio::select! { result = execute => panic!("attempt should wait: {result:?}"), result = observe => result? }
    assert_eq!(draws.get(), 1);
    let restarted = WorkflowHost::new(ProgramJournalRepository::new(pool.clone()));
    let clock = FixedClock {
        now: UnixMillis(9_007_199_254_740_994),
        draws: draws.clone(),
    };
    let mut resumed = DurablePrimitives::new(journal.clone(), clock);
    assert_eq!(
        restarted
            .execute_registered(run, &mut resumed, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(payload(input))
    );
    assert_eq!(
        draws.get(),
        1,
        "partial replay must consume no new randomness"
    );
    assert!(journal.outstanding_waits(run).await?.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn admitted_deadline_fires_after_restarting_the_host() -> Result<(), Box<dyn Error>> {
    let (_database, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let artifact = ProgramArtifact::new(
        r#"
import { defineProgram, jsonCodec, primitives } from "@signalbox/program-sdk/v1";
const string = jsonCodec((value) => {
  if (typeof value !== "string") throw new TypeError("expected a string");
  return value;
});
export default defineProgram({ input: string, output: string, async run(deadline) {
const wake = await primitives.sleepUntil(deadline);
if (wake.kind !== "wake" || wake.value !== "2000") throw new Error("deadline moved");
return wake.value;
}});
"#,
    );
    let input = br#""2000""#;
    let run = registration_with_input(
        &pool,
        ProgramGrants::new([ProgramCapability::Sleep]),
        artifact.source(),
        input,
    )
    .await?;
    let deadline = SleepUntil(UnixMillis(2000));
    let admitted = journal
        .append_request(run, None, RequestKind::Sleep(deadline.encode()))
        .await?;
    drop(journal);
    let restarted = ProgramJournalRepository::new(pool.clone());
    assert_eq!(restarted.outstanding_waits(run).await?, vec![admitted]);
    let mut primitives = DurablePrimitives::new(
        restarted.clone(),
        FixedClock {
            now: UnixMillis(3000),
            draws: Rc::default(),
        },
    );
    let outcome = WorkflowHost::new(restarted.clone())
        .execute_registered(run, &mut primitives, &mut no_effects())
        .await?;
    assert_eq!(outcome, ProgramExecutionOutcome::Completed(payload(input)));
    assert!(restarted.outstanding_waits(run).await?.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_wait_replays_its_source_position_and_retained_answer() -> Result<(), Box<dyn Error>>
{
    let (_database, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let source_run = run_id();
    journal.create_stream(source_run).await?;
    let source_request = journal
        .append_request(
            source_run,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    journal
        .append_delivery(
            source_run,
            DeliveryKind::Answer {
                resolves: source_request.ordinal(),
                payload: payload(b"first"),
            },
        )
        .await?;
    let next_request = journal
        .append_request(
            source_run,
            None,
            RequestKind::Random(InlineFramePayload::default()),
        )
        .await?;
    journal
        .append_delivery(
            source_run,
            DeliveryKind::Answer {
                resolves: next_request.ordinal(),
                payload: payload(b"next"),
            },
        )
        .await?;
    let wait = AwaitProgramEvent {
        source: ProgramEventSource::ProgramAnswers(source_run),
        after: 2,
    };
    let artifact = ProgramArtifact::new(
        r#"
import { defineProgram, jsonCodec, primitives } from "@signalbox/program-sdk/v1";
const string = jsonCodec((value) => {
  if (typeof value !== "string") throw new TypeError("expected a string");
  return value;
});
export default defineProgram({ input: string, output: string, async run(source) {
const event = await primitives.awaitEvent({ source: { kind: "program_answers", run: source }, after: "2" });
if (event.kind !== "answer" || event.value.position !== "4" || event.value.payload[0] !== 110) throw new Error("wrong event");
return source;
}});
"#,
    );
    let input = deno_core::serde_json::to_vec(&source_run.into_uuid().to_string())?;
    let run = registration_with_input(
        &pool,
        ProgramGrants::new([ProgramCapability::Subscribe]),
        artifact.source(),
        &input,
    )
    .await?;
    journal
        .append_request(run, None, RequestKind::AwaitEvent(wait.encode()))
        .await?;
    let mut primitives = DurablePrimitives::new(
        journal.clone(),
        FixedClock {
            now: UnixMillis(0),
            draws: Rc::default(),
        },
    );
    assert_eq!(
        WorkflowHost::new(journal.clone())
            .execute_registered(run, &mut primitives, &mut no_effects())
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(input))
    );
    assert_eq!(
        journal.next_event(wait).await?,
        Some(ProgramEvent {
            position: 4,
            payload: payload(b"next")
        })
    );
    pool.close().await;
    Ok(())
}

/// Commits the source answer after the initial empty read but before LISTEN is established.
struct CommitDuringSubscribe {
    journal: ProgramJournalRepository,
    source: ProgramRunId,
    resolves: RequestOrdinal,
}

impl signalbox_workflow_runtime::primitives::PrimitiveEvents for CommitDuringSubscribe {
    type Wake = signalbox_persistence::program_journal::ProgramJournalWake;

    async fn next_event(
        &mut self,
        wait: AwaitProgramEvent,
    ) -> Result<Option<ProgramEvent>, LiveDeliveryFailure> {
        self.journal
            .next_event(wait)
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))
    }

    async fn listen(&mut self, runs: &[ProgramRunId]) -> Result<Self::Wake, LiveDeliveryFailure> {
        self.journal
            .append_delivery(
                self.source,
                DeliveryKind::Answer {
                    resolves: self.resolves,
                    payload: payload(b"racing event"),
                },
            )
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))?;
        self.journal
            .listen(runs)
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_committed_between_empty_read_and_subscribe_is_delivered_without_a_notification()
-> Result<(), Box<dyn Error>> {
    const CATCH_UP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let (_database, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let source = run_id();
    journal.create_stream(source).await?;
    let source_request = journal
        .append_request(
            source,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    let artifact = ProgramArtifact::new(
        r#"
import { defineProgram, jsonCodec, primitives } from "@signalbox/program-sdk/v1";
const string = jsonCodec((value) => {
  if (typeof value !== "string") throw new TypeError("expected a string");
  return value;
});
export default defineProgram({ input: string, output: string, async run(source) {
const event = await primitives.awaitEvent({ source: { kind: "program_answers", run: source }, after: "0" });
if (event.kind !== "answer" || event.value.position !== "2" || event.value.payload[0] !== 114) throw new Error("lost racing event");
return source;
}});
"#,
    );
    let input = deno_core::serde_json::to_vec(&source.into_uuid().to_string())?;
    let run = registration_with_input(
        &pool,
        ProgramGrants::new([ProgramCapability::Subscribe]),
        artifact.source(),
        &input,
    )
    .await?;
    let events = CommitDuringSubscribe {
        journal: journal.clone(),
        source,
        resolves: source_request.ordinal(),
    };
    let mut primitives = DurablePrimitives::new(
        events,
        FixedClock {
            now: UnixMillis(0),
            draws: Rc::default(),
        },
    );
    let outcome = tokio::time::timeout(
        CATCH_UP_TIMEOUT,
        WorkflowHost::new(journal).execute_registered(run, &mut primitives, &mut no_effects()),
    )
    .await??;
    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(input))
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn journal_wake_observes_only_answers_from_watched_runs() -> Result<(), Box<dyn Error>> {
    const NOTIFICATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const QUIET_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);
    let (_database, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let source = run_id();
    journal.create_stream(source).await?;
    let mut listener = journal.listen(&[source]).await?;
    let request = journal
        .append_request(
            source,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    let wait = AwaitProgramEvent {
        source: ProgramEventSource::ProgramAnswers(source),
        after: 0,
    };
    assert_eq!(journal.next_event(wait).await?, None);
    let unrelated = distinct_run_id(1);
    journal.create_stream(unrelated).await?;
    let unrelated_request = journal
        .append_request(
            unrelated,
            None,
            RequestKind::Now(InlineFramePayload::default()),
        )
        .await?;
    journal
        .append_delivery(
            unrelated,
            DeliveryKind::Answer {
                resolves: unrelated_request.ordinal(),
                payload: payload(b"unrelated answer"),
            },
        )
        .await?;
    assert!(
        tokio::time::timeout(QUIET_PERIOD, listener.changed())
            .await
            .is_err()
    );
    journal
        .append_delivery(
            source,
            DeliveryKind::Answer {
                resolves: request.ordinal(),
                payload: payload(b"committed"),
            },
        )
        .await?;
    tokio::time::timeout(NOTIFICATION_TIMEOUT, listener.changed()).await??;
    assert_eq!(
        journal.next_event(wait).await?,
        Some(ProgramEvent {
            position: 2,
            payload: payload(b"committed")
        })
    );
    drop(listener);
    pool.close().await;
    Ok(())
}

#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn journal_listener_leaves_the_single_query_connection_available()
-> Result<(), Box<dyn Error>> {
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ImageExt, runners::AsyncRunner},
    };
    const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let container = Postgres::default()
        .with_cmd(signalbox_persistence::disposable_postgres_server_args())
        .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(postgres_test_image::POSTGRES_IMAGE_TAG)
        .with_labels(signalbox_persistence::disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    // The image's default disposable user and database are both postgres.
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(signalbox_persistence::local_test_connection_options(&url)?)
        .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let listener = journal.listen(&[run_id()]).await?;
    tokio::time::timeout(QUERY_TIMEOUT, sqlx::query("SELECT 1").execute(&pool)).await??;
    drop(listener);
    pool.close().await;
    Ok(())
}
