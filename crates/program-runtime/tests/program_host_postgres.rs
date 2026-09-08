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

#[path = "../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;
use postgres_test_image::POSTGRES_IMAGE_TAG;
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
        .execute_unregistered(run, &artifact, &mut live)
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
        .execute_unregistered(run, &artifact, &mut replay_must_not_go_live)
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
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
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
        .execute_unregistered(run, &artifact, &mut restarted_live_must_not_run)
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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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
        host.execute_unregistered(run, &artifact, &mut live_must_not_run),
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

    let outcome = host.execute_unregistered(run, &artifact, &mut live).await?;

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
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

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

/// Fixture registration names are arbitrary and distinct.
async fn registration_fixture(
    pool: &PgPool,
    grants: signalbox_domain::program_registration::ProgramGrants,
    artifact: &str,
) -> Result<ProgramRunId, Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramRegistrationRequest;
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let registration = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: "fixture-revision".into(),
                source: artifact.as_bytes().to_vec(),
                artifact: artifact.into(),
                grants,
            },
        )
        .await?;
    Ok(repository
        .start_run(
            signalbox_domain::ProgramRunId::from_uuid(Uuid::now_v7()),
            registration.id,
        )
        .await?)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_capability_requires_a_registered_run_with_the_session_grant()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = ProgramHost::new(journal.clone());
    let unregistered = run_id();
    assert!(host.session_capability(unregistered).await?.is_none());
    journal.create_stream(unregistered).await?;
    assert!(host.session_capability(unregistered).await?.is_none());
    let ungranted = registration_fixture(&pool, ProgramGrants::new([]), "export {};").await?;
    assert!(host.session_capability(ungranted).await?.is_none());
    let granted = registration_fixture(
        &pool,
        ProgramGrants::new([ProgramCapability::Session]),
        "export {};",
    )
    .await?;
    let capability = host
        .session_capability(granted)
        .await?
        .expect("registered session grant");
    assert!(
        matches!(capability.actor(), signalbox_domain::Actor::Program { run: reference } if reference.run() == granted)
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registered_run_executes_its_stored_artifact() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let artifact = immediately_requesting_artifact();
    let run = registration_fixture(
        &pool,
        ProgramGrants::new([ProgramCapability::Time]),
        artifact.source(),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let expected_request = request(1, RequestKind::Now(payload(&[FIRST_LIVE_REQUEST_BYTE])));
    let mut deliveries = ScriptedDeliveries::new([DeliveryKind::Answer {
        resolves: expected_request.ordinal(),
        payload: payload(&[FIRST_LIVE_ANSWER_BYTE]),
    }]);
    let mut effects = EffectProbe {
        policy: signalbox_program_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    assert_eq!(
        ProgramHost::new(journal.clone())
            .execute_registered(run, &mut deliveries, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed,
    );
    assert_eq!(
        deliveries.observed_outstanding,
        vec![vec![expected_request]]
    );
    assert_eq!(
        journal
            .load(run)
            .await?
            .expect("registered journal")
            .entries()
            .len(),
        2
    );
    pool.close().await;
    Ok(())
}

fn effect_artifact(expected_kind: &str) -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ effect }} from "{PROGRAM_SDK_V1_SPECIFIER}";
const result = await effect("judge", "score", new Uint8Array());
if (result.kind !== "{expected_kind}") throw new Error("unexpected effect outcome");
"#
    ))
}

async fn registered_run(
    pool: &PgPool,
    artifact: &ProgramArtifact,
    grants: signalbox_domain::program_registration::ProgramGrants,
) -> Result<ProgramRunId, Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramRegistrationRequest;
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let registration = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: "fixture-revision".into(),
                source: artifact.source().as_bytes().to_vec(),
                artifact: artifact.source().into(),
                grants,
            },
        )
        .await?;
    Ok(repository
        .start_run(ProgramRunId::from_uuid(Uuid::now_v7()), registration.id)
        .await?)
}

struct EffectProbe {
    policy: signalbox_program_runtime::effects::EffectRecovery,
    adopted: Option<InlineFramePayload>,
    executions: usize,
    adoptions: usize,
}

