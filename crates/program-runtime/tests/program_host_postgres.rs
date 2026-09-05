//! PostgreSQL integration coverage for the JavaScript program host.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::VecDeque, error::Error, future::Future, pin::Pin};

use signalbox_domain::{
    DeliveryKind, InlineFramePayload, JournalFrame, ProgramFault, ProgramRunId, ReplayCursor,
    RequestFrame, RequestKind, RequestOrdinal,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    program_journal::ProgramJournalRepository,
};
use signalbox_program_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, PROGRAM_SDK_V1_SPECIFIER, ProgramArtifact,
    ProgramExecutionOutcome, ProgramHost, ProgramHostError, ProgramHostProtocolError,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_program_host";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const RUN_ID: u128 = 0x5100_0200;
const REPLAY_REQUEST_BYTE: u8 = 1;
const REPLAY_ANSWER_BYTE: u8 = 11;
const FIRST_LIVE_REQUEST_BYTE: u8 = 2;
const FIRST_LIVE_ANSWER_BYTE: u8 = 22;
const SECOND_LIVE_REQUEST_BYTE: u8 = 3;
const SECOND_LIVE_ANSWER_BYTE: u8 = 33;
const DIVERGENT_REQUEST_BYTE: u8 = 9;
const RUN_CANCEL_BYTE: u8 = 44;
const THROWN_MESSAGE: &str = "the artifact threw";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn run_id() -> ProgramRunId {
    ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID))
}

fn distinct_run_id(offset: u128) -> ProgramRunId {
    ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID + offset))
}

fn payload(bytes: &'static [u8]) -> InlineFramePayload {
    InlineFramePayload::new(bytes)
}

fn request(ordinal: u64, kind: RequestKind) -> RequestFrame {
    RequestFrame::new(
        RequestOrdinal::try_from_u64(ordinal).expect("fixture request ordinal is positive"),
        None,
        kind,
    )
}

struct ScriptedDeliveries {
    deliveries: VecDeque<DeliveryKind>,
    observed_outstanding: Vec<Vec<RequestFrame>>,
}

impl ScriptedDeliveries {
    fn new(deliveries: impl IntoIterator<Item = DeliveryKind>) -> Self {
        Self {
            deliveries: deliveries.into_iter().collect(),
            observed_outstanding: Vec::new(),
        }
    }
}

impl LiveDeliverySource for ScriptedDeliveries {
    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        self.observed_outstanding.push(outstanding.to_vec());
        let delivery = self.deliveries.pop_front();
        Box::pin(async move {
            delivery.ok_or_else(|| LiveDeliveryFailure::new("scripted deliveries exhausted"))
        })
    }
}

fn tail_transition_artifact() -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ now, random }} from "{PROGRAM_SDK_V1_SPECIFIER}";
if (typeof Date !== "undefined" || typeof Math.random !== "undefined" || typeof Deno !== "undefined") {{
  throw new Error("ambient nondeterminism or engine ops reached the artifact");
}}
const first = await now(new Uint8Array([{REPLAY_REQUEST_BYTE}]));
if (first.kind !== "answer" || first.payload[0] !== {REPLAY_ANSWER_BYTE}) {{
  throw new Error("unexpected replayed answer");
}}
const [second, third] = await Promise.all([
  random(new Uint8Array([{FIRST_LIVE_REQUEST_BYTE}])),
  now(new Uint8Array([{SECOND_LIVE_REQUEST_BYTE}])),
]);
if (second.kind !== "answer" || second.payload[0] !== {FIRST_LIVE_ANSWER_BYTE}) {{
  throw new Error("unexpected first live answer");
}}
if (third.kind !== "answer" || third.payload[0] !== {SECOND_LIVE_ANSWER_BYTE}) {{
  throw new Error("unexpected second live answer");
}}
"#
    ))
}

