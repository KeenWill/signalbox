//! PostgreSQL integration coverage for both workflow host adapters.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use signalbox_persistence::test_support::postgres::TestDatabase;
use std::{collections::VecDeque, error::Error, future::Future, pin::Pin};

use signalbox_domain::{
    DeliveryKind, InlineFramePayload, JournalFrame, ProgramFault, ProgramRunId, ReplayCursor,
    RequestFrame, RequestKind, RequestOrdinal,
};
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_workflow_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, PROGRAM_SDK_V1_SPECIFIER, ProgramArtifact,
    ProgramExecutionOutcome, WorkflowHost, WorkflowHostError, WorkflowHostProtocolError,
};
use sqlx::PgPool;
use uuid::Uuid;

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

async fn migrated_postgres() -> Result<(TestDatabase, PgPool), Box<dyn Error>> {
    let (database, pool, _) =
        signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
    Ok((database, pool))
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
    let host = WorkflowHost::new(repository.clone());
    let mut live =
        ScriptedDeliveries::new([expected_concurrent_kind.clone(), expected_live_kind.clone()]);

    let first_outcome = host
        .execute_unregistered(run, &artifact, &mut live)
        .await
        .expect("partial-journal execution must reach live and complete");

    assert_eq!(
        first_outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
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
    assert_eq!(after_live.entries().len(), 8);
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

    assert_eq!(
        replay_outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
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
    let host = WorkflowHost::new(repository.clone());
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let failure = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await
        .expect_err("different request bytes must stop the isolate host");
    let WorkflowHostError::Nondeterminism {
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
    let host = WorkflowHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
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
    let host = WorkflowHost::new(repository);
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
            WorkflowHostError::Protocol(WorkflowHostProtocolError::Stalled)
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
    let host = WorkflowHost::new(repository.clone());

    let outcome = host.execute_unregistered(run, &artifact, &mut live).await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    assert_eq!(live.observed_outstanding, vec![vec![expected_request]]);
    let journal = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    assert_eq!(journal.entries().len(), 4);
    assert_eq!(journal.result(), Some(&InlineFramePayload::default()));

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
    let host = WorkflowHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let failure = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await
        .expect_err("a module that throws must not report completion");

    let WorkflowHostError::Isolate(error) = failure else {
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
    let host = WorkflowHost::new(repository);
    let mut live_must_not_run = ScriptedDeliveries::new([]);

    let outcome = host
        .execute_unregistered(run, &artifact, &mut live_must_not_run)
        .await?;

    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
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
    let host = WorkflowHost::new(repository.clone());
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
    let host = WorkflowHost::new(repository.clone());
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
    let host = WorkflowHost::new(repository.clone());
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
    let host = WorkflowHost::new(repository.clone());
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
    registration_with_input(pool, grants, artifact, &[]).await
}

async fn registration_with_input(
    pool: &PgPool,
    grants: signalbox_domain::program_registration::ProgramGrants,
    artifact: &str,
    input: &[u8],
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
            input,
        )
        .await?)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn javascript_registration_hashes_exact_source_bytes_before_persistence()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramRegistrationRequest;
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let registration = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: "fixture-revision".into(),
                source: b"// exact source\nexport {};\n".to_vec(),
                artifact: "export {};".into(),
                grants: ProgramGrants::new([]),
            },
        )
        .await?;
    let stored_digest: String = sqlx::query_scalar(
        "SELECT encode(source_digest, 'hex') FROM program_registration WHERE registration_id = $1",
    )
    .bind(registration.id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_digest, "9877bc113c535557e69aa1ec2adce73b6ba7a95cf5ed745405e9f1a28706971c",
        "source hashing must retain the submitted comment and trailing newline"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_capability_requires_a_registered_run_with_the_session_grant()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, program_registration::ProgramGrants};
    let (_container, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = WorkflowHost::new(journal.clone());
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
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    assert_eq!(
        WorkflowHost::new(journal.clone())
            .execute_registered(run, &mut deliveries, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default()),
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
        4
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
        .start_run(
            ProgramRunId::from_uuid(Uuid::now_v7()),
            registration.id,
            &[],
        )
        .await?)
}

struct EffectProbe {
    policy: signalbox_workflow_runtime::effects::EffectRecovery,
    adopted: Option<InlineFramePayload>,
    executions: usize,
    adoptions: usize,
}

impl signalbox_workflow_runtime::effects::EffectExecutor for EffectProbe {
    fn recovery(
        &self,
        _: &signalbox_domain::EffectRequest,
    ) -> signalbox_workflow_runtime::effects::EffectRecovery {
        self.policy
    }
    fn adopt<'a>(
        &'a mut self,
        _: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        self.adoptions += 1;
        Box::pin(async { Ok(self.adopted.clone()) })
    }
    fn execute<'a>(
        &'a mut self,
        _: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
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
    use signalbox_workflow_runtime::effects::EffectRecovery;
    let (_container, pool) = migrated_postgres().await?;
    let artifact = effect_artifact("reject");
    let run = registered_run(&pool, &artifact, ProgramGrants::new([])).await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = WorkflowHost::new(journal.clone());
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
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    assert_eq!(effects.executions, 0);
    assert_eq!(effects.adoptions, 0);
    assert!(primitives.observed_outstanding.is_empty());
    let loaded = journal
        .load(run)
        .await?
        .expect("registered run has a journal");
    assert!(
        matches!(loaded.entries().get(1).expect("effect delivery precedes completion").frame(), JournalFrame::Delivery(delivery) if matches!(delivery.kind(), DeliveryKind::Reject { reason: RejectReason::CapabilityDenied, .. }))
    );
    pool.close().await;
    Ok(())
}

