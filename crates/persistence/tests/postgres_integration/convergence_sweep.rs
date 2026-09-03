#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    error::Error,
    num::{NonZeroU16, NonZeroU64},
    time::Duration,
};

use super::migrated_postgres;
use signalbox_application::{
    CommissionDispatchRequest, CommissionedDispatchFence, UuidV7CommissionedDispatchIdGenerator,
};
use signalbox_domain::{
    BranchName, CommitSha, DangerousToolAutoApproval, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, GoalCommandResult, GoalStatement, GoalUserAction,
    GoalUserCommand, ModelSelectionRequest, PullRequestNumber, RepositorySlug,
    SessionConfigurationDefaults, SessionId, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    commissioned_dispatch::{CommissionDispatchOutcome, PostgresCommissionedDispatchStore},
    convergence_sweep::{
        ConvergenceSweepDecision, ConvergenceSweepFailureDisposition, ConvergenceSweepFailureKind,
        ConvergenceSweepObservation, ConvergenceSweepRetryPolicy, PostgresConvergenceSweepStore,
    },
    goal::{GoalCommandHandlingOutcome, GoalRepository},
};
use sqlx::types::Uuid;

const REPOSITORY: &str = "signalbox/repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
const BASE_BRANCH: &str = "main";
const HEAD_BRANCH: &str = "agent/convergence";
const TEMPLATE: &str = "review-response";
const PULL_REQUEST: u64 = 892;
const INACTIVE_PULL_REQUEST: u64 = 893;
const UNRESOLVED_THREADS: u64 = 3;
const RETRY_DELAY_SECONDS: u64 = 60;
const RETRY_DELAY_CAP_SECONDS: u64 = 15 * 60;
/// A cap low enough to bind before the retry budget parks the target. The
/// production pair (60s base, 900s cap) never reaches its cap — the largest
/// delay it computes is the fourth, 480s — so it leaves `least(..., cap)`
/// unexercised.
const BINDING_RETRY_DELAY_CAP_SECONDS: u64 = 100;
const RETRY_BUDGET: i16 = 5;
const MODEL_SELECTION_ID: u128 = 0x89_200;
const FIRST_PENDING_COMMAND: u128 = 0x89_210;
const SECOND_PENDING_COMMAND: u128 = 0x89_211;
const THIRD_PENDING_COMMAND: u128 = 0x89_212;
const FIRST_CONTENT_DIGEST_BYTE: u8 = 17;
const SECOND_CONTENT_DIGEST_BYTE: u8 = 18;
const CREDENTIAL_REFERENCE: &str = "fixture-credential";

fn repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(REPOSITORY.to_owned())?)
}

fn pull_request() -> PullRequestNumber {
    PullRequestNumber::new(NonZeroU64::new(PULL_REQUEST).expect("fixture number is positive"))
}

fn inactive_pull_request() -> PullRequestNumber {
    PullRequestNumber::new(
        NonZeroU64::new(INACTIVE_PULL_REQUEST).expect("fixture number is positive"),
    )
}

fn observation() -> Result<ConvergenceSweepObservation, Box<dyn Error>> {
    Ok(ConvergenceSweepObservation::new(
        CommitSha::try_new(HEAD_SHA.to_owned())?,
        UNRESOLVED_THREADS,
    ))
}

fn pending_command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn content_digest(value: u8) -> [u8; 32] {
    [value; 32]
}

async fn record_zero_delay_facts_failure(
    store: &PostgresConvergenceSweepStore,
    event_id: Uuid,
    repository: &RepositorySlug,
    observation: &ConvergenceSweepObservation,
) -> Result<(), Box<dyn Error>> {
    store
        .record_failure(
            event_id,
            repository,
            inactive_pull_request(),
            Some(observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::ZERO),
                backoff_cap: Some(Duration::ZERO),
            },
        )
        .await?;
    Ok(())
}

