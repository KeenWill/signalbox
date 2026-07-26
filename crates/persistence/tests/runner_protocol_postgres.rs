#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, time::Duration};

use rust_decimal::Decimal;
use signalbox_domain::{
    AuthorizedToolAttempt, CredentialProfileName, CredentialProfilePolicy, CredentialToolApproval,
    ProvisionedWorkspace, ReconstitutedToolAttempt, RunnerAdvertisement, RunnerAuthenticationId,
    RunnerCapabilityClass, RunnerCatalog, RunnerDomainError, RunnerEnrollment, RunnerEnrollmentId,
    RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseId, RunnerLeaseOfferRequest,
    RunnerSelector, RunnerToolDeclaration, RunnerToolEffectClass, RunnerToolModelDefinition,
    RunnerWorkingDirectory, SessionId, SessionRunnerPin, SessionRunnerPlacement,
    SessionRunnerPlacementRequest, ToolAdmissibleLoci, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDispatchGeneration,
    ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestId, TurnAttemptId, TurnId,
    WorkingDirectorySelection, WorkspaceCapability, WorkspaceRepositoryKey, WorkspaceRequirement,
};
use signalbox_persistence::{
    local_test_connection_options, migrate,
    runner_protocol::{
        RunnerProtocolCorruption, RunnerProtocolStore, RunnerProtocolStoreError,
        StoredValidatedRunnerRegistration,
    },
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
const REPLACEMENT_ENROLLMENT: u128 = 0x9101;
const REPLACEMENT_RUNNER: u128 = 0x9201;
const REPLACEMENT_AUTHENTICATION: u128 = 0x9301;
const LATER_ENROLLMENT: u128 = 0x9102;
const LATER_RUNNER: u128 = 0x9202;
const LATER_AUTHENTICATION: u128 = 0x9302;
const SESSION: u128 = 0x9400;
const FOREIGN_SESSION: u128 = 0x9401;
const LEASE: u128 = 0x9500;
const ATTEMPT: u128 = 0x9600;
const RETRY_ATTEMPT: u128 = 0x9601;
const FOREIGN_RUNNER: u128 = 0x9202;
const RELATED_IDENTITY_OFFSET: u128 = 0x100;
const LOCK_WAIT_PROBE: Duration = Duration::from_millis(100);

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
    request: INITIAL_PHYSICAL_ATTEMPT.request,
    turn: INITIAL_PHYSICAL_ATTEMPT.turn,
};
const PROFILELESS_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9602,
    request: 0x9701,
    turn: 0x9801,
};
const LATER_LEASE_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9604,
    request: 0x9702,
    turn: 0x9802,
};
const SECOND_LATER_LEASE_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9605,
    request: 0x9703,
    turn: 0x9803,
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

fn replacement_profile() -> CredentialProfileName {
    CredentialProfileName::try_new("operator".to_owned())
        .expect("the replacement profile name is valid")
}

fn model_definition() -> RunnerToolModelDefinition {
    RunnerToolModelDefinition::try_new(
        "Inspect the fixture workspace".to_owned(),
        format!(r#"{{"{}":0}}"#, "x".repeat(4096)),
    )
    .expect("the fixture model definition is valid")
}

fn authorized(facts: PhysicalAttemptFacts) -> AuthorizedToolAttempt {
    let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session: SessionId::from_uuid(uuid(SESSION)),
            turn: TurnId::from_uuid(uuid(facts.turn)),
            issuing_attempt: TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
            request: ToolRequestId::from_uuid(uuid(facts.request)),
            attempt: ToolAttemptId::from_uuid(uuid(facts.attempt)),
            generation: ToolDispatchGeneration::first(),
        },
    );
    let attempt = ToolAttemptReconstitutionInput::new(
        ToolAttemptId::from_uuid(uuid(facts.attempt)),
        ToolRequestId::from_uuid(uuid(facts.request)),
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the fixture in-flight attempt reconstitutes");
    let ReconstitutedToolAttempt::Current(attempt) = attempt else {
        panic!("the fixture attempt is current")
    };
    AuthorizedToolAttempt::reconstitute(attempt, dispatch)
        .expect("the canonical in-flight fixture authorizes")
}

fn offer_request() -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(LEASE)),
        tool: tool("inspect"),
    }
}

fn enrollment() -> RunnerEnrollment {
    RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(ENROLLMENT)),
        RunnerId::from_uuid(uuid(RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(AUTHENTICATION)),
        [class()],
    )
}

fn replacement_enrollment() -> RunnerEnrollment {
    RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(REPLACEMENT_ENROLLMENT)),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(REPLACEMENT_AUTHENTICATION)),
        [class()],
    )
}