/// A persisted request without an answer models a crash after request admission.
async fn recover_effect(
    policy: signalbox_workflow_runtime::effects::EffectRecovery,
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
    let host = WorkflowHost::new(journal.clone());
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
    let JournalFrame::Delivery(delivery) = loaded
        .entries()
        .get(1)
        .expect("effect delivery precedes completion")
        .frame()
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
    use signalbox_workflow_runtime::effects::EffectRecovery;
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
    use signalbox_workflow_runtime::effects::EffectRecovery;
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
    use signalbox_workflow_runtime::effects::EffectRecovery;
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
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    WorkflowHost::new(journal.clone())
        .execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
        .await?;
    let registrations: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&pool)
        .await?;
    assert_eq!(registrations, 1);
    let loaded = journal.load(run).await?.expect("registered journal exists");
    assert!(
        matches!(loaded.entries().get(1).expect("effect delivery precedes completion").frame(), JournalFrame::Delivery(delivery) if matches!(delivery.kind(), DeliveryKind::Reject { reason: RejectReason::CapabilityDenied, .. }))
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
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    WorkflowHost::new(journal.clone())
        .execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
        .await?;
    let loaded = journal.load(run).await?.expect("registered journal exists");
    let JournalFrame::Delivery(delivery) = loaded
        .entries()
        .get(1)
        .expect("effect delivery precedes completion")
        .frame()
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

impl signalbox_workflow_runtime::effects::EffectExecutor for CancellingEffects {
    fn recovery(
        &self,
        _: &signalbox_domain::EffectRequest,
    ) -> signalbox_workflow_runtime::effects::EffectRecovery {
        signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous
    }
    fn adopt<'a>(
        &'a mut self,
        _: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async { Ok(None) })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
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
    let outcome = WorkflowHost::new(journal.clone())
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

enum SessionEffectAttempt {
    Live,
    Recovered,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn emitted_typescript_entry_runs_through_host_and_replays_its_checked_result()
-> Result<(), Box<dyn Error>> {
    use deno_core::serde_json::{from_slice, json, to_vec};
    use signalbox_domain::{ProgramCapability, SessionId, program_registration::ProgramGrants};
    use signalbox_workflow_runtime::{effects::EffectRecovery, session_effects::SessionEffects};

    let (_container, pool) = migrated_postgres().await?;
    let input =
        json!({ "command": Uuid::now_v7().to_string(), "model": Uuid::now_v7().to_string() });
    let input_bytes = to_vec(&input)?;
    let artifact = include_str!("fixtures/session.js");
    let grants = ProgramGrants::new([ProgramCapability::Session]);
    let run = registration_with_input(&pool, grants.clone(), artifact, &input_bytes).await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = WorkflowHost::new(journal.clone());
    let mut primitives = ScriptedDeliveries::new([]);
    let unused_effects = || EffectProbe {
        policy: EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut effects = SessionEffects::new(
        session_repository(&pool),
        unused_effects(),
        |_| {},
        |_| None,
    );
    let outcome = host
        .execute_registered(run, &mut primitives, &mut effects)
        .await?;
    let ProgramExecutionOutcome::Completed(result) = &outcome else {
        panic!("typed entrypoint completes successfully");
    };
    let answer: deno_core::serde_json::Value = from_slice(result.as_bytes())?;
    let session = SessionId::from_uuid(
        answer["session"]
            .as_str()
            .expect("session result")
            .parse()?,
    );
    let created = signalbox_persistence::session::SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("entrypoint creates a session");
    assert!(
        matches!(created.creation_provenance().cause(), signalbox_domain::SessionCreationCause::Workflow { run: actor } if actor.run() == run)
    );
    let loaded = journal.load(run).await?.expect("completed journal");
    assert_eq!(loaded.result(), Some(result));
    let JournalFrame::Request(request) = loaded.entries()[0].frame() else {
        panic!("entrypoint effect request");
    };
    let RequestKind::Effect(effect) = request.kind() else {
        panic!("session create effect");
    };
    assert_eq!(
        from_slice::<deno_core::serde_json::Value>(effect.payload().as_bytes())?,
        input
    );
    let JournalFrame::Delivery(delivery) = loaded.entries()[1].frame() else {
        panic!("entrypoint effect answer");
    };
    assert!(matches!(delivery.kind(), DeliveryKind::Answer { payload, .. } if payload == result));

    // Resume a journal containing the effect and its answer but no terminal result.
    let replay_run = registration_with_input(&pool, grants.clone(), artifact, &input_bytes).await?;
    journal
        .append_request(replay_run, None, request.kind().clone())
        .await?;
    journal
        .append_delivery(replay_run, delivery.kind().clone())
        .await?;
    let mut no_live_effects = unused_effects();
    assert_eq!(
        host.execute_registered(replay_run, &mut primitives, &mut no_live_effects)
            .await?,
        outcome
    );
    assert_eq!(
        host.execute_registered(run, &mut primitives, &mut no_live_effects)
            .await?,
        outcome
    );
    assert_eq!(no_live_effects.executions, 0);
    assert_eq!(no_live_effects.adoptions, 0);
    assert!(primitives.observed_outstanding.is_empty());

    let invalid_run = registration_with_input(&pool, grants, artifact, b"{}").await?;
    let error = host
        .execute_registered(invalid_run, &mut primitives, &mut no_live_effects)
        .await
        .expect_err("input is checked before run");
    assert!(
        error
            .to_string()
            .contains("expected command and model strings")
    );
    assert!(
        journal
            .load(invalid_run)
            .await?
            .expect("invalid input journal")
            .entries()
            .is_empty()
    );
    assert_eq!(no_live_effects.executions, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM session")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn entrypoint_bytes_use_preloaded_intrinsics_and_nonbyte_results_cannot_complete()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramGrants;
    use signalbox_workflow_runtime::effects::EffectRecovery;
    let (_container, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let host = WorkflowHost::new(journal.clone());
    let mut effects = EffectProbe {
        policy: EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut primitives = ScriptedDeliveries::new([]);
    let artifact = r#"
globalThis.Uint8Array = () => { throw new Error("late bound constructor"); };
export default function(input) {
  if (this !== undefined) throw new Error("entrypoint received a receiver");
  return input;
}
"#;
    let input = b"retained input returned unchanged";
    let run = registration_with_input(&pool, ProgramGrants::new([]), artifact, input).await?;
    assert_eq!(
        host.execute_registered(run, &mut primitives, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(input.as_slice()))
    );
    let invalid =
        registration_fixture(&pool, ProgramGrants::new([]), "export default () => [1];").await?;
    let error = host
        .execute_registered(invalid, &mut primitives, &mut effects)
        .await
        .expect_err("nonbyte entrypoint result");
    assert!(
        error
            .to_string()
            .contains("program entrypoint must return a Uint8Array")
    );
    assert!(
        journal
            .load(invalid)
            .await?
            .expect("invalid result journal")
            .result()
            .is_none()
    );
    assert_eq!(effects.executions, 0);
    assert!(primitives.observed_outstanding.is_empty());
    pool.close().await;
    Ok(())
}

async fn execute_session_creation(attempt: SessionEffectAttempt) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        DirectModelSelection, DurableCommandId, EffectRequest, ModelSelectionRequest,
        ProgramCapability, SessionConfigurationDefaults, SessionId,
        program_registration::ProgramGrants, program_session::ProgramSessionCreate,
    };
    use signalbox_workflow_runtime::{effects::EffectRecovery, session_effects::SessionEffects};
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
    let created = if matches!(attempt, SessionEffectAttempt::Recovered) {
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
    let host = WorkflowHost::new(journal.clone());
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    let loaded = journal.load(run).await?.expect("run journal exists");
    let JournalFrame::Delivery(delivery) = loaded
        .entries()
        .get(1)
        .expect("effect delivery precedes completion")
        .frame()
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
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
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
    execute_session_creation(SessionEffectAttempt::Live).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_creation_recovery_adopts_the_durable_command_receipt() -> Result<(), Box<dyn Error>>
{
    execute_session_creation(SessionEffectAttempt::Recovered).await
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
    use signalbox_workflow_runtime::{effects::EffectRecovery, session_effects::SessionEffects};
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
    let host = WorkflowHost::new(journal.clone());
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
    assert_eq!(
        execution,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    let receipt = SubmitInputRepository::new(pool.clone())
        .load(command)
        .await?
        .expect("program input receipt");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        panic!("turn origin")
    };
    let loaded = journal.load(run).await?.expect("run journal");
    let JournalFrame::Delivery(delivery) = loaded
        .entries()
        .get(1)
        .expect("effect delivery precedes completion")
        .frame()
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
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    assert_eq!(journal.load(run).await?.expect("replayed journal"), loaded);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_unregistered_run_is_rejected_by_registered_execution()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let run = ProgramRunId::from_uuid(Uuid::now_v7());
    journal.create_stream(run).await?;
    journal
        .append_delivery(run, DeliveryKind::RunCancel(payload(b"cancelled")))
        .await?;
    let error = WorkflowHost::new(journal)
        .execute_registered(
            run,
            &mut ScriptedDeliveries::new([]),
            &mut EffectProbe {
                policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
                adopted: None,
                executions: 0,
                adoptions: 0,
            },
        )
        .await
        .expect_err("terminal streams require the same registration as live execution");
    assert!(matches!(
        error,
        WorkflowHostError::Registration(
            signalbox_persistence::program_registration::ProgramRegistrationError::RunMissing
        )
    ));
    pool.close().await;
    Ok(())
}

async fn refused_session_effect(
    pool: &PgPool,
    attempt: SessionEffectAttempt,
    method: &str,
    input: &[u8],
) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{EffectRequest, ProgramCapability, program_registration::ProgramGrants};
    use signalbox_workflow_runtime::{effects::EffectRecovery, session_effects::SessionEffects};
    let artifact = ProgramArtifact::new(format!(
        r#"
import {{ effect }} from "{PROGRAM_SDK_V1_SPECIFIER}";
const answer = await effect("session", "{method}", new Uint8Array({input:?}));
if (answer.kind !== "answer") throw new Error("expected a session refusal answer");
"#
    ));
    let run = registered_run(
        pool,
        &artifact,
        ProgramGrants::new([ProgramCapability::Session]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    if matches!(attempt, SessionEffectAttempt::Recovered) {
        journal
            .append_request(
                run,
                None,
                RequestKind::Effect(EffectRequest::new(
                    ProgramCapability::Session,
                    method.into(),
                    InlineFramePayload::new(input),
                )),
            )
            .await?;
    }
    let other = EffectProbe {
        policy: EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let mut effects = SessionEffects::new(session_repository(pool), other, |_| {}, |_| None);
    let host = WorkflowHost::new(journal.clone());
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    let loaded = journal.load(run).await?.expect("registered journal");
    let JournalFrame::Delivery(delivery) = loaded
        .entries()
        .get(1)
        .expect("effect delivery precedes completion")
        .frame()
    else {
        panic!("refusal answer")
    };
    let DeliveryKind::Answer { payload, .. } = delivery.kind() else {
        panic!("refusal answer")
    };
    assert_eq!(payload.as_bytes(), br#"{"outcome":"refused"}"#);
    assert_eq!(
        host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::default())
    );
    assert_eq!(journal.load(run).await?.expect("replayed journal"), loaded);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unsupported_session_operation_is_journaled_as_a_replayable_refusal()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    refused_session_effect(&pool, SessionEffectAttempt::Live, "unknown", &[]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unanswered_unsupported_session_operation_recovers_to_a_refusal()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    refused_session_effect(&pool, SessionEffectAttempt::Recovered, "unknown", &[]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn malformed_registration_requests_are_durably_rejected() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        EffectRequest, ProgramCapability, RejectReason, program_registration::ProgramGrants,
    };
    let (_container, pool) = migrated_postgres().await?;
    let malformed: &[&[u8]] = &[
        b"{",
        br#"{"id":"invalid","name":"child","revision":"one","source":[],"artifact":"","grants":[]}"#,
    ];
    for input in malformed {
        for recovered in [false, true] {
            let artifact = register_artifact(input, "reject");
            let run = registered_run(
                &pool,
                &artifact,
                ProgramGrants::new([ProgramCapability::Register]),
            )
            .await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            if recovered {
                journal
                    .append_request(
                        run,
                        None,
                        RequestKind::Effect(EffectRequest::new(
                            ProgramCapability::Register,
                            "register".into(),
                            InlineFramePayload::new(*input),
                        )),
                    )
                    .await?;
            }
            let mut effects = EffectProbe {
                policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
                adopted: None,
                executions: 0,
                adoptions: 0,
            };
            let host = WorkflowHost::new(journal.clone());
            host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
                .await?;
            let loaded = journal.load(run).await?.expect("registered journal exists");
            assert!(
                matches!(loaded.entries().get(1).expect("effect delivery precedes completion").frame(),
                JournalFrame::Delivery(delivery) if matches!(delivery.kind(),
                    DeliveryKind::Reject { reason: RejectReason::UnsupportedOperation, .. }))
            );
            host.execute_registered(run, &mut ScriptedDeliveries::new([]), &mut effects)
                .await?;
            let replayed = journal.load(run).await?.expect("replayed journal exists");
            assert_eq!(replayed.entries(), loaded.entries());
            assert_eq!(effects.executions, 0);
        }
    }
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn invalid_session_requests_are_refused_live_and_after_recovery() -> Result<(), Box<dyn Error>>
{
    use deno_core::serde_json::json;
    let (_container, pool) = migrated_postgres().await?;
    let identity = Uuid::now_v7().to_string();
    let create = json!({"command": identity, "model": identity});
    let turn =
        json!({"command": identity, "session": identity, "text": "input", "defaults_version": 1});
    let mut invalid = vec![("create", b"{".to_vec()), ("turn", b"{".to_vec())];
    for (method, valid, field, value) in [
        ("create", &create, "command", json!("invalid")),
        ("create", &create, "model", json!("invalid")),
        ("turn", &turn, "command", json!("invalid")),
        ("turn", &turn, "session", json!("invalid")),
        ("turn", &turn, "defaults_version", json!(0)),
        ("turn", &turn, "text", json!("")),
    ] {
        let mut input = valid.clone();
        input[field] = value;
        invalid.push((method, deno_core::serde_json::to_vec(&input)?));
    }
    for (method, input) in invalid {
        refused_session_effect(&pool, SessionEffectAttempt::Live, method, &input).await?;
        refused_session_effect(&pool, SessionEffectAttempt::Recovered, method, &input).await?;
    }
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cancellation_after_the_initial_load_outranks_successful_completion()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramGrants;
    let (_container, pool) = migrated_postgres().await?;
    let run = registered_run(
        &pool,
        &ProgramArtifact::new("export {};"),
        ProgramGrants::new([]),
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let mut registration_lock = pool.begin().await?;
    sqlx::query("LOCK TABLE program_registration IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *registration_lock)
        .await?;
    let host = WorkflowHost::new(journal.clone());
    let mut primitives = ScriptedDeliveries::new([]);
    let mut effects = EffectProbe {
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    let execute = async {
        host.execute_registered(run, &mut primitives, &mut effects)
            .await
            .map_err(Box::<dyn Error>::from)
    };
    let cancel = async {
        loop {
            let registration_read_blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND relation = 'program_registration'::regclass AND mode = 'AccessShareLock' AND NOT granted)",
            ).fetch_one(&pool).await?;
            if registration_read_blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
        journal
            .append_delivery(run, DeliveryKind::RunCancel(payload(b"cancelled")))
            .await?;
        registration_lock.commit().await?;
        Ok::<_, Box<dyn Error>>(())
    };
    let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::try_join!(execute, cancel)
    })
    .await??;
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
        1
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retained_success_loads_without_executable_code_or_live_work() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let artifact = ProgramArtifact::new("invalid JavaScript syntax !!!");
    let run = registered_run(
        &pool,
        &artifact,
        signalbox_domain::program_registration::ProgramGrants::new([]),
    )
    .await?;
    let result = payload(b"typed retained result");
    repository
        .complete_if_tail(run, 0, result.clone())
        .await?
        .expect("success");
    let host = WorkflowHost::new(repository.clone());
    let mut live = ScriptedDeliveries::new([]);
    assert_eq!(
        host.execute_unregistered(run, &artifact, &mut live).await?,
        ProgramExecutionOutcome::Completed(result.clone())
    );
    let mut effects = EffectProbe {
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    };
    assert_eq!(
        host.execute_registered(run, &mut live, &mut effects)
            .await?,
        ProgramExecutionOutcome::Completed(result)
    );
    assert!(live.observed_outstanding.is_empty());
    assert_eq!(effects.executions, 0);
    assert_eq!(effects.adoptions, 0);
    pool.close().await;
    Ok(())
}

use signalbox_domain::program_registration::{
    NativeProgramRegistrationRequest, ProgramExecutable, ProgramGrants,
};
use signalbox_workflow_runtime::native::{
    NativeCatalog, NativeProgram, NativeProgramError, NativeValue, WorkflowContext,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeNumber(u64);

impl NativeValue for NativeNumber {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        Ok(Self(u64::from_be_bytes(bytes.try_into().map_err(
            |_| NativeProgramError::new("expected one big-endian u64"),
        )?)))
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        Ok(self.0.to_be_bytes().to_vec())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ClockResult {
    input: NativeNumber,
    time: NativeNumber,
}

impl NativeValue for ClockResult {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        let (input, time) = bytes
            .split_at_checked(size_of::<u64>())
            .ok_or_else(|| NativeProgramError::new("missing clock input"))?;
        Ok(Self {
            input: NativeNumber::decode(input)?,
            time: NativeNumber::decode(time)?,
        })
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        let mut bytes = self.input.encode()?;
        bytes.extend(self.time.encode()?);
        Ok(bytes)
    }
}

struct ClockProgram;
impl NativeProgram for ClockProgram {
    type Input = NativeNumber;
    type Output = ClockResult;
    async fn run(
        mut context: WorkflowContext,
        input: Self::Input,
    ) -> Result<Self::Output, NativeProgramError> {
        let answer = context
            .now(InlineFramePayload::new(input.encode()?))
            .await?;
        Ok(ClockResult {
            input,
            time: NativeNumber::decode(answer.as_bytes())?,
        })
    }
}

async fn native_fixture<P: NativeProgram>(
    pool: &PgPool,
    grants: ProgramGrants,
    input: &[u8],
) -> Result<(WorkflowHost, ProgramRunId), Box<dyn Error>> {
    let mut catalog = NativeCatalog::new()?;
    let entry = std::any::type_name::<P>();
    catalog.insert::<P>(entry.into(), "one".into())?;
    let executable = catalog.executable(entry, "one").expect("compiled entry");
    let ProgramExecutable::Native {
        entry,
        revision: native_revision,
        binary_digest,
    } = executable
    else {
        panic!("native catalog entry")
    };
    let repository =
        signalbox_persistence::program_registration::ProgramRegistrationRepository::new(
            pool.clone(),
        );
    let registration = repository
        .register_native_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            NativeProgramRegistrationRequest {
                name: "fixture".into(),
                revision: "one".into(),
                entry,
                native_revision,
                binary_digest,
                grants,
            },
        )
        .await?;
    let run = ProgramRunId::from_uuid(Uuid::now_v7());
    repository.start_run(run, registration.id, input).await?;
    assert_eq!(repository.for_run(run).await?, Some(registration));
    Ok((
        WorkflowHost::new(ProgramJournalRepository::new(pool.clone())).with_native_catalog(catalog),
        run,
    ))
}

fn no_native_effects() -> EffectProbe {
    EffectProbe {
        policy: signalbox_workflow_runtime::effects::EffectRecovery::Ambiguous,
        adopted: None,
        executions: 0,
        adoptions: 0,
    }
}

/// The fixture input and clock reading are arbitrary distinct full-width values.
const NATIVE_INPUT: NativeNumber = NativeNumber(u64::MAX - 1);
const NATIVE_TIME: NativeNumber = NativeNumber(u64::MAX);

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_clock_retains_typed_result_without_resolving_code_on_retry()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let mut clock = ScriptedDeliveries::new([DeliveryKind::Answer {
        resolves: request(1, RequestKind::Now(InlineFramePayload::default())).ordinal(),
        payload: InlineFramePayload::new(NATIVE_TIME.encode()?),
    }]);
    let mut effects = no_native_effects();
    let outcome = host
        .execute_registered(run, &mut clock, &mut effects)
        .await?;
    let ProgramExecutionOutcome::Completed(result) = &outcome else {
        panic!("native clock completes: {outcome:?}")
    };
    assert_eq!(
        ClockResult::decode(result.as_bytes())?,
        ClockResult {
            input: NATIVE_INPUT,
            time: NATIVE_TIME
        }
    );
    assert_eq!(
        clock.observed_outstanding,
        vec![vec![request(
            1,
            RequestKind::Now(InlineFramePayload::new(NATIVE_INPUT.encode()?))
        )]]
    );
    let loaded = journal.load(run).await?.expect("retained clock journal");
    assert_eq!(loaded.result(), Some(result));
    assert_eq!(loaded.entries().len(), 4);
    let mut no_live = ScriptedDeliveries::new([]);
    assert_eq!(
        WorkflowHost::new(journal.clone())
            .execute_registered(run, &mut no_live, &mut effects)
            .await?,
        outcome
    );
    assert_eq!(
        journal
            .load(run)
            .await?
            .expect("retained journal")
            .entries(),
        loaded.entries()
    );
    assert!(no_live.observed_outstanding.is_empty());
    assert_eq!(effects.executions, 0);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_partial_replay_resumes_a_request_without_its_answer() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let recorded = journal
        .append_request(
            run,
            None,
            RequestKind::Now(InlineFramePayload::new(NATIVE_INPUT.encode()?)),
        )
        .await?;
    let mut clock = ScriptedDeliveries::new([DeliveryKind::Answer {
        resolves: recorded.ordinal(),
        payload: InlineFramePayload::new(NATIVE_TIME.encode()?),
    }]);
    let outcome = host
        .execute_registered(run, &mut clock, &mut no_native_effects())
        .await?;
    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(
            ClockResult {
                input: NATIVE_INPUT,
                time: NATIVE_TIME
            }
            .encode()?
        ))
    );
    assert_eq!(clock.observed_outstanding, vec![vec![recorded.clone()]]);
    let loaded = journal.load(run).await?.expect("resumed journal");
    assert_eq!(
        loaded.entries().first().expect("original request").frame(),
        &JournalFrame::Request(recorded)
    );
    assert_eq!(loaded.entries().len(), 4);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_partial_replay_consumes_a_recorded_clock_answer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let recorded = journal
        .append_request(
            run,
            None,
            RequestKind::Now(InlineFramePayload::new(NATIVE_INPUT.encode()?)),
        )
        .await?;
    journal
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: recorded.ordinal(),
                payload: InlineFramePayload::new(NATIVE_TIME.encode()?),
            },
        )
        .await?;
    let mut clock = ScriptedDeliveries::new([]);
    assert_eq!(
        host.execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(
            ClockResult {
                input: NATIVE_INPUT,
                time: NATIVE_TIME
            }
            .encode()?
        ))
    );
    assert!(clock.observed_outstanding.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_missing_grant_records_refusal_before_any_live_clock_call()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) =
        native_fixture::<ClockProgram>(&pool, ProgramGrants::new([]), &NATIVE_INPUT.encode()?)
            .await?;
    let mut clock = ScriptedDeliveries::new([]);
    assert!(matches!(
        host.execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Faulted(ProgramFault::ProgramError(_))
    ));
    assert!(clock.observed_outstanding.is_empty());
    let loaded = ProgramJournalRepository::new(pool.clone())
        .load(run)
        .await?
        .expect("refused journal");
    assert!(
        matches!(loaded.entries()[1].frame(), JournalFrame::Delivery(frame) if matches!(frame.kind(), DeliveryKind::Reject { reason: signalbox_domain::RejectReason::CapabilityDenied, .. }))
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_divergence_records_the_shared_nondeterminism_fault() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let journal = ProgramJournalRepository::new(pool.clone());
    let expected = journal
        .append_request(run, None, RequestKind::Now(payload(b"different request")))
        .await?;
    let mut clock = ScriptedDeliveries::new([]);
    let outcome = host
        .execute_registered(run, &mut clock, &mut no_native_effects())
        .await?;
    assert!(
        matches!(&outcome, ProgramExecutionOutcome::Faulted(ProgramFault::Nondeterminism { expected: frame, observed }) if frame == &expected && observed.kind() == &RequestKind::Now(InlineFramePayload::new(NATIVE_INPUT.encode()?)))
    );
    assert!(clock.observed_outstanding.is_empty());
    assert_eq!(
        WorkflowHost::new(journal)
            .execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        outcome
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_invalid_input_faults_before_program_requests() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        b"invalid",
    )
    .await?;
    let mut clock = ScriptedDeliveries::new([]);
    assert!(matches!(
        host.execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Faulted(ProgramFault::ProgramError(_))
    ));
    assert!(clock.observed_outstanding.is_empty());
    assert_eq!(
        ProgramJournalRepository::new(pool.clone())
            .load(run)
            .await?
            .expect("faulted journal")
            .entries()
            .len(),
        1
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unavailable_native_revision_faults_without_executing_another_revision()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let mut catalog = NativeCatalog::new()?;
    catalog.insert::<ClockProgram>(
        std::any::type_name::<ClockProgram>().into(),
        "different-revision".into(),
    )?;
    let host =
        WorkflowHost::new(ProgramJournalRepository::new(pool.clone())).with_native_catalog(catalog);
    let mut clock = ScriptedDeliveries::new([]);
    let outcome = host
        .execute_registered(run, &mut clock, &mut no_native_effects())
        .await?;
    assert!(matches!(
        &outcome,
        ProgramExecutionOutcome::Faulted(ProgramFault::ContractRetired(_))
    ));
    assert_eq!(
        host.execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        outcome
    );
    assert!(clock.observed_outstanding.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unavailable_native_binary_faults_even_when_entry_and_revision_match()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut catalog = NativeCatalog::new()?;
    catalog.insert::<ClockProgram>("fixture".into(), "one".into())?;
    let executable = catalog
        .executable("fixture", "one")
        .expect("compiled entry");
    let ProgramExecutable::Native {
        entry,
        revision: native_revision,
        ..
    } = executable
    else {
        panic!("native entry")
    };
    let repository =
        signalbox_persistence::program_registration::ProgramRegistrationRepository::new(
            pool.clone(),
        );
    let registration = repository
        .register_native_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            NativeProgramRegistrationRequest {
                name: "fixture".into(),
                revision: "one".into(),
                entry,
                native_revision,
                binary_digest: signalbox_domain::program_registration::ProgramContentDigest::of(
                    b"different executable",
                ),
                grants: ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
            },
        )
        .await?;
    let run = ProgramRunId::from_uuid(Uuid::now_v7());
    repository
        .start_run(run, registration.id, &NATIVE_INPUT.encode()?)
        .await?;
    let mut clock = ScriptedDeliveries::new([]);
    assert!(matches!(
        WorkflowHost::new(ProgramJournalRepository::new(pool.clone()))
            .with_native_catalog(catalog)
            .execute_registered(run, &mut clock, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Faulted(ProgramFault::ContractRetired(_))
    ));
    assert!(clock.observed_outstanding.is_empty());
    pool.close().await;
    Ok(())
}

struct ReceiptProgram;
impl NativeProgram for ReceiptProgram {
    type Input = NativeNumber;
    type Output = NativeNumber;
    async fn run(
        mut context: WorkflowContext,
        input: Self::Input,
    ) -> Result<Self::Output, NativeProgramError> {
        let answer = context
            .effect(signalbox_domain::EffectRequest::new(
                signalbox_domain::ProgramCapability::Judge,
                "fixture".into(),
                InlineFramePayload::new(input.encode()?),
            ))
            .await?;
        NativeNumber::decode(answer.as_bytes())
    }
}

struct LostAnswerEffect {
    pool: PgPool,
    executions: usize,
    adoptions: usize,
}
impl signalbox_workflow_runtime::effects::EffectExecutor for LostAnswerEffect {
    fn recovery(
        &self,
        _: &signalbox_domain::EffectRequest,
    ) -> signalbox_workflow_runtime::effects::EffectRecovery {
        signalbox_workflow_runtime::effects::EffectRecovery::Idempotent
    }
    fn adopt<'a>(
        &'a mut self,
        invocation: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        self.adoptions += 1;
        Box::pin(async move {
            let receipt: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT payload FROM native_effect_receipt WHERE run = $1 AND ordinal = $2",
            )
            .bind(invocation.run.into_uuid())
            .bind(invocation.ordinal.as_u64() as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))?;
            Ok(receipt.map(InlineFramePayload::new))
        })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: signalbox_workflow_runtime::effects::EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        self.executions += 1;
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO native_effect_receipt (run, ordinal, payload) VALUES ($1, $2, $3)",
            )
            .bind(invocation.run.into_uuid())
            .bind(invocation.ordinal.as_u64() as i64)
            .bind(invocation.request.payload().as_bytes())
            .execute(&self.pool)
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))?;
            Err(LiveDeliveryFailure::new("lost answer after effect commit"))
        })
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_lost_answer_adoption_returns_the_committed_receipt_without_reexecution()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    sqlx::query("CREATE TABLE native_effect_receipt (run uuid, ordinal bigint, payload bytea NOT NULL, PRIMARY KEY (run, ordinal))").execute(&pool).await?;
    let (host, run) = native_fixture::<ReceiptProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Judge]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let mut effects = LostAnswerEffect {
        pool: pool.clone(),
        executions: 0,
        adoptions: 0,
    };
    let mut clock = ScriptedDeliveries::new([]);
    assert!(matches!(
        host.execute_registered(run, &mut clock, &mut effects).await,
        Err(WorkflowHostError::LiveDelivery(_))
    ));
    let journal = ProgramJournalRepository::new(pool.clone());
    assert_eq!(
        journal
            .load(run)
            .await?
            .expect("unanswered effect")
            .entries()
            .len(),
        1
    );
    let outcome = host
        .execute_registered(run, &mut clock, &mut effects)
        .await?;
    assert_eq!(
        outcome,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(NATIVE_INPUT.encode()?))
    );
    assert_eq!(effects.executions, 1);
    assert_eq!(effects.adoptions, 1);
    assert!(clock.observed_outstanding.is_empty());
    assert_eq!(
        WorkflowHost::new(journal)
            .execute_registered(run, &mut clock, &mut effects)
            .await?,
        outcome
    );
    assert_eq!(effects.adoptions, 1);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_catalog_rebinding_cannot_execute_a_retained_run_as_another_program()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (original_host, run) = native_fixture::<ClockProgram>(
        &pool,
        ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    drop(original_host);
    let mut replacement = NativeCatalog::new()?;
    assert!(
        replacement
            .insert::<ReceiptProgram>(std::any::type_name::<ClockProgram>().into(), "one".into())
            .is_err(),
        "dropping a catalog must not allow its key to bind a different implementation"
    );
    let host = WorkflowHost::new(ProgramJournalRepository::new(pool.clone()))
        .with_native_catalog(replacement);
    let mut no_live = ScriptedDeliveries::new([]);
    assert!(matches!(
        host.execute_registered(run, &mut no_live, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Faulted(ProgramFault::ContractRetired(_))
    ));
    assert!(no_live.observed_outstanding.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn native_registration_adopts_equal_retries_and_refuses_changed_executable()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_host, run) =
        native_fixture::<ClockProgram>(&pool, ProgramGrants::new([]), &NATIVE_INPUT.encode()?)
            .await?;
    let repository =
        signalbox_persistence::program_registration::ProgramRegistrationRepository::new(
            pool.clone(),
        );
    let original = repository.for_run(run).await?.expect("native registration");
    let ProgramExecutable::Native {
        entry,
        revision: native_revision,
        binary_digest,
    } = original.content.executable.clone()
    else {
        panic!("native registration")
    };
    let request = NativeProgramRegistrationRequest {
        name: original.content.name.clone(),
        revision: original.content.revision.clone(),
        entry,
        native_revision,
        binary_digest,
        grants: original.content.grants.clone(),
    };
    assert_eq!(
        repository
            .register_native_user(original.id, request.clone())
            .await?,
        original
    );
    let mut changed = request;
    changed.native_revision = "different-revision".into();
    assert!(matches!(repository.register_native_user(original.id, changed).await, Err(signalbox_persistence::program_registration::ProgramRegistrationError::RegistrationConflict { .. })));
    assert_eq!(repository.for_run(run).await?, Some(original));
    let invalid = sqlx::query("INSERT INTO program_registration (registration_id, name, revision, executable_kind, artifact, native_entry, native_revision, binary_digest, grants) SELECT $1, 'mixed', revision, executable_kind, '', native_entry, native_revision, binary_digest, grants FROM program_registration")
        .bind(Uuid::now_v7()).execute(&pool).await.expect_err("native and JavaScript columns cannot coexist");
    assert_eq!(
        invalid
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("program_registration_executable_shape")
    );
    pool.close().await;
    Ok(())
}

#[path = "workflow_host_postgres/durable_primitives.rs"]
mod durable_primitives;

struct SuspendedWait;
impl LiveDeliverySource for SuspendedWait {
    fn suspend_on_wait(&self, outstanding: &[RequestFrame]) -> bool {
        outstanding
            .iter()
            .all(|frame| matches!(frame.kind(), RequestKind::Sleep(_)))
    }
    fn next_delivery<'a>(
        &'a mut self,
        _: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        panic!("a suspended host must release program state before waiting")
    }
}

