#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use super::migrated_postgres;
use signalbox_application::{
    CommissionDispatchRequest, CommissionedDispatchFence, UuidV7CommissionedDispatchIdGenerator,
};
use signalbox_domain::{
    BranchName, CommitSha, DangerousToolAutoApproval, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, GoalStatement, GoalUserAction, GoalUserCommand,
    ModelSelectionRequest, PullRequestNumber, RepositorySlug, SessionConfigurationDefaults,
    SessionId, SessionSystemPrompt, SessionTemplateContentDigest, SessionTemplateName,
    SessionTemplateProvenance, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    commissioned_dispatch::{CommissionDispatchOutcome, PostgresCommissionedDispatchStore},
    convergence_sweep::{
        ConvergenceSweepDecision, ConvergenceSweepFailureDisposition, ConvergenceSweepFailureKind,
        ConvergenceSweepObservation, PostgresConvergenceSweepStore,
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
            RETRY_DELAY_SECONDS,
        )
        .await?;
    let second = store
        .record_failure(
            Uuid::from_u128(2),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            RETRY_DELAY_SECONDS,
        )
        .await?;
    let third = store
        .record_failure(
            Uuid::from_u128(3),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            RETRY_DELAY_SECONDS,
        )
        .await?;
    let fourth = store
        .record_failure(
            Uuid::from_u128(4),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            RETRY_DELAY_SECONDS,
        )
        .await?;
    let fifth = store
        .record_failure(
            Uuid::from_u128(5),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            RETRY_DELAY_SECONDS,
        )
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
            RETRY_DELAY_SECONDS,
        )
        .await?;
    store
        .record_failure(
            Uuid::from_u128(9),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::FactsFetch,
            RETRY_DELAY_SECONDS,
        )
        .await?;
    store
        .record_failure(
            Uuid::from_u128(10),
            &repository,
            pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::CommissionRefused,
            RETRY_DELAY_SECONDS,
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
    assert!(matches!(stopped, GoalCommandHandlingOutcome::Recorded(_)));

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
async fn a_committed_pending_dispatch_is_available_for_projection_repair()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let commissioned = PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin());
    let sweep = PostgresConvergenceSweepStore::new(pool);
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
async fn no_model_activity_parks_immediately_with_its_typed_need() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool.clone());
    let repository = repository()?;
    let observation = observation()?;

    let disposition = store
        .record_failure(
            Uuid::from_u128(7),
            &repository,
            inactive_pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::NoModelActivity,
            RETRY_DELAY_SECONDS,
        )
        .await?;
    let parked: (String, String) = sqlx::query_as(
        "SELECT failure_kind, operator_need
           FROM convergence_sweep_parked_target
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(rust_decimal::Decimal::from(inactive_pull_request().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(disposition, ConvergenceSweepFailureDisposition::Parked);
    assert_eq!(
        parked,
        (
            String::from("no_model_activity"),
            String::from("inspect_inactive_session")
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn configured_target_reenrollment_clears_a_durable_park() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let store = PostgresConvergenceSweepStore::new(pool);
    let repository = repository()?;
    let observation = observation()?;

    store
        .record_failure(
            Uuid::from_u128(0x89_215),
            &repository,
            inactive_pull_request(),
            Some(&observation),
            ConvergenceSweepFailureKind::NoModelActivity,
            RETRY_DELAY_SECONDS,
        )
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