/// Records one facts-fetch failure under the capped retry policy and returns
/// the disposition the store chose for it.
///
/// The cap only binds from the second delay on, so the saturation test needs
/// several attempts; naming the transition keeps that test body straight-line,
/// so a disposition that comes back wrong is reported at the call site of the
/// retry ordinal that produced it rather than at one shared loop.
async fn record_capped_backoff_attempt(
    store: &PostgresConvergenceSweepStore,
    event_id: Uuid,
    repository: &RepositorySlug,
    observation: &ConvergenceSweepObservation,
    policy: ConvergenceSweepRetryPolicy,
) -> Result<ConvergenceSweepFailureDisposition, Box<dyn Error>> {
    Ok(store
        .record_failure(
            event_id,
            repository,
            pull_request(),
            Some(observation),
            ConvergenceSweepFailureKind::FactsFetch,
            policy,
        )
        .await?)
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-family",
        CREDENTIAL_REFERENCE,
    )])
    .expect("fixture credential pin is valid")
}

fn template() -> Result<(SessionTemplateProvenance, SessionConfigurationDefaults), Box<dyn Error>> {
    Ok((
        SessionTemplateProvenance::new(
            SessionTemplateName::try_new(TEMPLATE.to_owned())?,
            SessionTemplateContentDigest::from_bytes([7; 32]),
        ),
        SessionConfigurationDefaults::complete(
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                MODEL_SELECTION_ID,
            ))),
            DangerousToolAutoApproval::Disabled,
            Some(SessionSystemPrompt::try_new(
                "Respond to review findings.".to_owned(),
            )?),
        ),
    ))
}

fn prepared_commission(
    command: u128,
) -> Result<signalbox_application::PreparedCommissionedDispatch, Box<dyn Error>> {
    let request = CommissionDispatchRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        CommissionedDispatchFence::PullRequest {
            repository: repository()?,
            pull_request: pull_request(),
            head_sha: CommitSha::try_new(HEAD_SHA.to_owned())?,
            head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
            head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        },
        GoalStatement::try_new("Converge the pull request.".to_owned())?,
        UserContent::try_text("Respond to the review.".to_owned())
            .expect("fixture content is admitted"),
    )?;
    let (provenance, defaults) = template()?;
    Ok(request.prepare(
        &mut UuidV7CommissionedDispatchIdGenerator,
        provenance,
        defaults,
    )?)
}

fn dispatched(outcome: CommissionDispatchOutcome) -> (Uuid, SessionId) {
    match outcome {
        CommissionDispatchOutcome::Dispatched { dispatch, session } => {
            (dispatch.into_uuid(), session)
        }
        other => panic!("fresh fixture must dispatch: {other:?}"),
    }
}

