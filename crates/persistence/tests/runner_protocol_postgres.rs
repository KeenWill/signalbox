#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    CredentialProfileName, CredentialProfilePolicy, CredentialToolApproval, RunnerAdvertisement,
    RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog, RunnerEnrollment,
    RunnerEnrollmentId, RunnerGeneration, RunnerId, RunnerLeaseId, RunnerSelector,
    RunnerToolDeclaration, RunnerToolEffectClass, RunnerWorkingDirectory, SessionId,
    SessionRunnerPlacement, SessionRunnerPlacementRequest, ToolAdmissibleLoci, ToolAttemptId,
    ToolName, ToolPermissionDefault, WorkingDirectorySelection, WorkspaceCapability,
    WorkspaceRequirement,
};
use signalbox_persistence::{
    local_test_connection_options, migrate,
    runner_protocol::{RunnerProtocolStore, RunnerProtocolStoreError},
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test";
const DATABASE_NAME: &str = "signalbox";
const ENROLLMENT: u128 = 0x9100;
const RUNNER: u128 = 0x9200;
const AUTHENTICATION: u128 = 0x9300;
const SESSION: u128 = 0x9400;
const LEASE: u128 = 0x9500;
const ATTEMPT: u128 = 0x9600;
const RETRY_ATTEMPT: u128 = 0x9601;
const FOREIGN_RUNNER: u128 = 0x9201;
const RELATED_IDENTITY_OFFSET: u128 = 0x100;

#[derive(Clone, Copy)]
struct PhysicalAttemptFacts {
    attempt: u128,
    request: u128,
    turn: u128,
}

const INITIAL_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: ATTEMPT,
    request: 0x9700,
    turn: 0x9800,
};
const RETRY_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: RETRY_ATTEMPT,
    request: 0x9701,
    turn: 0x9801,
};

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_db_name(DATABASE_NAME)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn class() -> RunnerCapabilityClass {
    RunnerCapabilityClass::try_new("linux.workspace".to_owned())
        .expect("the fixture capability class is valid")
}

fn tool(name: &str) -> ToolName {
    ToolName::try_new(name.to_owned()).expect("the fixture tool name is valid")
}

fn profile() -> CredentialProfileName {
    CredentialProfileName::try_new("readonly".to_owned())
        .expect("the fixture profile name is valid")
}

fn enrollment() -> RunnerEnrollment {
    RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(ENROLLMENT)),
        RunnerId::from_uuid(uuid(RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(AUTHENTICATION)),
        [class()],
    )
}

fn catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [(tool("inspect"), CredentialToolApproval::Automatic)],
    )
    .expect("the fixture profile references its declared tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect],
        [policy],
        [WorkspaceCapability::WorktreePerSession],
    )
    .expect("the fixture catalog is internally consistent")
}

fn advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile()],
        [WorkspaceCapability::WorktreePerSession],
    )
}

async fn insert_session(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, 'owner_initiated', 'none')",
    )
    .bind(uuid(SESSION))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE session ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_physical_attempt(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'inspect', 'json', '{}')",
    )
    .bind(uuid(facts.request))
    .bind(uuid(SESSION))
    .bind(uuid(facts.turn))
    .bind(uuid(facts.request + RELATED_IDENTITY_OFFSET))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind)
         VALUES ($1, $2, $3, $4, $5, 'effect_free', 1, 'prepared')",
    )
    .bind(uuid(facts.attempt))
    .bind(uuid(facts.request))
    .bind(uuid(SESSION))
    .bind(uuid(facts.turn))
    .bind(uuid(facts.turn + RELATED_IDENTITY_OFFSET))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