impl signalbox_program_runtime::effects::EffectExecutor for EffectProbe {
    fn recovery(
        &self,
        _: &signalbox_domain::EffectRequest,
    ) -> signalbox_program_runtime::effects::EffectRecovery {
        self.policy
    }
    fn adopt<'a>(
        &'a mut self,
        _: signalbox_program_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        self.adoptions += 1;
        Box::pin(async { Ok(self.adopted.clone()) })
    }
    fn execute<'a>(
        &'a mut self,
        _: signalbox_program_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        self.executions += 1;
        Box::pin(async { Ok(payload(b"executed")) })
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ungranted_effect_is_journaled_as_refused_without_calling_the_executor()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{RejectReason, program_registration::ProgramGrants};
    use signalbox_program_runtime::effects::EffectRecovery;
    let (_container, pool) = migrated_postgres().await?;
    let artifact = effect_artifact("reject");
    let run = registered_run(&pool, &artifact, ProgramGrants::new([])).await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = ProgramHost::new(journal.clone());
    let mut effects = EffectProbe {
        policy: EffectRecovery::Idempotent,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut primitives = ScriptedDeliveries::new([]);
    assert_eq!(
        host.execute_registered(run, &mut primitives, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed
    );
    assert_eq!(effects.executions, 0);
    assert_eq!(effects.adoptions, 0);
    assert!(primitives.observed_outstanding.is_empty());
    let loaded = journal
        .load(run)
        .await?
        .expect("registered run has a journal");
    assert!(
        matches!(loaded.entries().last().expect("refusal journaled").frame(), JournalFrame::Delivery(delivery) if matches!(delivery.kind(), DeliveryKind::Reject { reason: RejectReason::CapabilityDenied, .. }))
    );
    pool.close().await;
    Ok(())
}

/// A persisted request without an answer models a crash after request admission.
async fn recover_effect(
    policy: signalbox_program_runtime::effects::EffectRecovery,
    adopted: Option<InlineFramePayload>,
) -> Result<(EffectProbe, InlineFramePayload), Box<dyn Error>> {
    use signalbox_domain::{EffectRequest, ProgramCapability, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let artifact = effect_artifact("answer");
    let run = registered_run(
        &pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Judge]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    journal
        .append_request(
            run,
            None,
            RequestKind::Effect(EffectRequest::new(
                ProgramCapability::Judge,
                "score".into(),
                InlineFramePayload::default(),
            )),
        )
        .await?;
    let host = ProgramHost::new(journal.clone());
    let mut effects = EffectProbe {
        policy,
        adopted,
        executions: 0,
        adoptions: 0,
    };
    let mut primitives = ScriptedDeliveries::new([]);
    host.execute_registered(run, &mut primitives, &mut effects)
        .await?;
    let loaded = journal.load(run).await?.expect("registered journal exists");
    let JournalFrame::Delivery(delivery) = loaded.entries().last().expect("answer exists").frame()
    else {
        panic!("expected delivery")
    };
    let DeliveryKind::Answer { payload, .. } = delivery.kind() else {
        panic!("expected answer")
    };
    let answer = payload.clone();
    // A second execution consumes the answer without executing or recovering again.
    host.execute_registered(run, &mut primitives, &mut effects)
        .await?;
    pool.close().await;
    Ok((effects, answer))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn crash_recovery_adopts_a_proven_answer_without_reissuing() -> Result<(), Box<dyn Error>> {
    use signalbox_program_runtime::effects::EffectRecovery;
    let retained = payload(b"durable outcome");
    let (effects, answer) =
        recover_effect(EffectRecovery::Ambiguous, Some(retained.clone())).await?;
    assert_eq!(effects.adoptions, 1);
    assert_eq!(effects.executions, 0);
    assert_eq!(answer, retained);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn crash_recovery_reissues_only_a_declared_idempotent_operation() -> Result<(), Box<dyn Error>>
{
    use signalbox_program_runtime::effects::EffectRecovery;
    let (effects, answer) = recover_effect(EffectRecovery::Idempotent, None).await?;
    assert_eq!(effects.adoptions, 1);
    assert_eq!(effects.executions, 1);
    assert_eq!(answer.as_bytes(), b"executed");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn crash_recovery_journals_ambiguity_without_a_silent_reissue() -> Result<(), Box<dyn Error>>
{
    use signalbox_program_runtime::effects::EffectRecovery;
    let (effects, answer) = recover_effect(EffectRecovery::Ambiguous, None).await?;
    assert_eq!(effects.adoptions, 1);
    assert_eq!(effects.executions, 0);
    assert_eq!(answer.as_bytes(), b"{\"outcome\":\"ambiguous\"}");
    Ok(())
}

fn register_artifact(input: &[u8], expected_kind: &str) -> ProgramArtifact {
    ProgramArtifact::new(format!(
        r#"
import {{ effect }} from "{PROGRAM_SDK_V1_SPECIFIER}";
const result = await effect("register", "register", new Uint8Array({input:?}));
if (result.kind !== "{expected_kind}") throw new Error("unexpected registration outcome");
"#
    ))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn program_registration_widening_is_refused_in_the_journal() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, RejectReason, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let input =
        br#"{"id":"01991964-62ef-7000-8000-000000000001","name":"child","revision":"one","source":[],"artifact":"","grants":["judge"]}"#;
    let artifact = register_artifact(input, "reject");
    let run = registered_run(
        &pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Register]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let mut effects = EffectProbe {
        policy: signalbox_program_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    ProgramHost::new(journal.clone())
        .execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
        .await?;
    let registrations: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&pool)
        .await?;
    assert_eq!(registrations, 1);
    let loaded = journal.load(run).await?.expect("registered journal exists");
    assert!(
        matches!(loaded.entries().last().expect("refusal exists").frame(), JournalFrame::Delivery(delivery) if matches!(delivery.kind(), DeliveryKind::Reject { reason: RejectReason::CapabilityDenied, .. }))
    );
    assert_eq!(effects.executions, 0);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registration_recovery_adopts_the_matching_immutable_row() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        EffectRequest, ProgramCapability,
        program_registration::{ProgramGrants, ProgramRegistrationRequest},
    };
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    let (_container, pool) = migrated_postgres().await?;
    let input = br#"{"id":"01991964-62ef-7000-8000-000000000002","name":"child","revision":"one","source":[],"artifact":"","grants":[]}"#;
    let artifact = register_artifact(input, "answer");
    let run = registered_run(
        &pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Register]),
    )
    .await?;
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let child = registrations
        .register_child(
            run,
            signalbox_domain::ProgramRegistrationId::from_uuid(
                deno_core::serde_json::from_slice::<deno_core::serde_json::Value>(input)?["id"]
                    .as_str()
                    .expect("fixture registration identity")
                    .parse()?,
            ),
            ProgramRegistrationRequest {
                name: "child".into(),
                revision: "one".into(),
                source: Vec::new(),
                artifact: String::new(),
                grants: ProgramGrants::new([]),
            },
        )
        .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    journal
        .append_request(
            run,
            None,
            RequestKind::Effect(EffectRequest::new(
                ProgramCapability::Register,
                "register".into(),
                InlineFramePayload::new(input.as_slice()),
            )),
        )
        .await?;
    let mut effects = EffectProbe {
        policy: signalbox_program_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    ProgramHost::new(journal.clone())
        .execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
        .await?;
    let loaded = journal.load(run).await?.expect("registered journal exists");
    let JournalFrame::Delivery(delivery) = loaded.entries().last().expect("answer exists").frame()
    else {
        panic!("expected delivery")
    };
    let DeliveryKind::Answer { payload, .. } = delivery.kind() else {
        panic!("expected answer")
    };
    let answer: deno_core::serde_json::Value =
        deno_core::serde_json::from_slice(payload.as_bytes())?;
    assert_eq!(answer["registration"], child.id.into_uuid().to_string());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2);
    assert_eq!(effects.executions, 0);
    pool.close().await;
    Ok(())
}

struct CancellingEffects(ProgramJournalRepository);

impl signalbox_program_runtime::effects::EffectExecutor for CancellingEffects {
    fn recovery(
        &self,
        _: &signalbox_domain::EffectRequest,
    ) -> signalbox_program_runtime::effects::EffectRecovery {
        signalbox_program_runtime::effects::EffectRecovery::Ambiguous
    }
    fn adopt<'a>(
        &'a mut self,
        _: signalbox_program_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async { Ok(None) })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: signalbox_program_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            self.0
                .append_delivery(
                    invocation.run,
                    DeliveryKind::RunCancel(payload(b"cancelled")),
                )
                .await
                .map_err(|error| LiveDeliveryFailure::new(error.to_string()))?;
            Ok(InlineFramePayload::default())
        })
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cancellation_committed_during_an_effect_remains_the_run_outcome()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let run = registered_run(
        &pool,
        &effect_artifact("answer"),
        ProgramGrants::new([ProgramCapability::Judge]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let outcome = ProgramHost::new(journal.clone())
        .execute_registered(
            run,
            &mut ScriptedDeliveries::new([]),
            &mut CancellingEffects(journal.clone()),
        )
        .await?;
    assert_eq!(
        outcome,
        ProgramExecutionOutcome::RunCancelled(payload(b"cancelled"))
    );
    assert_eq!(
        journal
            .load(run)
            .await?
            .expect("registered journal exists")
            .entries()
            .len(),
        2
    );
    pool.close().await;
    Ok(())
}

fn session_creation_artifact(command: Uuid, model: Uuid) -> (ProgramArtifact, InlineFramePayload) {
    let request = deno_core::serde_json::to_vec(
        &deno_core::serde_json::json!({"command":command.to_string(),"model":model.to_string()}),
    )
    .expect("fixture JSON encodes");
    let bytes = request
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    (
        ProgramArtifact::new(format!(
            r#"
import {{ effect }} from "{PROGRAM_SDK_V1_SPECIFIER}";
const answer = await effect("session", "create", new Uint8Array([{bytes}]));
if (answer.kind !== "answer") throw new Error("creation refused");
"#
        )),
        InlineFramePayload::new(request),
    )
}

fn session_repository(
    pool: &PgPool,
) -> signalbox_persistence::program_session::ProgramSessionRepository {
    let credentials = signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new("fixture-family", "fixture-credential"),
    ])
    .expect("host fixture credential pin is valid");
    signalbox_persistence::program_session::ProgramSessionRepository::new(
        pool.clone(),
        signalbox_persistence::submit_input::SubmitInputRepository::new(pool.clone()),
        signalbox_persistence::create_session::CreateSessionRepository::new(
            pool.clone(),
            credentials,
        ),
    )
}