fn immediately_requesting_artifact() -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ now }} from "{PROGRAM_SDK_V1_SPECIFIER}";
await now(new Uint8Array([{FIRST_LIVE_REQUEST_BYTE}]));
"#
    ))
}

fn two_request_artifact() -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ now }} from "{PROGRAM_SDK_V1_SPECIFIER}";
await now(new Uint8Array([{REPLAY_REQUEST_BYTE}]));
await now(new Uint8Array([{SECOND_LIVE_REQUEST_BYTE}]));
"#
    ))
}

fn divergent_artifact() -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ now }} from "{PROGRAM_SDK_V1_SPECIFIER}";
await now(new Uint8Array([{DIVERGENT_REQUEST_BYTE}]));
"#
    ))
}

/// a real isolate consumes recorded deliveries and appends only after the durable tail.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn isolate_replays_then_transitions_to_live_at_the_durable_tail() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let recorded_request = repository
        .append_request(run, None, RequestKind::Now(payload(&[REPLAY_REQUEST_BYTE])))
        .await?;
    let recorded_delivery = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: recorded_request.ordinal(),
                payload: payload(&[REPLAY_ANSWER_BYTE]),
            },
        )
        .await?;
    let expected_live_request =
        request(2, RequestKind::Random(payload(&[FIRST_LIVE_REQUEST_BYTE])));
    let expected_live_kind = DeliveryKind::Answer {
        resolves: expected_live_request.ordinal(),
        payload: payload(&[FIRST_LIVE_ANSWER_BYTE]),
    };
    let expected_concurrent_request =
        request(3, RequestKind::Now(payload(&[SECOND_LIVE_REQUEST_BYTE])));
    let expected_concurrent_kind = DeliveryKind::Answer {
        resolves: expected_concurrent_request.ordinal(),
        payload: payload(&[SECOND_LIVE_ANSWER_BYTE]),
    };
    let artifact = tail_transition_artifact();
    let host = ProgramHost::new(repository.clone());
    let mut live =
        ScriptedDeliveries::new([expected_concurrent_kind.clone(), expected_live_kind.clone()]);

    let first_outcome = host
        .execute(run, &artifact, &mut live)
        .await
        .expect("partial-journal execution must reach live and complete");

    assert_eq!(first_outcome, ProgramExecutionOutcome::Completed);
    assert_eq!(
        live.observed_outstanding,
        vec![
            vec![
                expected_live_request.clone(),
                expected_concurrent_request.clone()
            ],
            vec![expected_live_request.clone()]
        ]
    );
    let after_live = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(after_live.entries().len(), 6);
    assert_eq!(
        after_live.entries()[0].frame(),
        &JournalFrame::Request(recorded_request)
    );
    assert_eq!(
        after_live.entries()[1].frame(),
        &JournalFrame::Delivery(recorded_delivery)
    );
    assert_eq!(
        after_live.entries()[2].frame(),
        &JournalFrame::Request(expected_live_request)
    );
    assert_eq!(
        after_live.entries()[3].frame(),
        &JournalFrame::Request(expected_concurrent_request)
    );
    let first_appended_delivery = after_live.entries()[4].frame().clone();
    let JournalFrame::Delivery(first_appended_delivery) = first_appended_delivery else {
        panic!("the fifth frame must be the first live delivery");
    };
    assert_eq!(first_appended_delivery.kind(), &expected_concurrent_kind);
    let second_appended_delivery = after_live.entries()[5].frame().clone();
    let JournalFrame::Delivery(second_appended_delivery) = second_appended_delivery else {
        panic!("the sixth frame must be the second live delivery");
    };
    assert_eq!(second_appended_delivery.kind(), &expected_live_kind);
    let mut replay_must_not_go_live = ScriptedDeliveries::new([]);

    let replay_outcome = host
        .execute(run, &artifact, &mut replay_must_not_go_live)
        .await
        .expect("complete-journal replay must complete");

    assert_eq!(replay_outcome, ProgramExecutionOutcome::Completed);
    assert!(replay_must_not_go_live.observed_outstanding.is_empty());
    let after_replay = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(after_replay, after_live);

    pool.close().await;
    drop(container);
    Ok(())
}

