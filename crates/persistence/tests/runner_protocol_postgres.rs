#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    AuthorizedToolAttempt, CredentialProfileName, CredentialProfilePolicy, CredentialToolApproval,
    ReconstitutedToolAttempt, RunnerAdvertisement, RunnerAuthenticationId, RunnerCapabilityClass,
    RunnerCatalog, RunnerEnrollment, RunnerEnrollmentId, RunnerGeneration, RunnerId, RunnerLeaseId,
    RunnerLeaseOfferRequest, RunnerSelector, RunnerToolDeclaration, RunnerToolEffectClass,
    RunnerToolModelDefinition, RunnerWorkingDirectory, SessionId, SessionRunnerPlacement,
    SessionRunnerPlacementRequest, ToolAdmissibleLoci, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDispatchGeneration,
    ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestId, TurnAttemptId, TurnId,
    WorkingDirectorySelection, WorkspaceCapability, WorkspaceRequirement,
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
const REPLACEMENT_ENROLLMENT: u128 = 0x9101;
const REPLACEMENT_RUNNER: u128 = 0x9201;
const REPLACEMENT_AUTHENTICATION: u128 = 0x9301;
const SESSION: u128 = 0x9400;
const LEASE: u128 = 0x9500;
const ATTEMPT: u128 = 0x9600;
const RETRY_ATTEMPT: u128 = 0x9601;
const FOREIGN_RUNNER: u128 = 0x9202;
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
    request: INITIAL_PHYSICAL_ATTEMPT.request,
    turn: INITIAL_PHYSICAL_ATTEMPT.turn,
};
const PROFILELESS_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9602,
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
    RunnerCatalog::try_new(
        [class()],
        [inspect, catalog_only],
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
    store.store_pin(&pin, &registration).await?;

    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pinned placement is present");

    assert_eq!(loaded.placement(), &pin.placement);
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
    let revoked_predecessor = store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a revoked predecessor cannot authorize its replacement");

    assert_store_check_violation(revoked_predecessor);
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
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 1, 'offered')",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 2, 'claimed')",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 2, 3, 'lost_claimed')",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 2, 3)",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await?;
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