fn dispatched_and_busy(
    first: CommissionDispatchOutcome,
    second: CommissionDispatchOutcome,
) -> (SessionId, SessionId) {
    match (first, second) {
        (
            CommissionDispatchOutcome::Dispatched { session, .. },
            CommissionDispatchOutcome::TargetBusy {
                session: busy_session,
            },
        )
        | (
            CommissionDispatchOutcome::TargetBusy {
                session: busy_session,
            },
            CommissionDispatchOutcome::Dispatched { session, .. },
        ) => (session, busy_session),
        outcomes => panic!("one racing commission must dispatch and one must skip: {outcomes:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_commission_reuses_only_the_same_observation_and_content()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;

    let installed = store
        .begin_commission(
            &repository,
            pull_request(),
            &observation,
            content_digest(FIRST_CONTENT_DIGEST_BYTE),
            pending_command(FIRST_PENDING_COMMAND),
        )
        .await?;
    let reused = store
        .begin_commission(
            &repository,
            pull_request(),
            &observation,
            content_digest(FIRST_CONTENT_DIGEST_BYTE),
            pending_command(SECOND_PENDING_COMMAND),
        )
        .await?;
    let replaced_for_content = store
        .begin_commission(
            &repository,
            pull_request(),
            &observation,
            content_digest(SECOND_CONTENT_DIGEST_BYTE),
            pending_command(SECOND_PENDING_COMMAND),
        )
        .await?;
    let moved_observation = ConvergenceSweepObservation::new(
        observation.head_sha().clone(),
        observation.unresolved_threads() + 1,
    );
    let replaced_for_observation = store
        .begin_commission(
            &repository,
            pull_request(),
            &moved_observation,
            content_digest(SECOND_CONTENT_DIGEST_BYTE),
            pending_command(THIRD_PENDING_COMMAND),
        )
        .await?;

    assert_eq!(installed, pending_command(FIRST_PENDING_COMMAND));
    assert_eq!(reused, pending_command(FIRST_PENDING_COMMAND));
    assert_eq!(
        replaced_for_content,
        pending_command(SECOND_PENDING_COMMAND)
    );
    assert_eq!(
        replaced_for_observation,
        pending_command(THIRD_PENDING_COMMAND)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn transient_failures_retry_then_park_with_an_operator_need() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;

    let first = store
        .record_failure(
            Uuid::from_u128(1),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let second = store
        .record_failure(
            Uuid::from_u128(2),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let third = store
        .record_failure(
            Uuid::from_u128(3),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let fourth = store
        .record_failure(
            Uuid::from_u128(4),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let fifth = store
        .record_failure(
            Uuid::from_u128(5),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let retry_delays: Vec<i64> = sqlx::query_scalar(
        "SELECT round(EXTRACT(EPOCH FROM (retry_not_before - recorded_at)))::bigint
           FROM convergence_sweep_event
          WHERE repository = $1 AND pull_request_number = $2
            AND retry_not_before IS NOT NULL
          ORDER BY consecutive_failures",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_all(&pool)
    .await?;
    let parked: (String, i16, String) = sqlx::query_as("SELECT failure_kind, consecutive_failures, operator_need FROM convergence_sweep_parked_target WHERE repository = $1 AND pull_request_number = $2")
        .bind(repository.as_str())
        .bind(rust_decimal::Decimal::from(pull_request().get()))
        .fetch_one(&pool)
        .await?;

    assert_eq!(first, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(second, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(third, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(fourth, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(fifth, ConvergenceSweepFailureDisposition::Parked);
    assert_eq!(retry_delays, vec![60, 120, 240, 480]);
    assert_eq!(
        parked,
        (
            String::from("facts_fetch"),
            RETRY_BUDGET,
            String::from("repair_facts_fetch")
        )
    );
    Ok(())
}

/// A changed deployment budget applies after, not inside, a retry lineage.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn active_retry_lineage_retains_its_opening_budget() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let observation = observation()?;
    let initial = PostgresConvergenceSweepStore::new(pool.clone());
    let lowered =
        PostgresConvergenceSweepStore::new(pool.clone()).with_retry_budget(NonZeroU16::MIN);
    let policy = ConvergenceSweepRetryPolicy {
        backoff_base: Some(Duration::ZERO),
        backoff_cap: Some(Duration::ZERO),
    };

    let first = initial
        .record_failure(
            Uuid::from_u128(0x5b_001),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            policy,
        )
        .await?;
    let second = lowered
        .record_failure(
            Uuid::from_u128(0x5b_002),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            policy,
        )
        .await?;
    let state: (i16, i16) = sqlx::query_as(
        "SELECT consecutive_failures, retry_budget
           FROM convergence_sweep_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(first, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(second, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(state, (2, RETRY_BUDGET));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unbounded_retry_delay_has_no_claimable_deadline() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;

    let disposition = store
        .record_failure(
            Uuid::from_u128(6),
            &repository,
            pull_request(),
            Some(&observation()?),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: None,
                backoff_cap: None,
            },
        )
        .await?;
    let (state_kind, retry_is_infinite): (String, bool) = sqlx::query_as(
        "SELECT state_kind, retry_not_before = 'infinity'::timestamptz
           FROM convergence_sweep_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        disposition,
        ConvergenceSweepFailureDisposition::RetryScheduled
    );
    assert_eq!(state_kind, "retry_wait");
    assert!(retry_is_infinite);
    assert!(
        !store
            .load_target(&repository, pull_request())
            .await?
            .expect("the retrying target is durable")
            .retry_ready()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retry_backoff_saturates_at_the_configured_cap() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;
    let policy = ConvergenceSweepRetryPolicy {
        backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
        backoff_cap: Some(Duration::from_secs(BINDING_RETRY_DELAY_CAP_SECONDS)),
    };

    let first = record_capped_backoff_attempt(
        &store,
        Uuid::from_u128(1),
        &repository,
        &observation,
        policy,
    )
    .await?;
    let second = record_capped_backoff_attempt(
        &store,
        Uuid::from_u128(2),
        &repository,
        &observation,
        policy,
    )
    .await?;
    let third = record_capped_backoff_attempt(
        &store,
        Uuid::from_u128(3),
        &repository,
        &observation,
        policy,
    )
    .await?;
    let fourth = record_capped_backoff_attempt(
        &store,
        Uuid::from_u128(4),
        &repository,
        &observation,
        policy,
    )
    .await?;
    assert_eq!(first, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(second, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(third, ConvergenceSweepFailureDisposition::RetryScheduled);
    assert_eq!(fourth, ConvergenceSweepFailureDisposition::RetryScheduled);

    let retry_delays: Vec<i64> = sqlx::query_scalar(
        "SELECT round(EXTRACT(EPOCH FROM (retry_not_before - recorded_at)))::bigint
           FROM convergence_sweep_event
          WHERE repository = $1 AND pull_request_number = $2
            AND retry_not_before IS NOT NULL
          ORDER BY consecutive_failures",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_all(&pool)
    .await?;

    // Unbounded doubling would be 60, 120, 240, 480; the cap binds from the
    // second delay on. Removing `least(..., cap)` from the statement fails here.
    assert_eq!(
        retry_delays,
        vec![
            i64::try_from(RETRY_DELAY_SECONDS)?,
            i64::try_from(BINDING_RETRY_DELAY_CAP_SECONDS)?,
            i64::try_from(BINDING_RETRY_DELAY_CAP_SECONDS)?,
            i64::try_from(BINDING_RETRY_DELAY_CAP_SECONDS)?,
        ]
    );

    // The runtime gates retries on `retry_ready()`, not on the raw column: a
    // target whose backoff has not elapsed must read back as not ready.
    let state = store
        .load_target(&repository, pull_request())
        .await?
        .expect("the recorded failures enrolled the target");
    assert!(!state.is_parked());
    assert!(!state.retry_ready());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_different_failure_kind_starts_an_independent_lineage() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;

    store
        .record_failure(
            Uuid::from_u128(8),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    store
        .record_failure(
            Uuid::from_u128(9),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    store
        .record_failure(
            Uuid::from_u128(10),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::CommissionRefused,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await?;
    let state = store
        .load_target(&repository, pull_request())
        .await?
        .expect("the failed target is durable");

    assert_eq!(
        state.failure_kind(),
        Some(ConvergenceSweepFailureKind::CommissionRefused)
    );
    assert_eq!(state.consecutive_failures(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn racing_pull_request_commissions_skip_the_second_live_session() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresCommissionedDispatchStore::new(pool, credential_pin());
    let first = prepared_commission(0x89_201)?;
    let second = prepared_commission(0x89_202)?;

    let (first, second) = tokio::join!(
        store.commission(first, |_| None),
        store.commission(second, |_| None),
    );
    let (dispatched, busy) = dispatched_and_busy(first?, second?);

    assert_eq!(busy, dispatched);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn locked_admission_rejects_a_recent_terminal_dispatch_during_cool_off()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let (_, first_session) = dispatched(
        store
            .commission(prepared_commission(0x89_205)?, |_| None)
            .await?,
    );
    let stopped = GoalRepository::new(pool)
        .handle_user_command(
            GoalUserCommand::new(
                pending_command(0x89_214),
                first_session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;
    assert!(matches!(
        stopped,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_))
    ));

    let second = store
        .commission_after_cool_off(
            prepared_commission(0x89_206)?,
            Duration::from_secs(60),
            |_| None,
        )
        .await?;

    assert_eq!(
        second,
        CommissionDispatchOutcome::TargetCoolingOff {
            session: first_session
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn target_cool_off_uses_the_database_clock() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let sweep = PostgresConvergenceSweepStore::new(pool.clone());
    let _ = dispatched(
        commissioned
            .commission(prepared_commission(0x89_207)?, |_| None)
            .await?,
    );

    let recent = sweep
        .load_target_with_cool_off(&repository()?, pull_request(), Duration::from_secs(1))
        .await?
        .expect("loading enrolls the target");
    assert!(!recent.cool_off_elapsed());

    sqlx::query("SELECT pg_sleep(1.1)").execute(&pool).await?;
    let elapsed = sweep
        .load_target_with_cool_off(&repository()?, pull_request(), Duration::from_secs(1))
        .await?
        .expect("the target remains enrolled");

    assert!(elapsed.cool_off_elapsed());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_new_target_censuses_an_existing_commissioned_dispatch() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let sweep = PostgresConvergenceSweepStore::new(pool);
    let outcome = commissioned
        .commission(prepared_commission(0x89_203)?, |_| None)
        .await?;
    let (_, commissioned_session) = dispatched(outcome);
    let observed_after_dispatch = observation()?;
    sweep
        .record_decision(
            Uuid::from_u128(0x89_213),
            &repository()?,
            pull_request(),
            &observed_after_dispatch,
            ConvergenceSweepDecision::LiveSession,
        )
        .await?;

    let state = sweep
        .load_target(&repository()?, pull_request())
        .await?
        .expect("loading enrolls a configured target");

    assert_eq!(
        state
            .latest_dispatch()
            .expect("existing commissioned dispatch is visible")
            .session_id(),
        commissioned_session
    );
    assert_eq!(
        state.latest_dispatch_observation(),
        None,
        "a later sweep observation must not be treated as an external dispatch baseline"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn first_census_observation_becomes_the_external_dispatch_baseline()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let sweep = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;
    let (dispatch, session) = dispatched(
        commissioned
            .commission(prepared_commission(0x89_220)?, |_| None)
            .await?,
    );

    sweep
        .record_dispatch_decision(
            Uuid::from_u128(0x89_221),
            &repository,
            pull_request(),
            &observation,
            (dispatch, session),
            ConvergenceSweepDecision::LiveSession,
        )
        .await?;
    let state = sweep
        .load_target(&repository, pull_request())
        .await?
        .expect("the target remains enrolled");

    assert_eq!(state.latest_dispatch_observation(), Some(&observation));
    assert_eq!(
        state
            .latest_dispatch()
            .expect("the external dispatch remains selected")
            .session_id(),
        session
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_committed_pending_dispatch_is_available_for_projection_repair()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let sweep = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;
    let command = 0x89_204;
    sweep
        .begin_commission(
            &repository,
            pull_request(),
            &observation,
            content_digest(FIRST_CONTENT_DIGEST_BYTE),
            pending_command(command),
        )
        .await?;
    let outcome = commissioned
        .commission(prepared_commission(command)?, |_| None)
        .await?;
    let (dispatch, session) = dispatched(outcome);
    let commissioned_at: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT recorded_at FROM commissioned_dispatch WHERE dispatch_id = $1")
            .bind(dispatch)
            .fetch_one(&pool)
            .await?;

    let state = sweep
        .load_target(&repository, pull_request())
        .await?
        .expect("pending target remains durable");
    let pending = state
        .pending_dispatch()
        .expect("committed dispatch is linked to its pending command");

    assert_eq!(pending.dispatch_id(), dispatch);
    assert_eq!(pending.session_id(), session);
    assert_eq!(state.pending_observation(), Some(&observation));

    sqlx::query("SELECT pg_sleep(0.01)").execute(&pool).await?;
    sweep
        .record_dispatch(
            Uuid::from_u128(0x89_214),
            &repository,
            pull_request(),
            &observation,
            dispatch,
            session,
        )
        .await?;
    let projected_at: sqlx::types::time::OffsetDateTime = sqlx::query_scalar(
        "SELECT last_dispatched_at
           FROM convergence_sweep_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(projected_at, commissioned_at);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pull_request_dispatch_census_has_its_target_ordering_index() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'commissioned_dispatch_pull_request_target'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(definition.contains(
        "(target_kind, repository, pull_request_number, recorded_at DESC, dispatch_id DESC)"
    ));
    assert!(definition.contains("target_kind = 'pull_request'"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_watch_dispatch_census_has_its_target_indexes() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let event_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'repo_watch_event_pull_request_target'",
    )
    .fetch_one(&pool)
    .await?;
    let action_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'repo_watch_dispatch_action_event_target'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(event_definition.contains("(repository, pull_request_number, event_id)"));
    assert!(event_definition.contains("target_kind = 'pull_request'"));
    assert!(
        action_definition
            .contains("(event_id, recorded_at DESC, dispatch_id DESC, session_id DESC)")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn generic_failure_recording_rejects_no_model_activity() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;

    let error = store
        .record_failure(
            Uuid::from_u128(7),
            &repository,
            inactive_pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::NoModelActivity,
            ConvergenceSweepRetryPolicy {
                backoff_base: Some(Duration::from_secs(RETRY_DELAY_SECONDS)),
                backoff_cap: Some(Duration::from_secs(RETRY_DELAY_CAP_SECONDS)),
            },
        )
        .await
        .expect_err("generic recording must reject inactivity failures");
    let target_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM convergence_sweep_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(inactive_pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert!(error.to_string().contains("expected session"));
    assert_eq!(target_count, 0, "rejection must precede durable mutation");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn configured_target_reenrollment_clears_a_durable_park() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;

    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_215), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_216), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_217), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_218), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_219), &repository, &observation)
        .await?;
    store
        .reenroll_target(&repository, inactive_pull_request())
        .await?;
    let state = store
        .load_target(&repository, inactive_pull_request())
        .await?
        .expect("re-enrolled target remains durable");

    assert!(!state.is_parked());
    assert_eq!(state.failure_kind(), None);
    assert_eq!(state.consecutive_failures(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_session_without_model_activity_is_parked() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;
    let (dispatch, session) = dispatched(
        commissioned
            .commission(prepared_commission(0x89_208)?, |_| None)
            .await?,
    );
    store
        .record_dispatch_decision(
            Uuid::from_u128(0x89_216),
            &repository,
            pull_request(),
            &observation,
            (dispatch, session),
            ConvergenceSweepDecision::LiveSession,
        )
        .await?;

    let disposition = store
        .record_no_model_activity_failure(
            Uuid::from_u128(0x89_217),
            &repository,
            pull_request(),
            &observation,
            session,
        )
        .await?;
    let parked_identity: (Uuid, Uuid, sqlx::types::time::OffsetDateTime) = sqlx::query_as(
        "SELECT last_dispatch_id, last_session_id, last_dispatched_at
           FROM convergence_sweep_parked_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE convergence_sweep_target
            SET census_dispatch_id = $3, census_session_id = $4
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .bind(Uuid::from_u128(0x89_2f0))
    .bind(Uuid::from_u128(0x89_2f1))
    .execute(&pool)
    .await?;
    let retained_identity: (Uuid, Uuid, sqlx::types::time::OffsetDateTime) = sqlx::query_as(
        "SELECT last_dispatch_id, last_session_id, last_dispatched_at
           FROM convergence_sweep_parked_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(disposition, ConvergenceSweepFailureDisposition::Parked);
    assert_eq!(parked_identity.0, dispatch);
    assert_eq!(parked_identity.1, session.into_uuid());
    assert_eq!(retained_identity, parked_identity);
    assert!(
        store
            .load_target(&repository, pull_request())
            .await?
            .is_some_and(|state| state.is_parked())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_inactive_session_cannot_park_a_newer_dispatch() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;
    let (_, stale_session) = dispatched(
        commissioned
            .commission(prepared_commission(0x89_230)?, |_| None)
            .await?,
    );
    let stopped = GoalRepository::new(pool)
        .handle_user_command(
            GoalUserCommand::new(
                pending_command(0x89_231),
                stale_session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;
    let (_, latest_session) = dispatched(
        commissioned
            .commission(prepared_commission(0x89_232)?, |_| None)
            .await?,
    );

    let disposition = store
        .record_no_model_activity_failure(
            Uuid::from_u128(0x89_233),
            &repository,
            pull_request(),
            &observation,
            stale_session,
        )
        .await?;
    let state = store
        .load_target(&repository, pull_request())
        .await?
        .expect("the target remains enrolled");

    assert!(matches!(
        stopped,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_))
    ));
    assert_ne!(latest_session, stale_session);
    assert_eq!(
        disposition,
        ConvergenceSweepFailureDisposition::ActivityObserved
    );
    assert!(!state.is_parked());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn removed_targets_leave_the_parked_operator_view() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_218), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_219), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_21a), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_21b), &repository, &observation)
        .await?;
    record_zero_delay_facts_failure(&store, Uuid::from_u128(0x89_21c), &repository, &observation)
        .await?;

    store.reconcile_configured_targets(&[]).await?;

    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM convergence_sweep_parked_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(inactive_pull_request().get()))
    .fetch_one(&pool)
    .await?;
    let retained_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM convergence_sweep_event
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(inactive_pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(parked, 0);
    assert_eq!(retained_events, i64::from(RETRY_BUDGET));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_session_observation_is_retained_for_movement_detection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;

    store
        .record_decision(
            Uuid::from_u128(6),
            &repository,
            pull_request(),
            &observation,
            ConvergenceSweepDecision::LiveSession,
        )
        .await?;
    let state = store
        .load_target(&repository, pull_request())
        .await?
        .expect("the observed target is durable");

    assert_eq!(state.last_observation(), Some(&observation));
    Ok(())
}