/// isolate divergence is typed, persisted once, and replays as the same fault.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn isolate_divergence_persists_and_replays_the_nondeterminism_fault()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let expected = repository
        .append_request(run, None, RequestKind::Now(payload(&[REPLAY_REQUEST_BYTE])))
        .await?;
    let observed = request(1, RequestKind::Now(payload(&[DIVERGENT_REQUEST_BYTE])));
    let artifact = divergent_artifact();
    let host = ProgramHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let failure = host
        .execute(run, &artifact, &mut live_must_not_run)
        .await
        .expect_err("different request bytes must stop the isolate host");
    let ProgramHostError::Nondeterminism {
        expected: failed_expected,
        observed: failed_observed,
        fault,
    } = failure
    else {
        panic!("expected the typed nondeterminism failure, got {failure:?}");
    };

    assert_eq!(*failed_expected, expected.clone());
    assert_eq!(*failed_observed, observed.clone());
    assert!(live_must_not_run.observed_outstanding.is_empty());
    let persisted = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(
        persisted.entries()[1].frame(),
        &JournalFrame::Delivery(fault)
    );
    let mut restarted_live_must_not_run = ScriptedDeliveries::new([]);

    let restarted = host
        .execute(run, &artifact, &mut restarted_live_must_not_run)
        .await?;

    assert_eq!(
        restarted,
        ProgramExecutionOutcome::Faulted(ProgramFault::Nondeterminism { expected, observed })
    );
    assert!(restarted_live_must_not_run.observed_outstanding.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn isolate_closes_intl_and_the_raw_request_op_before_artifact_evaluation()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(1);
    repository.create_stream(run).await?;
    let artifact = ProgramArtifact::new(
        r#"
globalThis.Intl === undefined || (() => { throw new Error("Intl reached the artifact"); })();
globalThis.WeakRef === undefined || (() => { throw new Error("WeakRef reached the artifact"); })();
globalThis.FinalizationRegistry === undefined || (() => { throw new Error("FinalizationRegistry reached the artifact"); })();
globalThis.__signalboxProgramRequest === undefined || (() => { throw new Error("the raw request op reached the artifact"); })();
"#,
    );
    let host = ProgramHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(outcome, ProgramExecutionOutcome::Completed);
    assert!(live_must_not_run.observed_outstanding.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unresolved_top_level_await_returns_stalled_promptly() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(2);
    repository.create_stream(run).await?;
    let artifact = ProgramArtifact::new("await new Promise(() => {});");
    let host = ProgramHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let failure = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        host.execute(run, &artifact, &mut live_must_not_run),
    )
    .await
    .expect("a stalled artifact must return promptly")
    .expect_err("an unresolved top-level await must stall");

    assert!(
        matches!(
            failure,
            ProgramHostError::Protocol(ProgramHostProtocolError::Stalled)
        ),
        "expected the typed stalled failure, got {failure:?}"
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn completed_module_drains_an_unawaited_request_without_repolling_evaluation()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(3);
    repository.create_stream(run).await?;
    let artifact = ProgramArtifact::new(format!(
        r#"
import {{ now }} from "{PROGRAM_SDK_V1_SPECIFIER}";
now(new Uint8Array([{FIRST_LIVE_REQUEST_BYTE}]));
"#
    ));
    let expected_request = request(1, RequestKind::Now(payload(&[FIRST_LIVE_REQUEST_BYTE])));
    let mut live = ScriptedDeliveries::new([DeliveryKind::Answer {
        resolves: expected_request.ordinal(),
        payload: payload(&[FIRST_LIVE_ANSWER_BYTE]),
    }]);
    let host = ProgramHost::new(repository.clone());

    let outcome = host.execute(run, &artifact, &mut live).await?;

    assert_eq!(outcome, ProgramExecutionOutcome::Completed);
    assert_eq!(live.observed_outstanding, vec![vec![expected_request]]);
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 2);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_module_that_throws_is_an_isolate_failure_not_a_completion() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(8);
    repository.create_stream(run).await?;
    let artifact = ProgramArtifact::new(format!(r#"throw new Error("{THROWN_MESSAGE}");"#));
    let host = ProgramHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let failure = host
        .execute(run, &artifact, &mut live_must_not_run)
        .await
        .expect_err("a module that throws must not report completion");

    let ProgramHostError::Isolate(error) = failure else {
        panic!("expected the typed isolate failure, got {failure:?}");
    };
    assert!(
        error.to_string().contains(THROWN_MESSAGE),
        "the isolate failure must carry the artifact's own message, got {error}"
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn isolate_closes_shared_memory_and_locale_sensitive_methods() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(5);
    repository.create_stream(run).await?;
    let artifact = ProgramArtifact::new(
        r#"
globalThis.SharedArrayBuffer === undefined || (() => { throw new Error("SharedArrayBuffer reached the artifact"); })();
globalThis.Atomics === undefined || (() => { throw new Error("Atomics reached the artifact"); })();
globalThis.Temporal === undefined || (() => { throw new Error("Temporal reached the artifact"); })();
globalThis.WebAssembly === undefined || (() => { throw new Error("WebAssembly reached the artifact"); })();
Object.prototype.toLocaleString === undefined || (() => { throw new Error("Object.prototype.toLocaleString reached the artifact"); })();
Number.prototype.toLocaleString === undefined || (() => { throw new Error("Number.prototype.toLocaleString reached the artifact"); })();
BigInt.prototype.toLocaleString === undefined || (() => { throw new Error("BigInt.prototype.toLocaleString reached the artifact"); })();
Array.prototype.toLocaleString === undefined || (() => { throw new Error("Array.prototype.toLocaleString reached the artifact"); })();
Object.getPrototypeOf(Int8Array.prototype).toLocaleString === undefined || (() => { throw new Error("TypedArray.prototype.toLocaleString reached the artifact"); })();
String.prototype.localeCompare === undefined || (() => { throw new Error("String.prototype.localeCompare reached the artifact"); })();
String.prototype.toLocaleLowerCase === undefined || (() => { throw new Error("String.prototype.toLocaleLowerCase reached the artifact"); })();
String.prototype.toLocaleUpperCase === undefined || (() => { throw new Error("String.prototype.toLocaleUpperCase reached the artifact"); })();
typeof typedArrayPrototype === "undefined" || (() => { throw new Error("a bootstrap binding reached the artifact"); })();
typeof localeSensitiveMethods === "undefined" || (() => { throw new Error("a bootstrap binding reached the artifact"); })();
"#,
    );
    let host = ProgramHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(outcome, ProgramExecutionOutcome::Completed);
    assert!(live_must_not_run.observed_outstanding.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_journal_opening_with_a_run_cancel_replays_before_the_artifact_requests()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(6);
    repository.create_stream(run).await?;
    let recorded_cancel = repository
        .append_delivery(run, DeliveryKind::RunCancel(payload(&[RUN_CANCEL_BYTE])))
        .await?;
    let artifact = immediately_requesting_artifact();
    let host = ProgramHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::RunCancelled(payload(&[RUN_CANCEL_BYTE]))
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 1);
    assert_eq!(
        journal.entries()[0].frame(),
        &JournalFrame::Delivery(recorded_cancel)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_recorded_terminal_outcome_behind_a_request_outranks_an_unloadable_artifact()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(10);
    repository.create_stream(run).await?;
    let recorded_request = repository
        .append_request(run, None, RequestKind::Now(payload(&[REPLAY_REQUEST_BYTE])))
        .await?;
    let recorded_cancel = repository
        .append_delivery(run, DeliveryKind::RunCancel(payload(&[RUN_CANCEL_BYTE])))
        .await?;
    let artifact = ProgramArtifact::new(r#"import "./outside-the-contract.js";"#);
    let host = ProgramHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::RunCancelled(payload(&[RUN_CANCEL_BYTE]))
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 2);
    assert_eq!(
        journal.entries()[0].frame(),
        &JournalFrame::Request(recorded_request)
    );
    assert_eq!(
        journal.entries()[1].frame(),
        &JournalFrame::Delivery(recorded_cancel)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_leading_run_cancel_outranks_an_artifact_that_cannot_load() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(9);
    repository.create_stream(run).await?;
    let recorded_cancel = repository
        .append_delivery(run, DeliveryKind::RunCancel(payload(&[RUN_CANCEL_BYTE])))
        .await?;
    let artifact = ProgramArtifact::new(r#"import "./outside-the-contract.js";"#);
    let host = ProgramHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::RunCancelled(payload(&[RUN_CANCEL_BYTE]))
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 1);
    assert_eq!(
        journal.entries()[0].frame(),
        &JournalFrame::Delivery(recorded_cancel)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_run_cancel_behind_a_recorded_answer_replays_before_the_next_request()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(7);
    repository.create_stream(run).await?;
    let recorded_request = repository
        .append_request(run, None, RequestKind::Now(payload(&[REPLAY_REQUEST_BYTE])))
        .await?;
    let recorded_answer = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: recorded_request.ordinal(),
                payload: payload(&[REPLAY_ANSWER_BYTE]),
            },
        )
        .await?;
    let recorded_cancel = repository
        .append_delivery(run, DeliveryKind::RunCancel(payload(&[RUN_CANCEL_BYTE])))
        .await?;
    let artifact = two_request_artifact();
    let host = ProgramHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host.execute(run, &artifact, &mut live_must_not_run).await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::RunCancelled(payload(&[RUN_CANCEL_BYTE]))
    );
    assert!(live_must_not_run.observed_outstanding.is_empty());
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 3);
    assert_eq!(
        journal.entries()[0].frame(),
        &JournalFrame::Request(recorded_request)
    );
    assert_eq!(
        journal.entries()[1].frame(),
        &JournalFrame::Delivery(recorded_answer)
    );
    assert_eq!(
        journal.entries()[2].frame(),
        &JournalFrame::Delivery(recorded_cancel)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_loaded_tail_cannot_append_or_mutate_the_journal() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = distinct_run_id(4);
    repository.create_stream(run).await?;
    let winner = repository
        .append_request_if_tail(
            run,
            0,
            None,
            RequestKind::Now(payload(&[FIRST_LIVE_REQUEST_BYTE])),
        )
        .await?
        .expect("the current empty tail admits the first request");

    let stale = repository
        .append_request_if_tail(
            run,
            0,
            None,
            RequestKind::Random(payload(&[SECOND_LIVE_REQUEST_BYTE])),
        )
        .await?;
    let stale_delivery = repository
        .append_delivery_if_tail(
            run,
            0,
            DeliveryKind::Answer {
                resolves: winner.ordinal(),
                payload: payload(&[FIRST_LIVE_ANSWER_BYTE]),
            },
        )
        .await?;
    let loaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let mut replay = ReplayCursor::new(loaded);
    let divergence = replay
        .submit_request(request(
            1,
            RequestKind::Random(payload(&[SECOND_LIVE_REQUEST_BYTE])),
        ))
        .expect_err("the different request kind must diverge");
    let stale_fault = repository
        .append_nondeterminism_fault_if_tail(divergence, 0)
        .await?;

    assert_eq!(stale, None);
    assert_eq!(stale_delivery, None);
    assert_eq!(stale_fault, None);
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 1);
    assert_eq!(journal.entries()[0].frame(), &JournalFrame::Request(winner));

    pool.close().await;
    drop(container);
    Ok(())
}