enum SessionCreationAttempt {
    Live,
    Recovered,
}

async fn execute_session_creation(attempt: SessionCreationAttempt) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        DirectModelSelection, DurableCommandId, EffectRequest, ModelSelectionRequest,
        ProgramCapability, SessionConfigurationDefaults, SessionId,
        program_registration::ProgramGrants, program_session::ProgramSessionCreate,
    };
    use signalbox_program_runtime::{effects::EffectRecovery, session_effects::SessionEffects};
    let (_container, pool) = migrated_postgres().await?;
    let command = Uuid::now_v7();
    let model = Uuid::now_v7();
    let (artifact, request) = session_creation_artifact(command, model);
    let run = registered_run(
        &pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Session]),
    )
    .await?;
    let sessions = session_repository(&pool);
    let journal = ProgramJournalRepository::new(pool.clone());
    let created = if matches!(attempt, SessionCreationAttempt::Recovered) {
        let session = sessions
            .create(
                run,
                ProgramSessionCreate {
                    command: DurableCommandId::from_uuid(command),
                    defaults: SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                        DirectModelSelection::from_uuid(model),
                    )),
                },
            )
            .await?;
        journal
            .append_request(
                run,
                None,
                RequestKind::Effect(EffectRequest::new(
                    ProgramCapability::Session,
                    "create".into(),
                    request,
                )),
            )
            .await?;
        Some(session)
    } else {
        None
    };
    let other = EffectProbe {
        policy: EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut effects = SessionEffects::new(sessions, other, |_| {}, |_| None);
    let host = ProgramHost::new(journal.clone());
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed
    );
    let loaded = journal.load(run).await?.expect("run journal exists");
    let JournalFrame::Delivery(delivery) =
        loaded.entries().last().expect("creation answer").frame()
    else {
        panic!("creation answer")
    };
    let DeliveryKind::Answer { payload, .. } = delivery.kind() else {
        panic!("creation answer")
    };
    let answer: deno_core::serde_json::Value =
        deno_core::serde_json::from_slice(payload.as_bytes())?;
    let session = SessionId::from_uuid(
        answer["session"]
            .as_str()
            .expect("session identity")
            .parse()?,
    );
    if let Some(created) = created {
        assert_eq!(session, created);
    }
    let loaded = signalbox_persistence::session::SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("created session");
    assert!(
        matches!(loaded.creation_provenance().cause(), signalbox_domain::SessionCreationCause::Workflow { run: actor } if actor.run() == run)
    );
    assert_eq!(answer.as_object().expect("answer object").len(), 1);
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM session")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_session_effect_creates_a_workflow_session_host_side() -> Result<(), Box<dyn Error>> {
    execute_session_creation(SessionCreationAttempt::Live).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_creation_recovery_adopts_the_durable_command_receipt() -> Result<(), Box<dyn Error>>
{
    execute_session_creation(SessionCreationAttempt::Recovered).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_turn_answers_retain_only_the_exact_terminal_identities_and_digest()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        CommandPrincipal, DescendantTerminationScope, DirectModelSelection, DurableCommandId,
        ModelSelectionRequest, ProgramCapability, SessionConfigurationDefaults,
        SessionLifecycleCommand, SessionLifecycleOperation, StopStickiness,
        SubmitInputAppliedResult, SubmitInputResult, program_registration::ProgramGrants,
        program_session::ProgramSessionCreate,
    };
    use signalbox_persistence::{
        session_lifecycle_command::SessionLifecycleCommandRepository,
        submit_input::SubmitInputRepository,
    };
    use signalbox_program_runtime::{effects::EffectRecovery, session_effects::SessionEffects};
    let (_container, pool) = migrated_postgres().await?;
    let sessions = session_repository(&pool);
    let creator = registered_run(
        &pool,
        &ProgramArtifact::new("export {};"),
        ProgramGrants::new([ProgramCapability::Session]),
    )
    .await?;
    let session = sessions
        .create(
            creator,
            ProgramSessionCreate {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                defaults: SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                    DirectModelSelection::from_uuid(Uuid::now_v7()),
                )),
            },
        )
        .await?;
    let command = DurableCommandId::from_uuid(Uuid::now_v7());
    let request = deno_core::serde_json::to_vec(&deno_core::serde_json::json!({
        "command": command.into_uuid().to_string(), "session": session.into_uuid().to_string(),
        "text": "private transcript input", "defaults_version": 1,
    }))?;
    let bytes = request
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let artifact = ProgramArtifact::new(format!(
        r#"
import {{ effect }} from "{PROGRAM_SDK_V1_SPECIFIER}";
const answer = await effect("session", "turn", new Uint8Array([{bytes}]));
if (answer.kind !== "answer") throw new Error("turn refused");
"#
    ));
    let run = registered_run(
        &pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Session]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = ProgramHost::new(journal.clone());
    let (nudge, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let other = EffectProbe {
        policy: EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut effects = SessionEffects::new(
        sessions,
        other,
        |session| {
            nudge.send(session).expect("fixture receiver exists");
        },
        |_| None,
    );
    let execute = async {
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await
            .map_err(Box::<dyn Error>::from)
    };
    let stop = async {
        assert_eq!(ready.recv().await, Some(session));
        SessionLifecycleCommandRepository::new(pool.clone())
            .handle(
                SessionLifecycleCommand::new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    session,
                    SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                CommandPrincipal::Operator,
            )
            .await
            .map_err(Box::<dyn Error>::from)
    };
    let (execution, _) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::try_join!(execute, stop)
    })
    .await??;
    assert_eq!(execution, ProgramExecutionOutcome::Completed);
    let receipt = SubmitInputRepository::new(pool.clone())
        .load(command)
        .await?
        .expect("program input receipt");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        panic!("turn origin")
    };
    let loaded = journal.load(run).await?.expect("run journal");
    let JournalFrame::Delivery(delivery) = loaded.entries().last().expect("turn answer").frame()
    else {
        panic!("turn answer")
    };
    let DeliveryKind::Answer { payload, .. } = delivery.kind() else {
        panic!("turn answer")
    };
    let answer: deno_core::serde_json::Value =
        deno_core::serde_json::from_slice(payload.as_bytes())?;
    assert_eq!(answer["session"], session.into_uuid().to_string());
    assert_eq!(answer["turn"], origin.turn().into_uuid().to_string());
    assert_eq!(
        answer["accepted_input"],
        origin.accepted_input().into_uuid().to_string()
    );
    assert_eq!(answer["outcome"], "retired");
    assert_eq!(answer["digest"].as_array().expect("digest bytes").len(), 32);
    assert_eq!(answer.as_object().expect("thin answer").len(), 5);
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed
    );
    assert_eq!(journal.load(run).await?.expect("replayed journal"), loaded);
    pool.close().await;
    Ok(())
}