fn catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition(),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let catalog_only = RunnerToolDeclaration::new(
        tool("catalog_only"),
        model_definition(),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [
            (tool("inspect"), CredentialToolApproval::Automatic),
            (tool("catalog_only"), CredentialToolApproval::SessionPolicy),
        ],
    )
    .expect("the fixture profile references its declared tool");
    let replacement_policy = CredentialProfilePolicy::try_new(
        replacement_profile(),
        [
            (tool("inspect"), CredentialToolApproval::SessionPolicy),
            (tool("catalog_only"), CredentialToolApproval::SessionPolicy),
        ],
    )
    .expect("the replacement profile references declared tools");
    RunnerCatalog::try_new(
        [class()],
        [inspect, catalog_only],
        [policy, replacement_policy],
        [WorkspaceCapability::WorktreePerSession],
    )
    .expect("the fixture catalog is internally consistent")
}

fn advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
    )
}

fn narrowed_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
    )
}

fn profileless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [],
        [WorkspaceCapability::WorktreePerSession],
    )
}

fn workspaceless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [],
    )
}

fn expanded_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect"), tool("catalog_only")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
    )
}

async fn stored_pin_fixture(
    pool: &PgPool,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
    ),
    Box<dyn Error>,
> {
    insert_session(pool).await?;
    insert_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
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
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration pins the placement");
    store.store_pin(&pin, &registration).await?;
    Ok((store, expected_enrollment, registration, pin))
}

async fn stored_later_lease_fixture(
    pool: &PgPool,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
        RunnerLease,
    ),
    Box<dyn Error>,
> {
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(pool).await?;
    terminalize_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("the later lease is valid before durable authority is revoked");
    Ok((store, expected_enrollment, registration, pin, lease))
}