// Only NativeWaitProgram owns these guards; this fixture runs in one test.
static NATIVE_WAIT_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct NativeWaitGuard;
impl Drop for NativeWaitGuard {
    fn drop(&mut self) {
        NATIVE_WAIT_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
struct NativeWaitProgram;
impl NativeProgram for NativeWaitProgram {
    type Input = NativeNumber;
    type Output = NativeNumber;
    async fn run(
        mut context: WorkflowContext,
        input: NativeNumber,
    ) -> Result<NativeNumber, NativeProgramError> {
        let _guard = NativeWaitGuard;
        context
            .sleep(
                signalbox_domain::program_primitives::SleepUntil(
                    signalbox_domain::program_primitives::UnixMillis(input.0),
                )
                .encode(),
            )
            .await?;
        Ok(input)
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_native_wait_drops_program_memory_before_reconstructing_from_the_journal()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProgramCapability, program_primitives::UnixMillis};
    let (_database, pool) = migrated_postgres().await?;
    let (host, run) = native_fixture::<NativeWaitProgram>(
        &pool,
        ProgramGrants::new([ProgramCapability::Sleep]),
        &NATIVE_INPUT.encode()?,
    )
    .await?;
    let outcome = host
        .execute_registered(run, &mut SuspendedWait, &mut no_native_effects())
        .await?;
    let ProgramExecutionOutcome::Suspended(outstanding) = outcome else {
        panic!("durable wait suspends");
    };
    assert_eq!(
        NATIVE_WAIT_DROPS.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(outstanding.len(), 1);
    let journal = ProgramJournalRepository::new(pool.clone());
    journal
        .append_delivery(
            run,
            DeliveryKind::Wake {
                resolves: outstanding[0].ordinal(),
                payload: UnixMillis(NATIVE_INPUT.0).encode(),
            },
        )
        .await?;
    assert_eq!(
        host.execute_registered(run, &mut SuspendedWait, &mut no_native_effects())
            .await?,
        ProgramExecutionOutcome::Completed(InlineFramePayload::new(NATIVE_INPUT.encode()?))
    );
    assert_eq!(
        NATIVE_WAIT_DROPS.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(journal.load(run).await?.unwrap().entries().len(), 4);
    pool.close().await;
    Ok(())
}