#[track_caller]
fn assert_check_violation(error: sqlx::Error) {
    assert_eq!(
        error
            .as_database_error()
            .expect("PostgreSQL reports a database error")
            .code()
            .as_deref(),
        Some("23514")
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_inv042_registration_round_trips_canonical_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;

    let loaded_enrollment = store
        .load_enrollment(expected_enrollment.enrollment())
        .await?
        .expect("the inserted enrollment is present");
    let loaded_registration = store
        .load_registration(expected_enrollment.enrollment(), stored.revision())
        .await?
        .expect("the validated registration is present");

    assert_eq!(loaded_enrollment, expected_enrollment);
    assert_eq!(loaded_registration, stored);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv035_credential_relations_admit_names_and_audit_only() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let forbidden_columns: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name LIKE 'runner_%'
            AND (
                column_name LIKE '%credential_value%'
                OR column_name LIKE '%secret%'
                OR column_name IN ('value', 'payload', 'payload_json')
            )",
    )
    .fetch_one(&pool)
    .await?;
    let credential_tables: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT table_name)
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name IN (
                'runner_registration_profile',
                'runner_registration_profile_approval',
                'runner_credential_grant',
                'runner_credential_grant_tool',
                'runner_credential_grant_audit'
            )
            AND column_name = 'credential_profile_name'",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(forbidden_columns, 0);
    assert_eq!(credential_tables, 5);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_inv045_pinned_affinity_and_grant_round_trip() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
    };
    let placement = SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), request);
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin(
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
        )
        .expect("the validated registration pins the placement");
    store
        .store_placement(&pin.placement, Some(&registration), pin.grant.as_ref())
        .await?;

    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pinned placement is present");

    assert_eq!(loaded.placement(), &pin.placement);
    assert_eq!(loaded.grant(), pin.grant.as_ref());
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_claimed_retry_state_survives_reconstitution()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
    };
    let placement = SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), request);
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin(
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
        )
        .expect("the validated registration pins the placement");
    store
        .store_placement(&pin.placement, Some(&registration), pin.grant.as_ref())
        .await?;
    let offered = pin
        .placement
        .offer_lease(
            registration.registration(),
            pin.grant.as_ref(),
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            ToolAttemptId::from_uuid(uuid(ATTEMPT)),
            tool("inspect"),
            RunnerGeneration::one(),
        )
        .expect("the pinned placement authorizes the fixture lease");
    store.store_lease(&offered).await?;
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed.lose().expect("the claimed pure lease may be lost");
    store.store_lease(loss.lost()).await?;
    let lost = store
        .load_lease(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("the first generation is durable before the loss event");
    let retry = pin
        .placement
        .offer_retry(
            registration.registration(),
            pin.grant.as_ref(),
            loss,
            ToolAttemptId::from_uuid(uuid(RETRY_ATTEMPT)),
        )
        .expect("claimed pure work requires a fresh physical attempt");
    store.store_lease(&retry).await?;
    let reconstituted = store
        .load_lease(RunnerLeaseId::from_uuid(uuid(LEASE)), retry.generation())
        .await?
        .expect("the retry generation is durable");

    assert_eq!(
        lost.state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert_eq!(reconstituted, retry);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_relational_retry_rejects_claimed_attempt_reuse()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
    };
    let placement = SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), request);
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin(
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
        )
        .expect("the validated registration pins the placement");
    store
        .store_placement(&pin.placement, Some(&registration), pin.grant.as_ref())
        .await?;
    let offered = pin
        .placement
        .offer_lease(
            registration.registration(),
            pin.grant.as_ref(),
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            ToolAttemptId::from_uuid(uuid(ATTEMPT)),
            tool("inspect"),
            RunnerGeneration::one(),
        )
        .expect("the pinned placement authorizes the fixture lease");
    store.store_lease(&offered).await?;
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed.lose().expect("the claimed pure lease may be lost");
    store.store_lease(loss.lost()).await?;

    let error = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT lease_id, generation + 1, attempt_id, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, generation
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await
    .expect_err("claimed retry cannot reuse its physical attempt identity");

    assert_check_violation(error);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_reconstitution_rejects_cross_wired_registration() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;
    sqlx::query("ALTER TABLE runner_registration DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_registration
            SET runner_id = $3
          WHERE enrollment_id = $1 AND registration_revision = $2",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(rust_decimal::Decimal::from(stored.revision().get()))
    .bind(uuid(FOREIGN_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_registration ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let error = store
        .load_registration(expected_enrollment.enrollment(), stored.revision())
        .await
        .expect_err("cross-wired canonical identity fails closed");

    assert!(matches!(error, RunnerProtocolStoreError::Domain(_)));
    drop(pool);
    Ok(())
}