async fn insert_session_for(pool: &PgPool, session: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, 'owner_initiated', 'none')",
    )
    .bind(session)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE session ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_session(pool: &PgPool) -> Result<(), sqlx::Error> {
    insert_session_for(pool, uuid(SESSION)).await
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
         VALUES ($1, $2, $3, $4, 0, 'inspect', 'json', '{}')
         ON CONFLICT (request_id) DO NOTHING",
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
         VALUES ($1, $2, $3, $4, $5, 'effect_free', 1, 'in_flight')",
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

async fn terminalize_physical_attempt(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'execution_failed'
          WHERE attempt_id = $1",
    )
    .bind(uuid(facts.attempt))
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

#[track_caller]
fn assert_store_check_violation(error: RunnerProtocolStoreError) {
    let RunnerProtocolStoreError::Database(error) = error else {
        panic!("PostgreSQL must reject the invalid durable evidence")
    };
    assert_check_violation(error);
}

#[track_caller]
fn assert_store_domain_error(error: RunnerProtocolStoreError, expected: RunnerDomainError) {
    let RunnerProtocolStoreError::Domain(actual) = error else {
        panic!("the adapter must reject invalid domain evidence before writing")
    };
    assert_eq!(actual, expected);
}

#[track_caller]
fn assert_store_corruption(error: RunnerProtocolStoreError, expected: RunnerProtocolCorruption) {
    let RunnerProtocolStoreError::Corruption(actual) = error else {
        panic!("the adapter must return typed corruption for malformed durable evidence")
    };
    assert_eq!(actual, expected);
}

#[track_caller]
fn assert_one_store_succeeds_and_one_conflicts(
    first: Result<(), RunnerProtocolStoreError>,
    second: Result<(), RunnerProtocolStoreError>,
) {
    match (first, second) {
        (Ok(()), Err(error)) | (Err(error), Ok(())) => assert_store_check_violation(error),
        outcomes => panic!("one attempt binding must win exactly once: {outcomes:?}"),
    }
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
    store
        .revoke_enrollment(expected_enrollment.enrollment())
        .await?;
    let historical_registration = store
        .load_registration(expected_enrollment.enrollment(), stored.revision())
        .await?
        .expect("revocation preserves historical validated registration");

    assert_eq!(loaded_enrollment, expected_enrollment);
    assert_eq!(loaded_registration, stored);
    assert_eq!(historical_registration, stored);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_orphan_revocation_audit_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id,
             authentication_reference_id, allowed_class_count, state_kind)
         SELECT enrollment_id, 2, runner_id,
                authentication_reference_id, allowed_class_count, 'revoked'
           FROM runner_enrollment
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         SELECT enrollment_id, 2, capability_class
           FROM runner_enrollment_allowed_class
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&mut *malformed)
    .await?;
    let orphan = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("a terminal audit must advance the canonical enrollment");

    assert_check_violation(orphan);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_current_registration_gates_new_leases() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            expanded_advertisement(),
            &catalog(),
        )
        .await?;
    let retained_tool_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("an additive registration retains the pinned tool");
    store.store_lease(&retained_tool_lease).await?;
    terminalize_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, SECOND_LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            narrowed_advertisement(),
            &catalog(),
        )
        .await?;
    let stale_registration_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(SECOND_LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 2)),
                tool: tool("inspect"),
            },
        )
        .expect("historical domain evidence isolates the relational current-head gate");
    let stale_registration = store
        .store_lease(&stale_registration_lease)
        .await
        .expect_err("a withdrawn current tool cannot receive a later runner lease");

    assert_store_check_violation(stale_registration);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv042_current_registration_preserves_complete_placement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(
            expected_enrollment.enrollment(),
            expanded_advertisement(),
            &catalog(),
        )
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the expanded registration pins both runner-required tools");
    store.store_pin(&pin, &registration).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, PROFILELESS_PHYSICAL_ATTEMPT).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            advertisement(),
            &catalog(),
        )
        .await?;
    let stale_snapshot_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(PROFILELESS_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("historical evidence still admits the retained offered tool");
    let stale_snapshot = store
        .store_lease(&stale_snapshot_lease)
        .await
        .expect_err("current availability must retain every runner-required pinned tool");

    assert_store_check_violation(stale_snapshot);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv042_current_registration_preserves_profile() -> Result<(), Box<dyn Error>> {
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
    let directory = RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
        .expect("the fixture working directory is valid");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            directory,
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the profile satisfies the initial pin");
    store.store_pin(&pin, &registration).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            profileless_advertisement(),
            &catalog(),
        )
        .await?;
    let profile_stale = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("historical evidence still carries the pinned profile");
    let profile_rejected = store
        .store_lease(&profile_stale)
        .await
        .expect_err("current registration must retain the pinned profile");

    assert_store_check_violation(profile_rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv042_current_registration_preserves_workspace() -> Result<(), Box<dyn Error>> {
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
    let repository = WorkspaceRepositoryKey::try_new("signalbox".to_owned())
        .expect("the repository key is valid");
    let directory = RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
        .expect("the fixture working directory is valid");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: repository.clone(),
            },
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            directory.clone(),
            Some(ProvisionedWorkspace {
                session: SessionId::from_uuid(uuid(SESSION)),
                runner: expected_enrollment.runner(),
                repository,
                working_directory: directory,
            }),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the worktree capability satisfies the initial pin");
    store.store_pin(&pin, &registration).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            workspaceless_advertisement(),
            &catalog(),
        )
        .await?;
    let workspace_stale = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("historical evidence still carries the worktree capability");
    let workspace_rejected = store
        .store_lease(&workspace_stale)
        .await
        .expect_err("current registration must retain the worktree capability");

    assert_store_check_violation(workspace_rejected);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s30_inv042_registration_replacement_serializes_later_lease_admission()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let mut blocker = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&mut *blocker)
    .await?;
    let replacement_store = RunnerProtocolStore::new(pool.clone());
    let daemon_catalog = catalog();
    let mut replacement = Box::pin(replacement_store.register(
        expected_enrollment.enrollment(),
        narrowed_advertisement(),
        &daemon_catalog,
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut replacement)
        .await
        .expect_err("registration replacement must wait for enrollment authority");
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("lease admission must wait behind registration replacement");
    blocker.commit().await?;
    replacement.await?;
    let rejected = lease_store
        .await
        .expect_err("withdrawn current availability cannot authorize the later lease");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_current_registration_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, initial, _) = stored_pin_fixture(&pool).await?;
    store
        .register(
            expected_enrollment.enrollment(),
            expanded_advertisement(),
            &catalog(),
        )
        .await?;
    let rewound_head = sqlx::query(
        "UPDATE runner_current_registration
            SET registration_revision = $2
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(initial.revision().get()))
    .execute(&pool)
    .await
    .expect_err("the registration head cannot be rewound to retained history");

    assert_check_violation(rewound_head);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_concurrent_attempt_binding_has_one_lease_lineage()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let first_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("the first lease candidate is valid in isolation");
    let second_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 2)),
                tool: tool("inspect"),
            },
        )
        .expect("the second lease candidate is valid in isolation");
    let first_store = RunnerProtocolStore::new(pool.clone());
    let second_store = RunnerProtocolStore::new(pool.clone());
    let (first, second) = tokio::join!(
        first_store.store_lease(&first_lease),
        second_store.store_lease(&second_lease)
    );

    assert_one_store_succeeds_and_one_conflicts(first, second);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_request_cannot_start_second_lease_lineage() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    let claimed = pin
        .lease
        .clone()
        .claim(pin.lease.correlation())
        .expect("the exact first lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed pure work may enter durable retry classification");
    store.store_lease(loss.lost()).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let second_lineage = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorized(RETRY_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("the public fresh-offer path cannot see durable request lineage");
    let rejected = store
        .store_lease(&second_lineage)
        .await
        .expect_err("one logical tool request belongs to one lease lineage");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_inv042_concurrent_enrollment_revocation_blocks_a_later_lease()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment().into_uuid();
    let mut revocation = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment)
    .fetch_one(&mut *revocation)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease insert must wait for enrollment authority");
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id,
             authentication_reference_id, allowed_class_count, state_kind)
         SELECT enrollment_id, 2, runner_id,
                authentication_reference_id, allowed_class_count, 'revoked'
           FROM runner_enrollment_audit
          WHERE enrollment_id = $1 AND revision = 1",
    )
    .bind(enrollment)
    .execute(&mut *revocation)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         SELECT enrollment_id, 2, capability_class
           FROM runner_enrollment_audit_allowed_class
          WHERE enrollment_id = $1 AND revision = 1",
    )
    .bind(enrollment)
    .execute(&mut *revocation)
    .await?;
    sqlx::query(
        "UPDATE runner_enrollment
            SET revision = 2, state_kind = 'revoked'
          WHERE enrollment_id = $1",
    )
    .bind(enrollment)
    .execute(&mut *revocation)
    .await?;
    revocation.commit().await?;
    let rejected = lease_store
        .await
        .expect_err("a concurrently revoked enrollment cannot authorize the lease");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_inv045_concurrent_grant_revocation_blocks_a_later_lease() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin, lease) = stored_later_lease_fixture(&pool).await?;
    let grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    let mut revocation = pool.begin().await?;
    sqlx::query(
        "SELECT credential_profile_name
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3
          FOR UPDATE",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .fetch_one(&mut *revocation)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease insert must wait for credential-grant authority");
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         VALUES ($1, $2, $3, 2, 'revoked', $4)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(grant.profile().as_str())
    .execute(&mut *revocation)
    .await?;
    revocation.commit().await?;
    let rejected = lease_store
        .await
        .expect_err("a concurrently revoked grant cannot authorize the lease");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_inv045_grant_revocation_serializes_profile_replacement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    let replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            original_grant.clone(),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the active predecessor permits profile replacement");
    let mut revocation = pool.begin().await?;
    sqlx::query(
        "SELECT credential_profile_name
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3
          FOR UPDATE",
    )
    .bind(original_grant.session().into_uuid())
    .bind(original_grant.runner().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .fetch_one(&mut *revocation)
    .await?;
    let replacement_grant = replacement.grant.grant.clone();
    let mut replacement_store = Box::pin(store.store_placement(
        &replacement.placement,
        Some(&registration),
        Some(&replacement_grant),
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut replacement_store)
        .await
        .expect_err("replacement must wait for predecessor grant authority");
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         VALUES ($1, $2, $3, 2, 'revoked', $4)",
    )
    .bind(original_grant.session().into_uuid())
    .bind(original_grant.runner().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .bind(original_grant.profile().as_str())
    .execute(&mut *revocation)
    .await?;
    revocation.commit().await?;
    let stored = replacement_store
        .await
        .expect("the serialized successor does not reactivate its revoked predecessor");
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the successor placement remains loadable");

    assert_eq!(stored.placement(), &replacement.placement);
    assert_eq!(stored.grant(), Some(&replacement_grant));
    assert_eq!(loaded.placement(), &replacement.placement);
    assert_eq!(loaded.grant(), Some(&replacement_grant));
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_replaced_grant_is_not_a_current_revocation_target() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    let replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            original_grant.clone(),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the active predecessor permits profile replacement");
    store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            Some(&replacement.grant.grant),
        )
        .await?;
    let obsolete = store
        .revoke_grant(
            original_grant.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?;

    assert_eq!(obsolete, None);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv044_first_placement_record_is_created_unpinned() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session_for(&pool, uuid(FOREIGN_SESSION)).await?;
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
    let malformed_first = sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_capability_class,
             directory_selection_kind, requested_credential_profile_name,
             workspace_requirement_kind, state_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, credential_grant_revision)
         VALUES (
             $1, 1, 1, 'runner_replaced',
             'capability_class', $2,
             'runner_default', $3,
             'none', 'pinned', $4,
             $5, $3,
             $6, $7,
             (
                 SELECT count(*)
                   FROM runner_registration_tool
                  WHERE enrollment_id = $6
                    AND registration_revision = $7
             ),
             1
         )",
    )
    .bind(uuid(FOREIGN_SESSION))
    .bind(class().as_str())
    .bind(profile().as_str())
    .bind(expected_enrollment.runner().into_uuid())
    .bind("/workspace/forged-first")
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(registration.revision().get()))
    .execute(&pool)
    .await
    .expect_err("the first placement row cannot begin as a replacement");

    assert_check_violation(malformed_first);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv044_initial_pin_requires_loadable_offered_lease() -> Result<(), Box<dyn Error>> {
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
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/profileless".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the profileless initial pin is valid");
    let correlation = pin.lease.correlation();
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id,
             selector_capability_class, directory_selection_kind,
             requested_working_directory,
             requested_credential_profile_name,
             workspace_requirement_kind, requested_repository_key,
             state_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision, 'pinned',
                selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                'pinned', $2, $3,
                NULL, $4, $5,
                (
                    SELECT count(*)
                      FROM runner_registration_tool
                     WHERE enrollment_id = $4
                       AND registration_revision = $5
                       AND tool_name = $6
                ),
                NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 1",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(pin.lease.runner().into_uuid())
    .bind("/workspace/profileless")
    .bind(registration.registration().enrollment().into_uuid())
    .bind(Decimal::from(registration.revision().get()))
    .bind(pin.lease.tool().as_str())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
            (session_id, event_ordinal, tool_name, runner_required)
         VALUES ($1, 2, $2, TRUE)",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(pin.lease.tool().as_str())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         VALUES (
             $1, 1, $2, $3, $4,
             $5, 'pure', 2,
             $6, $7,
             NULL, NULL, NULL, NULL
         )",
    )
    .bind(correlation.lease.into_uuid())
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(pin.placement.session().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(correlation.tool.as_str())
    .bind(registration.registration().enrollment().into_uuid())
    .bind(Decimal::from(registration.revision().get()))
    .execute(&mut *malformed)
    .await?;
    let missing_offer = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("a pinned placement requires its loadable offered lease and current head");

    assert_check_violation(missing_offer);
    malformed.rollback().await?;
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
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration pins the placement");
    let claimed_pin = SessionRunnerPin {
        placement: pin.placement.clone(),
        grant: pin.grant.clone(),
        lease: pin
            .lease
            .clone()
            .claim(pin.lease.correlation())
            .expect("the exact fixture correlation claims its lease"),
    };
    let non_offered_pin = store
        .store_pin(&claimed_pin, &registration)
        .await
        .expect_err("an atomic pin may store only its original offered lease");

    assert_store_domain_error(non_offered_pin, RunnerDomainError::InvalidState);
    store.store_pin(&pin, &registration).await?;

    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pinned placement is present");

    assert_eq!(loaded.placement(), &pin.placement);
    assert_eq!(loaded.registration(), Some(&registration));
    assert_eq!(loaded.grant(), pin.grant.as_ref());
    insert_physical_attempt(&pool, PROFILELESS_PHYSICAL_ATTEMPT).await?;
    let profileless_placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
        },
    );
    let profileless_pin = profileless_placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/profileless".to_owned())
                .expect("the profileless directory is valid"),
            None,
            authorized(PROFILELESS_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("the separate profileless aggregate can construct its own lease");
    let missing_current_grant = store
        .store_lease(&profileless_pin.lease)
        .await
        .expect_err("canonical profile selection requires its exact grant on every lease");

    assert_store_check_violation(missing_current_grant);
    let profile_replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            pin.grant
                .clone()
                .expect("the fixture pin carries a credential grant"),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the replacement profile is valid for the pinned runner");
    let predecessor_grant = store
        .store_placement(
            &profile_replacement.placement,
            Some(&registration),
            pin.grant.as_ref(),
        )
        .await
        .expect_err("a replacement placement cannot retain its predecessor grant");

    assert_store_domain_error(predecessor_grant, RunnerDomainError::CorruptStoredFacts);
    let same_profile_replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            pin.grant
                .clone()
                .expect("the fixture pin carries a credential grant"),
            registration.registration(),
            profile(),
            [tool("inspect")],
        )
        .expect("an explicit same-profile replacement still advances grant lineage");
    let stale_grant_revision = store
        .store_placement(
            &same_profile_replacement.placement,
            Some(&registration),
            pin.grant.as_ref(),
        )
        .await
        .expect_err("a replacement cannot retain its predecessor grant revision");

    assert_store_check_violation(stale_grant_revision);
    let lost = pin
        .placement
        .clone()
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    store
        .store_placement(&lost, Some(&registration), pin.grant.as_ref())
        .await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the credential-bearing pin has its grant");
    let revoked = store
        .revoke_grant(
            pin.placement.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?
        .expect("the active grant revokes exactly once");
    let replacement = lost
        .replace_lost_runner(
            pin.placement.request().clone(),
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            Some(revoked),
        )
        .expect("the domain records a successor grant revision");
    store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            replacement.grant.as_ref(),
        )
        .await?;
    let loaded_replacement = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the successor of a revoked grant remains loadable");

    assert_eq!(loaded_replacement.placement(), &replacement.placement);
    assert_eq!(loaded_replacement.grant(), replacement.grant.as_ref());
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_pin_grant_requires_complete_registration_inventory()
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
            expanded_advertisement(),
            &catalog(),
        )
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the expanded registration pins its complete tool inventory");
    store.store_pin(&pin, &registration).await?;
    let grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    sqlx::query(
        "ALTER TABLE runner_credential_grant
         DISABLE TRIGGER runner_credential_grant_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant_tool
         DISABLE TRIGGER runner_credential_grant_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "DELETE FROM runner_credential_grant_tool
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3
            AND tool_name = $4",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(tool("catalog_only").as_str())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE runner_credential_grant
            SET tool_count = tool_count - 1
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .execute(&mut *malformed)
    .await?;
    let incomplete = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("a pin-created grant must snapshot every registration tool");

    assert_check_violation(incomplete);
    malformed.rollback().await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant_tool
         ENABLE TRIGGER runner_credential_grant_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant
         ENABLE TRIGGER runner_credential_grant_is_append_only",
    )
    .execute(&pool)
    .await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_loaded_placement_retains_reconciliation_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, historical, pin) = stored_pin_fixture(&pool).await?;
    let current = store
        .register(
            expected_enrollment.enrollment(),
            narrowed_advertisement(),
            &catalog(),
        )
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pinned placement and historical registration reload together");
    let lost = loaded
        .placement()
        .clone()
        .reconcile_registration(current.registration())
        .expect("withdrawn runner-required availability marks the placement lost");
    store
        .store_placement(&lost, loaded.registration(), loaded.grant())
        .await?;
    let reloaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the reconciled placement remains loadable");

    assert_eq!(loaded.registration(), Some(&historical));
    assert_eq!(reloaded.placement(), &lost);
    assert_eq!(reloaded.registration(), Some(&historical));
    assert_eq!(reloaded.grant(), pin.grant.as_ref());
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_current_placement_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    store
        .store_placement(&lost, Some(&registration), pin.grant.as_ref())
        .await?;
    let rewound_head = sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal - 1
          WHERE session_id = $1",
    )
    .bind(lost.session().into_uuid())
    .execute(&pool)
    .await
    .expect_err("the placement head cannot be rewound to historical evidence");

    assert_check_violation(rewound_head);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_current_lease_event_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let claimed = pin
        .lease
        .clone()
        .claim(pin.lease.correlation())
        .expect("the exact lease fence claims");
    store.store_lease(&claimed).await?;
    let rewound_head = sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = event_ordinal - 1
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(claimed.correlation().lease.into_uuid())
    .bind(Decimal::from(claimed.generation().get()))
    .execute(&pool)
    .await
    .expect_err("the lease event head cannot be rewound to retained history");

    assert_check_violation(rewound_head);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_every_generation_requires_offered_event_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    insert_physical_attempt(&pool, PROFILELESS_PHYSICAL_ATTEMPT).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, NULL
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(pin.lease.correlation().lease.into_uuid())
    .bind(uuid(LEASE + 1))
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.attempt))
    .execute(&mut *malformed)
    .await?;
    let missing_events = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("every generation needs its offered event and current head");

    assert_check_violation(missing_events);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_new_revoked_grant_round_trips_terminal_audit() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            pin.grant
                .clone()
                .expect("the fixture pin carries a credential grant"),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the fixture profile replacement is valid");
    let revoked = replacement
        .grant
        .grant
        .revoke()
        .expect("the new grant can be revoked before persistence");
    store
        .store_placement(&replacement.placement, Some(&registration), Some(&revoked))
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the replacement placement remains loadable");

    assert_eq!(loaded.grant(), Some(&revoked));
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_grant_audit_kind_is_revision_bound() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let initial = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued grant");
    let replacement = pin
        .placement
        .clone()
        .replace_credential_profile(
            initial.clone(),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the active predecessor permits profile replacement");
    store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            Some(&replacement.grant.grant),
        )
        .await?;
    sqlx::query("ALTER TABLE runner_credential_grant_audit DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let forged_initial = sqlx::query(
        "UPDATE runner_credential_grant_audit
            SET event_kind = 'replaced'
          WHERE session_id = $1
            AND grant_revision = $2
            AND audit_ordinal = 1",
    )
    .bind(initial.session().into_uuid())
    .bind(Decimal::from(initial.revision().get()))
    .execute(&pool)
    .await
    .expect_err("grant revision one is issued, never replaced");
    let forged_successor = sqlx::query(
        "UPDATE runner_credential_grant_audit
            SET event_kind = 'issued'
          WHERE session_id = $1
            AND grant_revision = $2
            AND audit_ordinal = 1",
    )
    .bind(replacement.grant.grant.session().into_uuid())
    .bind(Decimal::from(replacement.grant.grant.revision().get()))
    .execute(&pool)
    .await
    .expect_err("a successor grant is replaced, never issued");
    sqlx::query("ALTER TABLE runner_credential_grant_audit ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    assert_check_violation(forged_initial);
    assert_check_violation(forged_successor);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_inv045_relational_placement_binds_selected_grant() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id,
             selector_capability_class, directory_selection_kind,
             requested_working_directory,
             requested_credential_profile_name,
             workspace_requirement_kind, requested_repository_key,
             state_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'profile_replaced',
                selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                $2,
                workspace_requirement_kind, requested_repository_key,
                state_kind, pinned_runner_id, pinned_working_directory,
                $2, registration_enrollment_id,
                registration_revision, pinned_tool_count,
                workspace_repository_key, workspace_working_directory,
                credential_grant_revision + 1
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(uuid(SESSION))
    .bind(replacement_profile().as_str())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
            (session_id, event_ordinal, tool_name, runner_required)
         SELECT session_id, event_ordinal + 1, tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(uuid(SESSION))
    .execute(&mut *transaction)
    .await?;
    let mismatched_grant = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *transaction)
        .await
        .expect_err("a replacement profile cannot reference the predecessor profile grant");

    assert_check_violation(mismatched_grant);
    transaction.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_inv045_cross_runner_grant_predecessor_round_trips() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store
        .register(first_enrollment.enrollment(), advertisement(), &catalog())
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
    };
    let placement =
        SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), request.clone());
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &first_enrollment,
            first_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/first".to_owned())
                .expect("the first runner directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the first runner pins the placement");
    store.store_pin(&pin, &first_registration).await?;
    let lost = pin
        .placement
        .clone()
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    store
        .store_placement(&lost, Some(&first_registration), pin.grant.as_ref())
        .await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store
        .register(second_enrollment.enrollment(), advertisement(), &catalog())
        .await?;
    let replacement = lost
        .replace_lost_runner(
            request,
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the replacement runner directory is valid"),
            None,
            pin.grant.clone(),
        )
        .expect("the replacement advances the cross-runner grant lineage");
    store
        .store_placement(
            &replacement.placement,
            Some(&second_registration),
            replacement.grant.as_ref(),
        )
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the cross-runner replacement is durable");

    assert_eq!(loaded.placement(), &replacement.placement);
    assert_eq!(loaded.grant(), replacement.grant.as_ref());
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_profile_free_replacement_starts_independent_grant_lineage()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store
        .register(first_enrollment.enrollment(), advertisement(), &catalog())
        .await?;
    let profiled_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
    };
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        profiled_request.clone(),
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &first_enrollment,
            first_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/first".to_owned())
                .expect("the first runner directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the first runner pins the placement");
    store.store_pin(&pin, &first_registration).await?;
    let first_lost = pin
        .placement
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    store
        .store_placement(&first_lost, Some(&first_registration), pin.grant.as_ref())
        .await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store
        .register(second_enrollment.enrollment(), advertisement(), &catalog())
        .await?;
    let profile_free = first_lost
        .replace_lost_runner(
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::CapabilityClass(class()),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
            },
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the second runner directory is valid"),
            None,
            pin.grant,
        )
        .expect("the replacement may intentionally omit a credential profile");
    store
        .store_placement(&profile_free.placement, Some(&second_registration), None)
        .await?;
    let second_lost = profile_free
        .placement
        .mark_runner_lost()
        .expect("the profile-free runner may be marked lost");
    store
        .store_placement(&second_lost, Some(&second_registration), None)
        .await?;
    let later_enrollment = RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(LATER_ENROLLMENT)),
        RunnerId::from_uuid(uuid(LATER_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(LATER_AUTHENTICATION)),
        [class()],
    );
    store.insert_enrollment(&later_enrollment).await?;
    let later_registration = store
        .register(later_enrollment.enrollment(), advertisement(), &catalog())
        .await?;
    let later = second_lost
        .replace_lost_runner(
            profiled_request,
            later_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/later".to_owned())
                .expect("the later runner directory is valid"),
            None,
            None,
        )
        .expect("profile selection after a profile-free placement starts a grant lineage");
    store
        .store_placement(
            &later.placement,
            Some(&later_registration),
            later.grant.as_ref(),
        )
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the later profiled replacement is durable");

    assert_eq!(loaded.placement(), &later.placement);
    assert_eq!(loaded.grant(), later.grant.as_ref());
    let later_grant = later
        .grant
        .clone()
        .expect("the later profiled replacement starts its grant lineage");
    let successor = later
        .placement
        .replace_credential_profile(
            later_grant.clone(),
            later_registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the independent grant lineage may advance");
    store
        .store_placement(
            &successor.placement,
            Some(&later_registration),
            Some(&successor.grant.grant),
        )
        .await?;
    let prior_runner: Uuid = sqlx::query_scalar(
        "SELECT prior_runner_id
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(successor.grant.grant.session().into_uuid())
    .bind(successor.grant.grant.runner().into_uuid())
    .bind(Decimal::from(successor.grant.grant.revision().get()))
    .fetch_one(&pool)
    .await?;

    assert_eq!(RunnerId::from_uuid(prior_runner), later_grant.runner());
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_worktree_pin_requires_provisioned_facts() -> Result<(), Box<dyn Error>> {
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
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: WorkspaceRepositoryKey::try_new("signalbox".to_owned())
                    .expect("the repository key is valid"),
            },
        },
    );
    store.store_placement(&placement, None, None).await?;
    let missing_workspace = sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id,
             selector_capability_class, directory_selection_kind,
             requested_working_directory,
             requested_credential_profile_name,
             workspace_requirement_kind, requested_repository_key,
             state_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             credential_grant_revision)
         SELECT session_id, 2, placement_revision, 'pinned',
                selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                'pinned', $2, '/workspace/session',
                NULL, $3, $4, 1,
                NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 1",
    )
    .bind(uuid(SESSION))
    .bind(registration.registration().runner().into_uuid())
    .bind(registration.registration().enrollment().into_uuid())
    .bind(Decimal::from(registration.revision().get()))
    .execute(&pool)
    .await
    .expect_err("a pinned worktree placement requires both provisioned facts");

    assert_check_violation(missing_workspace);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_fresh_retry_attempt_is_current_before_successor_lease()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let claimed = pin
        .lease
        .clone()
        .claim(pin.lease.correlation())
        .expect("the exact first lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed pure work may enter durable retry classification");
    store.store_lease(loss.lost()).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let retired_attempts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .fetch_all(&pool)
    .await?;
    insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let fresh_attempts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.request))
    .fetch_all(&pool)
    .await?;

    assert!(retired_attempts.is_empty());
    assert_eq!(fresh_attempts, vec![uuid(RETRY_PHYSICAL_ATTEMPT.attempt)]);
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
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration pins the placement");
    let offered = pin.lease.clone();
    store.store_pin(&pin, &registration).await?;
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
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let retry = pin
        .placement
        .offer_retry(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            loss,
            authorized(RETRY_PHYSICAL_ATTEMPT),
        )
        .expect("claimed pure work requires a fresh physical attempt");
    store.store_lease(&retry).await?;
    let reconstituted = store
        .load_lease(RunnerLeaseId::from_uuid(uuid(LEASE)), retry.generation())
        .await?
        .expect("the retry generation is durable");
    let batch_attempts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1
          ORDER BY attempt_id",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.request))
    .fetch_all(&pool)
    .await?;

    assert_eq!(
        lost.state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert_eq!(reconstituted, retry);
    assert_eq!(batch_attempts, vec![retry.attempt().into_uuid()]);
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
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration pins the placement");
    let offered = pin.lease.clone();
    store.store_pin(&pin, &registration).await?;
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
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET effect_class = 'external_effect'
          WHERE attempt_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let effect_mismatch = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, 'idempotent', placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, NULL
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(uuid(LEASE))
    .bind(uuid(LEASE + 1))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await
    .expect_err("lease effect must equal the validated registration declaration");

    assert_check_violation(effect_mismatch);
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET effect_class = 'effect_free',
                state_kind = 'prepared'
          WHERE attempt_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let non_in_flight = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, NULL
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(uuid(LEASE))
    .bind(uuid(LEASE + 2))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await
    .expect_err("only an in-flight physical attempt may receive a lease");

    assert_check_violation(non_in_flight);
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'in_flight'
          WHERE attempt_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut valid_retry = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT lease_id, 2, $2, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, 1
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(uuid(LEASE))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 1, 'offered')",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 2, 1)",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 2, 'claimed')",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 2
          WHERE lease_id = $1 AND generation = 2",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 3, 'lost_claimed')",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 3
          WHERE lease_id = $1 AND generation = 2",
    )
    .bind(uuid(LEASE))
    .execute(&mut *valid_retry)
    .await?;
    valid_retry.commit().await?;
    terminalize_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'in_flight',
                terminal_disposition_kind = NULL,
                error_kind = NULL
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let nonadjacent_reuse = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         SELECT lease_id, 3, $2, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name, credential_grant_revision,
                credential_approval_kind, 2
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 2",
    )
    .bind(uuid(LEASE))
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await
    .expect_err("no later generation may reuse any previously claimed attempt");

    assert_check_violation(nonadjacent_reuse);
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_reconstitution_rejects_noncanonical_tool_schema() -> Result<(), Box<dyn Error>>
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
    sqlx::query("ALTER TABLE runner_registration_tool DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_registration_tool
            SET model_input_schema = '{ \"x\" : 0 }'
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let malformed = store
        .load_registration(expected_enrollment.enrollment(), stored.revision())
        .await
        .expect_err("noncanonical durable schema text must fail closed");

    assert_store_corruption(malformed, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_idempotent_registration_tool_requires_runner_only_locus()
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
    sqlx::query(
        "ALTER TABLE runner_registration_tool
         DISABLE TRIGGER runner_registration_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    let invalid_locus = sqlx::query(
        "UPDATE runner_registration_tool
            SET effect_class = 'idempotent',
                loci_kind = 'daemon_or_runner'
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("idempotent tools have no daemon-local projection");
    sqlx::query(
        "ALTER TABLE runner_registration_tool
         ENABLE TRIGGER runner_registration_tool_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(invalid_locus);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_reconstitution_rejects_cross_wired_enrollment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    sqlx::query("ALTER TABLE runner_enrollment DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_enrollment
            SET runner_id = $2
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(uuid(FOREIGN_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_enrollment ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let error = store
        .load_enrollment(expected_enrollment.enrollment())
        .await
        .expect_err("cross-wired enrollment identity fails independent audit evidence");

    assert!(matches!(error, RunnerProtocolStoreError::Domain(_)));
    drop(pool);
    Ok(())
}
