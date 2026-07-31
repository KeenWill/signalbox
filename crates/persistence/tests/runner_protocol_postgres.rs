//! Feature-gated PostgreSQL coverage for runner enrollment, leases, placement, and grants.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, time::Duration};

use rust_decimal::Decimal;
use signalbox_domain::{
    ApprovedToolRequest, CanonicalCloneUrlDigest, ContextFrontierId, CredentialProfileGrant,
    CredentialProfileGrantReconstitutionInput, CredentialProfileName, CredentialProfilePolicy,
    CredentialToolApproval, DecideToolRequest, DurableCommandId, EndedToolAttempt, ModelCallId,
    NormalizedToolArguments, ProvisionedWorkspace, ResolvedContextFrontierReconstitutionInput,
    RunnerAdvertisement, RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog,
    RunnerDomainError, RunnerEnrollment, RunnerEnrollmentId, RunnerGeneration, RunnerId,
    RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId, RunnerLeaseOfferRequest,
    RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation, RunnerRepositoryEntry,
    RunnerSandboxProfile, RunnerSelector, RunnerToolAttemptAuthorization, RunnerToolDeclaration,
    RunnerToolEffectClass, RunnerToolModelDefinition, RunnerToolPermissionOverride,
    RunnerToolPermissionOverrides, RunnerWorkingDirectory, SessionId, SessionRunnerPin,
    SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, ToolAdmissibleLoci, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolBatch,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestId, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TurnAttemptId, TurnId, ValidatedRunnerRegistration,
    WorkingDirectorySelection, WorkspaceCapability, WorkspaceManifestId, WorkspaceRecovery,
    WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement, WorkspaceRevision,
};
use signalbox_persistence::{
    MIGRATOR, local_test_connection_options, migrate,
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
const PRE_RUNNER_WIRE_MIGRATION: i64 = 202607310102;
const LEGACY_PLACEMENT_REFUSAL: &str =
    "runner wire contract requires empty legacy placement history";

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

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    Ok((container, pool))
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let (container, pool) = unmigrated_postgres().await?;
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

fn sandbox_profiles() -> [RunnerSandboxProfile; 2] {
    [
        RunnerSandboxProfile::Ambient,
        RunnerSandboxProfile::WorkspaceRestricted,
    ]
}

fn no_permission_overrides() -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([])
        .expect("the empty permission override fixture is valid")
}

fn permission_overrides(permission: RunnerToolPermissionOverride) -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([(tool("inspect"), permission)])
        .expect("the exact permission override fixture is valid")
}

fn repository_key() -> WorkspaceRepositoryKey {
    WorkspaceRepositoryKey::try_new("signalbox".to_owned())
        .expect("the fixture repository key is valid")
}

fn repository_entry() -> RunnerRepositoryEntry {
    RunnerRepositoryEntry::new(repository_key(), None)
}

fn model_definition() -> RunnerToolModelDefinition {
    RunnerToolModelDefinition::try_new(
        "Inspect the fixture workspace".to_owned(),
        format!(r#"{{"{}":0}}"#, "x".repeat(4096)),
    )
    .expect("the fixture model definition is valid")
}

fn approved_request(facts: PhysicalAttemptFacts) -> ApprovedToolRequest {
    let request = ToolRequestReconstitutionInput::new(
        ToolRequestId::from_uuid(uuid(facts.request)),
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        ModelCallId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 2))),
        ToolRequestOrdinal::from_u32(0),
        tool("inspect"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("the fixture arguments are canonical"),
    )
    .into_request();
    let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        .reconstitute()
        .expect("the fixture registry policy approves");
    ApprovedToolRequest::try_from_resolution(request, approval)
        .expect("the fixture approval matches its request")
}

fn confirmed_approved_request(facts: PhysicalAttemptFacts) -> ApprovedToolRequest {
    let request = ToolRequestReconstitutionInput::new(
        ToolRequestId::from_uuid(uuid(facts.request)),
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        ModelCallId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 2))),
        ToolRequestOrdinal::from_u32(0),
        tool("inspect"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("the fixture arguments are canonical"),
    )
    .into_request();
    let command = DecideToolRequest::try_new(
        DurableCommandId::from_uuid(uuid(facts.request + (RELATED_IDENTITY_OFFSET * 4))),
        request.id(),
        ToolApprovalDecision::Approve,
    )
    .expect("the fixture command identity is valid");
    let prepared = command
        .prepare_applied(&request)
        .expect("the fixture request and owner decision correlate");
    let signalbox_domain::DecideToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("the approving fixture owner decision applies")
    };
    ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
        .expect("the fixture owner approval matches its request")
}

fn authorization_from_approved(
    approved: ApprovedToolRequest,
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let attempt_id = ToolAttemptId::from_uuid(uuid(facts.attempt));
    let attempt = ToolAttemptReconstitutionInput::new(
        attempt_id,
        ToolRequestId::from_uuid(uuid(facts.request)),
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        effect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the fixture in-flight attempt reconstitutes");
    let batch = ToolBatchReconstitutionInput::new(
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        ModelCallId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 2))),
        ResolvedContextFrontierReconstitutionInput::new(
            SessionId::from_uuid(uuid(SESSION)),
            ContextFrontierId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 3))),
            Vec::new(),
        )
        .reconstitute()
        .expect("the empty fixture frontier is valid"),
        vec![approved.request().clone()],
        vec![approved.approval().clone()],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        },
    )
    .reconstitute()
    .expect("the fixture batch is complete");
    batch
        .resume_runner_attempt(attempt_id)
        .expect("the batch restores canonical runner authority")
}

fn authorized_with_effect(
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved(approved_request(facts), facts, effect)
}

fn confirmed_authorized_with_effect(
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved(confirmed_approved_request(facts), facts, effect)
}

fn claimed_batch_with_effect(facts: PhysicalAttemptFacts, effect: ToolEffectClass) -> ToolBatch {
    let approved = approved_request(facts);
    let attempt = ToolAttemptReconstitutionInput::new(
        ToolAttemptId::from_uuid(uuid(facts.attempt)),
        ToolRequestId::from_uuid(uuid(facts.request)),
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        effect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the claimed fixture attempt reconstitutes");
    ToolBatchReconstitutionInput::new(
        SessionId::from_uuid(uuid(SESSION)),
        TurnId::from_uuid(uuid(facts.turn)),
        ModelCallId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 2))),
        ResolvedContextFrontierReconstitutionInput::new(
            SessionId::from_uuid(uuid(SESSION)),
            ContextFrontierId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 3))),
            Vec::new(),
        )
        .reconstitute()
        .expect("the empty claimed fixture frontier is valid"),
        vec![approved.request().clone()],
        vec![approved.approval().clone()],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        },
    )
    .reconstitute()
    .expect("the claimed fixture batch is complete")
}

fn authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::EffectFree)
}

fn offer_request() -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(LEASE)),
        tool: tool("inspect"),
    }
}

fn lease_with_cross_wired_dispatch(
    lease: &RunnerLease,
    registration: &ValidatedRunnerRegistration,
) -> RunnerLease {
    let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session: lease.session(),
            turn: TurnId::from_uuid(uuid(FOREIGN_SESSION)),
            issuing_attempt: TurnAttemptId::from_uuid(uuid(FOREIGN_SESSION + 1)),
            request: ToolRequestId::from_uuid(uuid(FOREIGN_SESSION + 2)),
            attempt: lease.attempt(),
            generation: ToolDispatchGeneration::first()
                .checked_next()
                .expect("the second dispatch generation is representable"),
        },
    );
    let authorization = lease.credential_authorization().cloned();
    let correlation = RunnerLeaseCorrelation {
        lease: lease.correlation().lease,
        runner: lease.runner(),
        tool: lease.tool().clone(),
        dispatch,
        generation: lease.generation(),
    };
    RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease: correlation.lease,
            dispatch,
            runner: lease.runner(),
            tool: lease.tool().clone(),
            effect: lease.effect(),
            credential_authorization: authorization.clone(),
            generation: lease.generation(),
            state: lease.state(),
            recorded_correlation: correlation,
            recorded_session: lease.session(),
            recorded_effect: lease.effect(),
            recorded_credential_authorization: authorization,
            recorded_state: lease.state(),
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration,
    )
    .expect("the cross-wired dispatch remains internally self-consistent")
}

fn duplicate_lease(lease: &RunnerLease, registration: &ValidatedRunnerRegistration) -> RunnerLease {
    let correlation = lease.correlation();
    let authorization = lease.credential_authorization().cloned();
    RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease: correlation.lease,
            dispatch: correlation.dispatch,
            runner: lease.runner(),
            tool: lease.tool().clone(),
            effect: lease.effect(),
            credential_authorization: authorization.clone(),
            generation: lease.generation(),
            state: lease.state(),
            recorded_correlation: correlation,
            recorded_session: lease.session(),
            recorded_effect: lease.effect(),
            recorded_credential_authorization: authorization,
            recorded_state: lease.state(),
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration,
    )
    .expect("the fixture lease facts reconstitute")
}

fn duplicate_placement(
    placement: &SessionRunnerPlacement,
    registration: Option<&ValidatedRunnerRegistration>,
) -> SessionRunnerPlacement {
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session: placement.session(),
            revision: placement.revision(),
            request: placement.request().clone(),
            state: placement.state().clone(),
        },
        placement.session(),
        registration,
        None,
    )
    .expect("the fixture placement facts reconstitute")
}

fn duplicate_grant(
    grant: &CredentialProfileGrant,
    registration: &ValidatedRunnerRegistration,
) -> CredentialProfileGrant {
    CredentialProfileGrant::reconstitute(
        CredentialProfileGrantReconstitutionInput {
            session: grant.session(),
            runner: grant.runner(),
            revision: grant.revision(),
            profile: grant.profile().clone(),
            tools: grant.tools().cloned().collect(),
            approvals: grant
                .approvals()
                .map(|(tool, approval)| (tool.clone(), approval))
                .collect(),
            state: grant.state(),
        },
        grant.session(),
        registration,
        RunnerSandboxProfile::Ambient,
        &no_permission_overrides(),
    )
    .expect("the fixture grant facts reconstitute")
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
        sandbox_profiles(),
    )
    .expect("the fixture catalog is internally consistent")
}

fn confirm_catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition(),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [(tool("inspect"), CredentialToolApproval::Automatic)],
    )
    .expect("the confirm fixture profile references its declared tool");
    let replacement_policy = CredentialProfilePolicy::try_new(
        replacement_profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the confirm replacement profile references its declared tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect],
        [policy, replacement_policy],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
    )
    .expect("the confirm fixture catalog is internally consistent")
}

fn idempotent_catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition(),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::Idempotent,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [(tool("inspect"), CredentialToolApproval::Automatic)],
    )
    .expect("the idempotent fixture profile references its declared tool");
    let replacement_policy = CredentialProfilePolicy::try_new(
        replacement_profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the idempotent replacement profile references its declared tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect],
        [policy, replacement_policy],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
    )
    .expect("the idempotent fixture catalog is internally consistent")
}

fn advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

fn narrowed_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

fn profileless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

fn workspaceless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [],
        sandbox_profiles(),
        [repository_entry()],
    )
}

fn expanded_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect"), tool("catalog_only")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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

async fn insert_lease_generation_direct(
    pool: &PgPool,
    lease: &RunnerLease,
) -> Result<(), sqlx::Error> {
    let correlation = lease.correlation();
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT $1, $2, $3, record.session_id, $4,
                $5, registered.effect_class, record.event_ordinal,
                record.registration_enrollment_id, record.registration_revision,
                record.pinned_credential_profile_name,
                record.credential_grant_lineage_origin_ordinal,
                record.credential_grant_revision, approval.approval_kind, NULL
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS record
             ON record.session_id = current_placement.session_id
            AND record.event_ordinal = current_placement.event_ordinal
           JOIN runner_registration_tool AS registered
             ON registered.enrollment_id = record.registration_enrollment_id
            AND registered.registration_revision = record.registration_revision
            AND registered.tool_name = $5
           LEFT JOIN runner_credential_grant_tool AS approval
             ON approval.session_id = record.session_id
            AND approval.lineage_origin_event_ordinal =
                record.credential_grant_lineage_origin_ordinal
            AND approval.runner_id = record.pinned_runner_id
            AND approval.grant_revision = record.credential_grant_revision
            AND approval.tool_name = $5
          WHERE current_placement.session_id = $6",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(correlation.tool.as_str())
    .bind(correlation.dispatch.session().into_uuid())
    .execute(pool)
    .await?;
    Ok(())
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
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_runner_retry_is_authorized",
    )
    .execute(pool)
    .await?;
    let inserted = sqlx::query(
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
    .await;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    inserted?;
    Ok(())
}

async fn replace_approval_with_owner_command(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    let command = uuid(facts.request + (RELATED_IDENTITY_OFFSET * 4));
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp())",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'approve', NULL, 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(uuid(facts.request))
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    let updated = sqlx::query(
        "UPDATE tool_approval_decision
            SET decision_source = 'owner_command',
                owner_command_id = $2
          WHERE request_id = $1",
    )
    .bind(uuid(facts.request))
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    assert_eq!(updated.rows_affected(), 1);
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

async fn insert_external_physical_attempt(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    insert_physical_attempt(pool, facts).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET effect_class = 'external_effect'
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

/// Stores one retryable loss; the loss leaves the in-flight source attempt
/// untouched, so no fixture trigger accommodation is needed.
async fn store_fixture_retryable_loss(
    store: &RunnerProtocolStore,
    _pool: &PgPool,
    loss: &signalbox_domain::RunnerLeaseLoss,
) -> Result<(), Box<dyn Error>> {
    store.store_lease_loss(loss).await?;
    Ok(())
}

async fn authorize_fixture_claimed_retry(
    store: &RunnerProtocolStore,
    loss: &signalbox_domain::RunnerLeaseLoss,
    effect: ToolEffectClass,
) -> Result<signalbox_domain::RunnerClaimedAttemptReplacement, Box<dyn Error>> {
    let replacement = loss
        .retry()
        .expect("the durable loss carries checked retry authority")
        .prepare_claimed_attempt(
            claimed_batch_with_effect(INITIAL_PHYSICAL_ATTEMPT, effect),
            ToolAttemptId::from_uuid(uuid(RETRY_PHYSICAL_ATTEMPT.attempt)),
        )
        .expect("the owning batch produces the exact replacement attempt");
    store
        .store_claimed_retry_attempt_authority(loss, &replacement)
        .await?;
    Ok(replacement)
}

/// Persists the atomic replacement-attempt/successor-lease pair while the
/// fixture rows lack the approval and turn-attempt authority production data
/// carries; the two runner-retry attempt triggers stay enabled because they
/// are the behavior under test.
async fn store_fixture_claimed_retry_replacement(
    store: &RunnerProtocolStore,
    pool: &PgPool,
    retired: &EndedToolAttempt,
    retry: &RunnerLease,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_runner_retry_is_authorized",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_replacement_commits_with_successor_lease",
    )
    .execute(pool)
    .await?;
    let stored = store.store_claimed_retry_replacement(retired, retry).await;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    stored?;
    Ok(())
}

async fn clone_registration_without_advancing_head(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    enrollment: RunnerEnrollmentId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_registration
            (enrollment_id, registration_revision, runner_id,
             authentication_reference_id, class_count, tool_count,
             profile_count, workspace_count, repository_count, sandbox_count)
         SELECT enrollment_id, 2, runner_id, authentication_reference_id,
                class_count, tool_count, profile_count, workspace_count,
                repository_count, sandbox_count
           FROM runner_registration
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_class
         SELECT enrollment_id, 2, capability_class
           FROM runner_registration_class
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_tool
         SELECT enrollment_id, 2, tool_name, model_description,
                model_input_schema, permission_kind, effect_class,
                loci_kind, selector_kind, selector_runner_id,
                selector_capability_class
           FROM runner_registration_tool
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_profile
         SELECT enrollment_id, 2, credential_profile_name, approval_count
           FROM runner_registration_profile
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_profile_approval
         SELECT enrollment_id, 2, credential_profile_name,
                tool_name, approval_kind
           FROM runner_registration_profile_approval
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_workspace
         SELECT enrollment_id, 2, workspace_kind
           FROM runner_registration_workspace
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_sandbox
         SELECT enrollment_id, 2, sandbox_profile
           FROM runner_registration_sandbox
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_registration_repository
         SELECT enrollment_id, 2, repository_key, credential_profile_name
           FROM runner_registration_repository
          WHERE enrollment_id = $1 AND registration_revision = 1",
    )
    .bind(enrollment.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn append_runner_lost_without_advancing_head(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision,
                'runner_lost', selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'runner_lost', pinned_runner_id,
                pinned_working_directory, pinned_credential_profile_name,
                registration_enrollment_id, registration_revision,
                pinned_tool_count, workspace_repository_key,
                workspace_working_directory, workspace_manifest_id,
                workspace_clone_url_digest, workspace_credential_profile_name,
                workspace_sandbox_profile, workspace_relative_path,
                workspace_recovery_kind, workspace_branch_name, workspace_revision,
                credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
         SELECT session_id, event_ordinal + 1, tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
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

async fn rejected_workspace_branch(pool: &PgPool, session: SessionId, branch: &str) -> sqlx::Error {
    let pinned_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_kind = 'pinned'",
    )
    .bind(session.into_uuid())
    .fetch_one(pool)
    .await
    .expect("the pinned placement count is queryable");
    assert_eq!(pinned_count, 1);
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET workspace_recovery_kind = 'branch',
                workspace_branch_name = $2
          WHERE session_id = $1
            AND event_kind = 'pinned'",
    )
    .bind(session.into_uuid())
    .bind(branch)
    .execute(pool)
    .await
    .expect_err("the malformed workspace recovery branch must be schema-rejected")
}

#[track_caller]
fn assert_foreign_key_violation(error: sqlx::Error) {
    assert_eq!(
        error
            .as_database_error()
            .expect("PostgreSQL reports a database error")
            .code()
            .as_deref(),
        Some("23503")
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

/// One genuine constraint rejection for the assertion-helper tests below: the
/// enrollment guard trigger rejects an insert that does not begin active at
/// revision one with the same SQLSTATE the concurrency races produce.
async fn stored_check_violation(pool: &PgPool) -> RunnerProtocolStoreError {
    RunnerProtocolStoreError::Database(
        sqlx::query(
            "INSERT INTO runner_enrollment
                (enrollment_id, runner_id, authentication_reference_id,
                 allowed_class_count, revision, state_kind)
             VALUES ($1, $2, $3, 0, 2, 'revoked')",
        )
        .bind(uuid(LATER_ENROLLMENT))
        .bind(uuid(LATER_RUNNER))
        .bind(uuid(LATER_AUTHENTICATION))
        .execute(pool)
        .await
        .expect_err("an enrollment inserted as already revoked violates the guard"),
    )
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_wire_migration_rejects_legacy_placement_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = unmigrated_postgres().await?;
    MIGRATOR.run_to(PRE_RUNNER_WIRE_MIGRATION, &pool).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, state_kind, pinned_tool_count)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'unpinned', 0)",
    )
    .bind(uuid(SESSION))
    .bind(uuid(RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let refusal = migrate(&pool)
        .await
        .expect_err("legacy placement history must fail before wire facts are invented");
    let applied_version: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await?;

    assert!(
        refusal.to_string().contains(LEGACY_PLACEMENT_REFUSAL),
        "migration refusal must name the unsupported legacy history"
    );
    assert_eq!(applied_version, Some(PRE_RUNNER_WIRE_MIGRATION));
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn one_conflict_assertion_accepts_either_winning_order() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let second_loses = stored_check_violation(&pool).await;
    let first_loses = stored_check_violation(&pool).await;

    assert_one_store_succeeds_and_one_conflicts(Ok(()), Err(second_loses));
    assert_one_store_succeeds_and_one_conflicts(Err(first_loses), Ok(()));
    drop(pool);
    Ok(())
}

#[test]
#[should_panic(expected = "one attempt binding must win exactly once")]
fn one_conflict_assertion_rejects_two_successes() {
    assert_one_store_succeeds_and_one_conflicts(Ok(()), Ok(()));
}

#[test]
#[should_panic(expected = "one attempt binding must win exactly once")]
fn one_conflict_assertion_rejects_two_rejections() {
    assert_one_store_succeeds_and_one_conflicts(
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
    );
}

#[test]
#[should_panic(expected = "PostgreSQL must reject the invalid durable evidence")]
fn one_conflict_assertion_rejects_a_non_constraint_rejection() {
    assert_one_store_succeeds_and_one_conflicts(
        Ok(()),
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_inv042_registration_round_trips_canonical_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let mut expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;

    let loaded_enrollment = store
        .load_enrollment(expected_enrollment.enrollment())
        .await?
        .expect("the inserted enrollment is present");
    let loaded_registration = store
        .load_registration(&loaded_enrollment, stored.revision())
        .await?
        .expect("the validated registration is present");
    let loaded_placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    let _loaded_pin = loaded_placement
        .pin_and_offer_lease(
            &loaded_enrollment,
            loaded_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/loaded".to_owned())
                .expect("the loaded fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the loaded registration shares its loaded enrollment authority");
    assert_eq!(loaded_enrollment, expected_enrollment);
    assert!(store.revoke_enrollment(&mut expected_enrollment).await?);
    let historical_registration = store
        .load_registration(&expected_enrollment, stored.revision())
        .await?
        .expect("revocation preserves historical validated registration");

    assert_eq!(loaded_registration, stored);
    assert_eq!(historical_registration, stored);
    let revoked_placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    let revoked = revoked_placement
        .pin_and_offer_lease(
            &expected_enrollment,
            stored.registration(),
            RunnerWorkingDirectory::try_new("/workspace/revoked".to_owned())
                .expect("the revoked fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect_err("durable revocation closes the exact caller-held enrollment fence");
    assert_eq!(revoked, RunnerDomainError::EnrollmentRevoked);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_store_rejects_oversized_repository_inventory_before_write()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let repositories = (0..=RunnerAdvertisement::MAX_REPOSITORIES).map(|index| {
        RunnerRepositoryEntry::new(
            WorkspaceRepositoryKey::try_new(format!("repository_{index}"))
                .expect("the generated repository key is valid"),
            None,
        )
    });
    let oversized = RunnerAdvertisement::new([class()], [], [], [], [], repositories);
    let error = store
        .register(&expected_enrollment, oversized)
        .await
        .expect_err("the persistence boundary rejects the oversized inventory");
    let RunnerProtocolStoreError::Domain(actual) = error else {
        panic!("the oversized inventory must fail at the domain boundary");
    };
    let durable_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM runner_registration
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(actual, RunnerDomainError::TooManyAdvertisedRepositories);
    assert_eq!(durable_count, 0);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_failed_registration_write_preserves_prior_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let prior = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query(
        "ALTER TABLE runner_registration
         ADD CONSTRAINT reject_registration_insert_for_test
         CHECK (registration_revision < 2)",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .register(&expected_enrollment, expanded_advertisement())
        .await
        .expect_err("a synthetic storage failure rejects the replacement");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    let _retained = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            prior.registration(),
            RunnerWorkingDirectory::try_new("/workspace/retained".to_owned())
                .expect("the retained fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("failed persistence cannot retire the prior registration");
    let RunnerProtocolStoreError::Database(_) = rejected else {
        panic!("the synthetic constraint must reject the durable write")
    };

    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_insert_enrollment_requires_pristine_registration_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    expected_enrollment
        .register(advertisement(), &catalog())
        .expect("the domain-only path issues a registration before insertion");

    let rejected = store
        .insert_enrollment(&expected_enrollment)
        .await
        .expect_err("an enrollment that already issued a registration is not pristine");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert!(
        store
            .load_enrollment(expected_enrollment.enrollment())
            .await?
            .is_none()
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_outstanding_preparation_fails_registration_before_durable_writes()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let first = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let outstanding = expected_enrollment
        .prepare_registration(advertisement(), &catalog())
        .expect("the enrollment prepares a concurrent registration");

    let rejected = store
        .register(&expected_enrollment, advertisement())
        .await
        .expect_err("an outstanding preparation excludes a second registration");

    assert_store_domain_error(rejected, RunnerDomainError::RegistrationInProgress);
    let current = store
        .load_current_registration(&expected_enrollment)
        .await?
        .expect("the rejected registration left the durable head unchanged");
    assert_eq!(current, first);
    drop(outstanding);
    let advanced = store
        .register(&expected_enrollment, advertisement())
        .await?;
    assert_eq!(
        Some(advanced.revision().get()),
        first.revision().get().checked_add(1)
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_historical_registration_load_remains_stale() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let historical = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    let loaded_enrollment = store
        .load_enrollment(expected_enrollment.enrollment())
        .await?
        .expect("the enrollment with its advanced head is present");
    let loaded_historical = store
        .load_registration(&loaded_enrollment, historical.revision())
        .await?
        .expect("the historical registration remains readable");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    let rejected = placement
        .pin_and_offer_lease(
            &loaded_enrollment,
            loaded_historical.registration(),
            RunnerWorkingDirectory::try_new("/workspace/stale".to_owned())
                .expect("the stale fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect_err("a historical registration cannot regain current authority");

    assert_eq!(rejected, RunnerDomainError::RegistrationChanged);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_stale_loaded_enrollment_cannot_bind_historical_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let historical = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let stale_enrollment = store
        .load_enrollment(expected_enrollment.enrollment())
        .await?
        .expect("the first registration head is loaded");
    let current_enrollment = store
        .load_enrollment(expected_enrollment.enrollment())
        .await?
        .expect("the independent current authority is loaded");
    store
        .register(&current_enrollment, expanded_advertisement())
        .await?;
    let rejected = store
        .load_registration(&stale_enrollment, historical.revision())
        .await
        .expect_err("stale enrollment revision cannot bind historical registration as current");

    assert_store_domain_error(rejected, RunnerDomainError::CorruptStoredFacts);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_orphan_revocation_audit_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
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
async fn s30_inv042_historical_enrollment_audit_rechecks_its_own_revision()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let mut expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    store.revoke_enrollment(&mut expected_enrollment).await?;
    let corrupted_history = sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         VALUES ($1, 1, 'foreign.class')",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&pool)
    .await
    .expect_err("historical audit satellites must recheck their named revision");

    assert_check_violation(corrupted_history);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_current_registration_gates_new_leases() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _registration, pin) = stored_pin_fixture(&pool).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let expanded_registration = store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    let retained_tool_lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            expanded_registration.registration(),
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
    let narrowed_registration = store
        .register(&expected_enrollment, narrowed_advertisement())
        .await?;
    let stale_registration = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            narrowed_registration.registration(),
            pin.grant.as_ref(),
            authorized(SECOND_LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 2)),
                tool: tool("inspect"),
            },
        )
        .expect_err("a withdrawn current tool cannot receive a later runner lease");

    assert_eq!(stale_registration, RunnerDomainError::RegistrationChanged);
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
    let current_registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let stale_snapshot = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            current_registration.registration(),
            pin.grant.as_ref(),
            authorized(PROFILELESS_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect_err("current availability must retain every runner-required pinned tool");

    assert_eq!(stale_snapshot, RunnerDomainError::RegistrationChanged);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv042_current_registration_preserves_profile() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
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
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
    let current_registration = store
        .register(&expected_enrollment, profileless_advertisement())
        .await?;
    let profile_stale = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            current_registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect_err("current registration must retain the pinned profile");

    assert_eq!(profile_stale, RunnerDomainError::RegistrationChanged);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv042_current_registration_preserves_workspace() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
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
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
                placement_revision: RunnerGeneration::one(),
                runner: expected_enrollment.runner(),
                repository: Some(repository),
                canonical_clone_url_digest: Some(
                    CanonicalCloneUrlDigest::try_new("b".repeat(64))
                        .expect("the fixture clone URL digest is canonical"),
                ),
                credential_profile: None,
                sandbox: RunnerSandboxProfile::Ambient,
                working_directory: directory,
                relative_path: WorkspaceRelativePath::try_new("sessions/session/1/repo".to_owned())
                    .expect("the fixture workspace path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x80)),
                recovery: Some(WorkspaceRecovery::Commit {
                    revision: WorkspaceRevision::try_new("c".repeat(40))
                        .expect("the fixture recovery revision is canonical"),
                }),
            }),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the worktree capability satisfies the initial pin");
    store.store_pin(&pin, &registration).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let current_registration = store
        .register(&expected_enrollment, workspaceless_advertisement())
        .await?;
    let workspace_stale = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            current_registration.registration(),
            pin.grant.as_ref(),
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect_err("current registration must retain the worktree capability");

    assert_eq!(workspace_stale, RunnerDomainError::RegistrationChanged);
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let closing_bracket = sqlx::query(
        "UPDATE runner_session_placement_record
            SET workspace_recovery_kind = 'branch',
                workspace_branch_name = 'topic]ok'
          WHERE session_id = $1
            AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(closing_bracket.rows_affected(), 1);
    let single_at_branch = rejected_workspace_branch(&pool, pin.placement.session(), "@").await;
    let double_dot_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "main..x").await;
    let reflog_branch = rejected_workspace_branch(&pool, pin.placement.session(), "bad@{x").await;
    let trailing_dot_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "feature.").await;
    let hidden_component_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "topic/.hidden").await;
    let lock_suffix_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "topic.lock").await;
    let bracket_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "topic[bad").await;
    let backslash_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), r"topic\bad").await;
    let control_branch =
        rejected_workspace_branch(&pool, pin.placement.session(), "topic\nbad").await;
    let pinned_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pinned_count, 1);
    let absolute_path = sqlx::query(
        "UPDATE runner_session_placement_record
            SET workspace_relative_path = '/absolute'
          WHERE session_id = $1
            AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await
    .expect_err("an absolute workspace manifest path is schema-rejected");
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(single_at_branch);
    assert_check_violation(double_dot_branch);
    assert_check_violation(reflog_branch);
    assert_check_violation(trailing_dot_branch);
    assert_check_violation(hidden_component_branch);
    assert_check_violation(lock_suffix_branch);
    assert_check_violation(bracket_branch);
    assert_check_violation(backslash_branch);
    assert_check_violation(control_branch);
    assert_check_violation(absolute_path);
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
    let replacement_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let mut replacement =
        Box::pin(replacement_store.register(&expected_enrollment, narrowed_advertisement()));
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
        .register(&expected_enrollment, expanded_advertisement())
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_current_registration_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_current_registration")
        .execute(&pool)
        .await
        .expect_err("the registration head cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_enrollment_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_enrollment_allowed_class CASCADE")
        .execute(&pool)
        .await
        .expect_err("immutable enrollment classes cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_enrollment_audit_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_enrollment_audit_allowed_class")
        .execute(&pool)
        .await
        .expect_err("immutable enrollment audit classes cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_registration_inventories_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let registration = sqlx::query("TRUNCATE runner_registration CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration record cannot be truncated");
    let classes = sqlx::query("TRUNCATE runner_registration_class CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration class inventory cannot be truncated");
    let tools = sqlx::query("TRUNCATE runner_registration_tool CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration tool inventory cannot be truncated");
    let profiles = sqlx::query("TRUNCATE runner_registration_profile CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration profile inventory cannot be truncated");
    let approvals = sqlx::query("TRUNCATE runner_registration_profile_approval CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration profile approvals cannot be truncated");
    let workspaces = sqlx::query("TRUNCATE runner_registration_workspace CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration workspace inventory cannot be truncated");
    let sandboxes = sqlx::query("TRUNCATE runner_registration_sandbox CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration sandbox inventory cannot be truncated");
    let repositories = sqlx::query("TRUNCATE runner_registration_repository CASCADE")
        .execute(&pool)
        .await
        .expect_err("the registration repository inventory cannot be truncated");

    assert_check_violation(registration);
    assert_check_violation(classes);
    assert_check_violation(tools);
    assert_check_violation(profiles);
    assert_check_violation(approvals);
    assert_check_violation(workspaces);
    assert_check_violation(sandboxes);
    assert_check_violation(repositories);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_appended_registration_must_advance_current_head() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    store
        .register(&expected_enrollment, advertisement())
        .await?;
    let mut malformed = pool.begin().await?;
    clone_registration_without_advancing_head(&mut malformed, expected_enrollment.enrollment())
        .await?;
    let stale_head = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("every complete registration append must advance its current head");

    assert_check_violation(stale_head);
    malformed.rollback().await?;
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
    let first_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let second_store = RunnerProtocolStore::new(pool.clone(), catalog());
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
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact first lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed pure work may enter durable retry classification");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;
    let rejected = insert_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT)
        .await
        .expect_err("an extra physical attempt requires durable retry authority");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_orphan_request_lease_binding_cannot_commit() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_tool_request_lease_binding
            (request_id, lease_id)
         VALUES ($1, $2)",
    )
    .bind(uuid(LATER_LEASE_PHYSICAL_ATTEMPT.request))
    .bind(uuid(LEASE + 99))
    .execute(&mut *malformed)
    .await?;
    let orphan = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("a request binding must install its matching lease lineage");

    assert_check_violation(orphan);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_request_lease_binding_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_tool_request_lease_binding")
        .execute(&pool)
        .await
        .expect_err("durable request lineage cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_orphan_physical_attempt_binding_cannot_commit()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let orphan = sqlx::query(
        "INSERT INTO runner_physical_attempt_lease_binding
            (attempt_id, lease_id)
         VALUES ($1, $2)",
    )
    .bind(uuid(LATER_LEASE_PHYSICAL_ATTEMPT.attempt))
    .bind(uuid(LEASE + 99))
    .execute(&pool)
    .await
    .expect_err("a physical attempt binding must install its matching lease lineage");

    assert_check_violation(orphan);
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
async fn s31_inv042_direct_lease_admission_serializes_enrollment_revocation()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment().into_uuid();
    let mut revocation = pool.begin().await?;
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
    let mut direct_admission = Box::pin(insert_lease_generation_direct(&pool, &lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut direct_admission)
        .await
        .expect_err("the trigger must wait for direct enrollment revocation");
    revocation.commit().await?;
    let rejected = direct_admission
        .await
        .expect_err("a directly inserted lease cannot use revoked enrollment authority");

    assert_check_violation(rejected);
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
        "INSERT INTO runner_credential_grant_audit
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         SELECT $1, lineage_origin_event_ordinal,
                $2, $3, 2, 'revoked', $4
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(grant.profile().as_str())
    .execute(&mut *revocation)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease insert must wait for direct revocation authority");
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
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(original_grant, registration.registration()),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the active predecessor permits profile replacement");
    let replacement_grant = duplicate_grant(&replacement.grant.grant, registration.registration());
    let mut revocation = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         SELECT $1, lineage_origin_event_ordinal,
                $2, $3, 2, 'revoked', $4
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(original_grant.session().into_uuid())
    .bind(original_grant.runner().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .bind(original_grant.profile().as_str())
    .execute(&mut *revocation)
    .await?;
    let mut replacement_store = Box::pin(store.store_placement(
        &replacement.placement,
        Some(&registration),
        Some(&replacement_grant),
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut replacement_store)
        .await
        .expect_err("replacement must wait for direct revocation authority");
    revocation.commit().await?;
    let rejected = replacement_store
        .await
        .expect_err("profile replacement cannot reactivate a revoked predecessor");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// S32 / INV-045: profile replacement stays durable after an
/// availability-equivalent re-registration. The domain validates the
/// replacement against the enrollment-owned current revision while the
/// placement record carries the pinned registration snapshot forward.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_profile_replacement_survives_equivalent_reregistration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    let current = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(original_grant, current.registration()),
            current.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the advanced current registration permits profile replacement");
    let replacement_grant = duplicate_grant(&replacement.grant.grant, current.registration());
    store
        .store_placement(
            &replacement.placement,
            Some(&current),
            Some(&replacement_grant),
        )
        .await?;
    let recorded: (String, Decimal) = sqlx::query_as(
        "SELECT event_kind, registration_revision
           FROM runner_session_placement_record
          WHERE session_id = $1
          ORDER BY event_ordinal DESC
          LIMIT 1",
    )
    .bind(uuid(SESSION))
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        recorded,
        (
            "profile_replaced".to_owned(),
            Decimal::from(registration.revision().get())
        )
    );
    drop(pool);
    Ok(())
}

/// S31 / INV-035 / INV-045: a session-policy tool/profile pair admits a lease
/// only with confirmed approval provenance; policy-auto provenance is
/// rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv035_inv045_session_policy_lease_requires_confirmed_provenance()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(replacement_profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: permission_overrides(RunnerToolPermissionOverride::Confirm),
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
            confirmed_authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::EffectFree),
            offer_request(),
        )
        .expect("the session-policy profile pins the placement");
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source)
         VALUES ($1, 'approve', 'policy_auto')",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let unconfirmed = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("policy-auto provenance cannot admit a session-policy lease");
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_approval_decision
            SET decision_source = 'session_blanket'
          WHERE request_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let blanket = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("a session blanket cannot authorize runner dispatch");
    replace_approval_with_owner_command(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    store.store_pin(&pin, &registration).await?;
    let admitted: Decimal = sqlx::query_scalar(
        "SELECT generation
           FROM runner_lease_generation
          WHERE lease_id = $1",
    )
    .bind(uuid(LEASE))
    .fetch_one(&pool)
    .await?;

    assert_store_check_violation(unconfirmed);
    assert_store_check_violation(blanket);
    assert_eq!(admitted, Decimal::from(1u64));
    drop(pool);
    Ok(())
}

/// S31 / INV-035 / INV-045: a profileless lease on a Confirm-permission tool
/// admits only confirmed approval provenance; policy-auto provenance is
/// rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv035_inv045_profileless_confirm_lease_requires_confirmed_provenance()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), confirm_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: permission_overrides(RunnerToolPermissionOverride::Confirm),
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
            confirmed_authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::EffectFree),
            offer_request(),
        )
        .expect("the profileless placement pins its runner");
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source)
         VALUES ($1, 'approve', 'policy_auto')",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let unconfirmed = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("policy-auto provenance cannot admit a profileless confirm lease");
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_approval_decision
            SET decision_source = 'session_blanket'
          WHERE request_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let blanket = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("a session blanket cannot authorize runner dispatch");
    replace_approval_with_owner_command(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    store.store_pin(&pin, &registration).await?;
    let admitted: Decimal = sqlx::query_scalar(
        "SELECT generation
           FROM runner_lease_generation
          WHERE lease_id = $1",
    )
    .bind(uuid(LEASE))
    .fetch_one(&pool)
    .await?;

    assert_store_check_violation(unconfirmed);
    assert_store_check_violation(blanket);
    assert_eq!(admitted, Decimal::from(1u64));
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
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(original_grant, registration.registration()),
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
async fn s30_inv044_profile_replacement_requires_current_registration() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries a credential grant");
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(original_grant, registration.registration()),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("the current registration validates the replacement");
    store
        .register(&expected_enrollment, advertisement())
        .await?;

    let stale = store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            Some(&replacement.grant.grant),
        )
        .await
        .expect_err("a superseded registration cannot install replacement authority");

    assert_store_domain_error(stale, RunnerDomainError::RegistrationChanged);
    let retained = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pre-replacement placement remains current");
    assert_eq!(retained.placement(), &pin.placement);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv044_runner_replacement_requires_active_enrollment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = duplicate_placement(&pin.placement, Some(registration.registration()))
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
        .expect("the caller-held authority still validates the replacement");
    let enrollment_uuid = expected_enrollment.enrollment().into_uuid();
    let mut revocation = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id,
             authentication_reference_id, allowed_class_count, state_kind)
         SELECT enrollment_id, 2, runner_id,
                authentication_reference_id, allowed_class_count, 'revoked'
           FROM runner_enrollment_audit
          WHERE enrollment_id = $1 AND revision = 1",
    )
    .bind(enrollment_uuid)
    .execute(&mut *revocation)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         SELECT enrollment_id, 2, capability_class
           FROM runner_enrollment_audit_allowed_class
          WHERE enrollment_id = $1 AND revision = 1",
    )
    .bind(enrollment_uuid)
    .execute(&mut *revocation)
    .await?;
    sqlx::query(
        "UPDATE runner_enrollment
            SET revision = 2, state_kind = 'revoked'
          WHERE enrollment_id = $1",
    )
    .bind(enrollment_uuid)
    .execute(&mut *revocation)
    .await?;
    revocation.commit().await?;

    let rejected = store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a revoked enrollment cannot install replacement authority");

    assert_store_domain_error(rejected, RunnerDomainError::EnrollmentRevoked);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv044_first_placement_record_is_created_unpinned() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session_for(&pool, uuid(FOREIGN_SESSION)).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
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
async fn s30_inv042_inv044_placement_required_flag_matches_registered_locus()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_tool
         DISABLE TRIGGER runner_session_placement_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    let mismatched_flag = sqlx::query(
        "UPDATE runner_session_placement_tool
            SET runner_required = false
          WHERE session_id = $1
            AND event_ordinal = 2
            AND tool_name = $2",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("a runner-only declaration must remain runner-required");
    sqlx::query(
        "ALTER TABLE runner_session_placement_tool
         ENABLE TRIGGER runner_session_placement_tool_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(mismatched_flag);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv044_initial_pin_requires_loadable_offered_lease() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
             requested_sandbox_profile, permission_override_count,
             state_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             workspace_manifest_id, workspace_clone_url_digest,
             workspace_credential_profile_name, workspace_sandbox_profile,
             workspace_relative_path, workspace_recovery_kind,
             workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision, 'pinned',
                selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'pinned', $2, $3,
                NULL, $4, $5,
                (
                    SELECT count(*)
                      FROM runner_registration_tool
                     WHERE enrollment_id = $4
                       AND registration_revision = $5
                       AND tool_name = $6
                ),
                workspace_repository_key, workspace_working_directory,
                workspace_manifest_id, workspace_clone_url_digest,
                workspace_credential_profile_name, workspace_sandbox_profile,
                workspace_relative_path, workspace_recovery_kind,
                workspace_branch_name, workspace_revision,
                credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision
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
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         VALUES (
             $1, 1, $2, $3, $4,
             $5, 'pure', 2,
             $6, $7,
             NULL, NULL, NULL, NULL, NULL
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
async fn s32_inv045_grant_lineage_origin_is_part_of_every_durable_identity()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let grant_primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT array_agg(attribute.attname::text ORDER BY key.ordinality)
           FROM pg_constraint AS constraint_record
           JOIN pg_class AS relation
             ON relation.oid = constraint_record.conrelid
          CROSS JOIN LATERAL
               unnest(constraint_record.conkey)
               WITH ORDINALITY AS key(attnum, ordinality)
           JOIN pg_attribute AS attribute
             ON attribute.attrelid = relation.oid
            AND attribute.attnum = key.attnum
          WHERE constraint_record.contype = 'p'
            AND relation.relname = 'runner_credential_grant'
          GROUP BY constraint_record.oid",
    )
    .fetch_one(&pool)
    .await?;
    let underbound_references: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_constraint AS constraint_record
           JOIN pg_class AS referenced_relation
             ON referenced_relation.oid = constraint_record.confrelid
          WHERE constraint_record.contype = 'f'
            AND referenced_relation.relname IN (
                'runner_credential_grant',
                'runner_credential_grant_tool',
                'runner_credential_grant_audit'
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM unnest(constraint_record.confkey)
                       AS referenced_key(attnum)
                  JOIN pg_attribute AS referenced_attribute
                    ON referenced_attribute.attrelid =
                        constraint_record.confrelid
                   AND referenced_attribute.attnum =
                        referenced_key.attnum
                 WHERE referenced_attribute.attname =
                    'lineage_origin_event_ordinal'
            )",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        grant_primary_key,
        vec![
            "session_id",
            "lineage_origin_event_ordinal",
            "runner_id",
            "grant_revision",
        ],
    );
    assert_eq!(underbound_references, 0);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_inv045_pinned_affinity_and_grant_round_trip() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
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
        placement: duplicate_placement(&pin.placement, Some(registration.registration())),
        grant: pin
            .grant
            .as_ref()
            .map(|grant| duplicate_grant(grant, registration.registration())),
        lease: duplicate_lease(&pin.lease, registration.registration())
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
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
    let profile_replacement =
        duplicate_placement(&pin.placement, Some(registration.registration()))
            .replace_credential_profile(
                duplicate_grant(
                    pin.grant
                        .as_ref()
                        .expect("the fixture pin carries a credential grant"),
                    registration.registration(),
                ),
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
    let same_profile_replacement =
        duplicate_placement(&pin.placement, Some(registration.registration()))
            .replace_credential_profile(
                duplicate_grant(
                    pin.grant
                        .as_ref()
                        .expect("the fixture pin carries a credential grant"),
                    registration.registration(),
                ),
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

    assert_store_domain_error(stale_grant_revision, RunnerDomainError::CorruptStoredFacts);
    let lost = duplicate_placement(&pin.placement, Some(registration.registration()))
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
        .register(&expected_enrollment, narrowed_advertisement())
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the pinned placement and historical registration reload together");
    let lost = duplicate_placement(
        loaded.placement(),
        loaded
            .registration()
            .map(StoredValidatedRunnerRegistration::registration),
    )
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_inv044_direct_lease_admission_serializes_runner_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin, lease) = stored_later_lease_fixture(&pool).await?;
    let mut runner_loss = pool.begin().await?;
    append_runner_lost_without_advancing_head(&mut runner_loss, pin.placement.session()).await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&mut *runner_loss)
    .await?;
    let mut direct_admission = Box::pin(insert_lease_generation_direct(&pool, &lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut direct_admission)
        .await
        .expect_err("the trigger must wait for the placement head transition");
    runner_loss.commit().await?;
    let rejected = direct_admission
        .await
        .expect_err("a directly inserted lease cannot use the lost runner placement");

    assert_check_violation(rejected);
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
async fn s32_inv044_current_placement_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, _) = stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_current_session_placement")
        .execute(&pool)
        .await
        .expect_err("the placement head cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_appended_placement_must_advance_current_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    append_runner_lost_without_advancing_head(&mut malformed, pin.placement.session()).await?;
    let stale_head = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("every complete placement append must advance its current head");

    assert_check_violation(stale_head);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_initial_lease_rejects_cross_wired_dispatch_fence() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, _, lease) = stored_later_lease_fixture(&pool).await?;
    let cross_wired = lease_with_cross_wired_dispatch(&lease, registration.registration());
    let rejected = store
        .store_lease(&cross_wired)
        .await
        .expect_err("an offered lease must match every canonical dispatch-fence field");

    assert_store_corruption(rejected, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_later_lease_event_rejects_cross_wired_dispatch_fence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, _, lease) = stored_later_lease_fixture(&pool).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact lease fence claims");
    let cross_wired = lease_with_cross_wired_dispatch(&claimed, registration.registration());
    let rejected = store
        .store_lease(&cross_wired)
        .await
        .expect_err("a later event must match every canonical dispatch-fence field");

    assert_store_corruption(rejected, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_current_lease_event_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
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
async fn s31_inv043_current_lease_event_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query("TRUNCATE runner_current_lease_event")
        .execute(&pool)
        .await
        .expect_err("the lease event head cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_lease_event_history_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    stored_pin_fixture(&pool).await?;
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         DISABLE TRIGGER runner_current_lease_event_rejects_truncate",
    )
    .execute(&pool)
    .await?;
    let truncated = sqlx::query(
        "TRUNCATE runner_lease_event,
                  runner_current_lease_event",
    )
    .execute(&pool)
    .await
    .expect_err("durable lease state history cannot be truncated");
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         ENABLE TRIGGER runner_current_lease_event_rejects_truncate",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_appended_lease_event_must_advance_current_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'claimed')",
    )
    .bind(pin.lease.correlation().lease.into_uuid())
    .bind(Decimal::from(pin.lease.correlation().generation.get()))
    .execute(&mut *malformed)
    .await?;
    let stale_head = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("an appended lease event must advance its current head");

    assert_check_violation(stale_head);
    malformed.rollback().await?;
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
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, NULL
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
async fn s30_inv042_explicit_automatic_grant_approval_cannot_be_downgraded()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = duplicate_placement(&pin.placement, Some(registration.registration()))
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    store
        .store_placement(&lost, Some(&registration), pin.grant.as_ref())
        .await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    let grant = replacement
        .grant
        .as_ref()
        .expect("the replacement carries its successor grant");
    store
        .store_placement(&replacement.placement, Some(&registration), Some(grant))
        .await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant_tool
         DISABLE TRIGGER runner_credential_grant_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    let downgraded = sqlx::query(
        "UPDATE runner_credential_grant_tool
            SET approval_kind = 'session_policy'
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3
            AND tool_name = $4",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("an explicit automatic profile approval cannot be downgraded");
    sqlx::query(
        "ALTER TABLE runner_credential_grant_tool
         ENABLE TRIGGER runner_credential_grant_tool_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(downgraded);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_grant_audit_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    stored_pin_fixture(&pool).await?;
    let truncated = sqlx::query(
        "TRUNCATE runner_credential_grant_audit,
                  runner_current_credential_grant_audit",
    )
    .execute(&pool)
    .await
    .expect_err("immutable credential grant audit evidence cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_new_revoked_grant_round_trips_terminal_audit() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(
                pin.grant
                    .as_ref()
                    .expect("the fixture pin carries a credential grant"),
                registration.registration(),
            ),
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
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(initial, registration.registration()),
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
             requested_sandbox_profile, permission_override_count,
             state_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             workspace_manifest_id, workspace_clone_url_digest,
             workspace_credential_profile_name, workspace_sandbox_profile,
             workspace_relative_path, workspace_recovery_kind,
             workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'profile_replaced',
                selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                $2,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                state_kind, pinned_runner_id, pinned_working_directory,
                $2, registration_enrollment_id,
                registration_revision, pinned_tool_count,
                workspace_repository_key, workspace_working_directory,
                workspace_manifest_id, workspace_clone_url_digest,
                workspace_credential_profile_name, workspace_sandbox_profile,
                workspace_relative_path, workspace_recovery_kind,
                workspace_branch_name, workspace_revision,
                credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
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

    assert_foreign_key_violation(mismatched_grant);
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
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
    let lost = duplicate_placement(&pin.placement, Some(first_registration.registration()))
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    store
        .store_placement(&lost, Some(&first_registration), pin.grant.as_ref())
        .await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    let replacement = lost
        .replace_lost_runner(
            request,
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the replacement runner directory is valid"),
            None,
            pin.grant
                .as_ref()
                .map(|grant| duplicate_grant(grant, first_registration.registration())),
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

/// S32 / INV-045: a profile-free tombstone retains the predecessor placement's
/// approval policy even when the successor placement selects a different one.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_profile_free_tombstone_uses_predecessor_approval_policy()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    let first_directory = RunnerWorkingDirectory::try_new("/workspace/first".to_owned())
        .expect("the first runner directory is valid");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &first_enrollment,
            first_registration.registration(),
            first_directory.clone(),
            Some(ProvisionedWorkspace {
                session: SessionId::from_uuid(uuid(SESSION)),
                placement_revision: RunnerGeneration::one(),
                runner: first_enrollment.runner(),
                repository: None,
                canonical_clone_url_digest: None,
                credential_profile: None,
                sandbox: RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: first_directory,
                relative_path: WorkspaceRelativePath::try_new("sessions/session/1/work".to_owned())
                    .expect("the private-root path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x80)),
                recovery: None,
            }),
            authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::ExternalEffect),
            offer_request(),
        )
        .expect("the restricted profiled placement pins automatically");
    let inspect_tool = tool("inspect");
    let expected_approval = pin
        .grant
        .as_ref()
        .expect("the profiled placement carries a grant")
        .approvals()
        .find(|(name, _)| *name == &inspect_tool)
        .map(|(_, approval)| approval)
        .expect("the predecessor grant records inspect approval");
    store.store_pin(&pin, &first_registration).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    store
        .store_placement(&lost, Some(&first_registration), pin.grant.as_ref())
        .await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    let profile_free = lost
        .replace_lost_runner(
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::CapabilityClass(class()),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: RunnerSandboxProfile::Ambient,
                permission_overrides: no_permission_overrides(),
            },
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the second runner directory is valid"),
            None,
            pin.grant,
        )
        .expect("the profile-free replacement changes placement approval policy");
    let tombstone = profile_free
        .grant
        .expect("the profile-free replacement carries its terminal grant tombstone");
    store
        .store_placement(
            &profile_free.placement,
            Some(&second_registration),
            Some(&tombstone),
        )
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the differently-policied profile-free replacement is loadable");
    let actual_approval = loaded
        .grant()
        .expect("the loaded placement retains its tombstone")
        .approvals()
        .find(|(name, _)| *name == &inspect_tool)
        .map(|(_, approval)| approval)
        .expect("the loaded tombstone records inspect approval");

    assert_eq!(actual_approval, expected_approval);
    assert_eq!(loaded.grant(), Some(&tombstone));
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv045_profile_free_replacement_preserves_grant_lineage() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    let profiled_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
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
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    let profile_free = first_lost
        .replace_lost_runner(
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::CapabilityClass(class()),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: RunnerSandboxProfile::Ambient,
                permission_overrides: no_permission_overrides(),
            },
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the second runner directory is valid"),
            None,
            pin.grant,
        )
        .expect("the replacement may intentionally omit a credential profile");
    let tombstone = profile_free
        .grant
        .expect("the profile-free replacement carries a terminal grant tombstone");
    let expected_tombstone_revision = tombstone.revision();
    let expected_tombstone_runner = tombstone.runner();
    store
        .store_placement(
            &profile_free.placement,
            Some(&second_registration),
            Some(&tombstone),
        )
        .await?;
    let stored_profile_free = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the profile-free replacement remains loadable");
    assert_eq!(stored_profile_free.grant(), Some(&tombstone));
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(&pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    let profileless_lease = profile_free
        .placement
        .offer_lease(
            &second_enrollment,
            second_registration.registration(),
            None,
            authorized(LATER_LEASE_PHYSICAL_ATTEMPT),
            RunnerLeaseOfferRequest {
                lease: RunnerLeaseId::from_uuid(uuid(LEASE + 1)),
                tool: tool("inspect"),
            },
        )
        .expect("a revoked tombstone does not become profileless lease authority");
    store.store_lease(&profileless_lease).await?;
    let second_lost = profile_free
        .placement
        .mark_runner_lost()
        .expect("the profile-free runner may be marked lost");
    store
        .store_placement(&second_lost, Some(&second_registration), Some(&tombstone))
        .await?;
    let later_enrollment = RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(LATER_ENROLLMENT)),
        RunnerId::from_uuid(uuid(LATER_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(LATER_AUTHENTICATION)),
        [class()],
    );
    store.insert_enrollment(&later_enrollment).await?;
    let later_registration = store.register(&later_enrollment, advertisement()).await?;
    let later = second_lost
        .replace_lost_runner(
            profiled_request,
            later_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/later".to_owned())
                .expect("the later runner directory is valid"),
            None,
            Some(tombstone),
        )
        .expect("profile selection after a profile-free placement advances the grant lineage");
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
    let restored_grant = later
        .grant
        .as_ref()
        .expect("the restored profile carries its successor grant");
    assert_eq!(
        restored_grant.revision(),
        expected_tombstone_revision
            .checked_next()
            .expect("the tombstone successor revision is representable"),
    );
    let later_grant = duplicate_grant(
        later
            .grant
            .as_ref()
            .expect("the later profiled replacement starts its grant lineage"),
        later_registration.registration(),
    );
    let later_prior_runner: Uuid = sqlx::query_scalar(
        "SELECT prior_runner_id
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(later_grant.session().into_uuid())
    .bind(later_grant.runner().into_uuid())
    .bind(Decimal::from(later_grant.revision().get()))
    .fetch_one(&pool)
    .await?;
    let expected_prior_runner = later_grant.runner();
    let successor = later
        .placement
        .replace_credential_profile(
            later_grant,
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
    let successor_prior_runner: Uuid = sqlx::query_scalar(
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

    assert_eq!(
        RunnerId::from_uuid(later_prior_runner),
        expected_tombstone_runner,
    );
    assert_eq!(
        RunnerId::from_uuid(successor_prior_runner),
        expected_prior_runner,
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_inv044_worktree_pin_requires_provisioned_facts() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
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
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
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
async fn s31_inv004_inv043_replacement_attempt_commits_only_with_successor_lease()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact first lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed pure work may enter durable retry classification");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;
    let lost_source_facts: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .fetch_one(&pool)
    .await?;
    let lost_source_current: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.request))
    .fetch_all(&pool)
    .await?;
    let replacement =
        authorize_fixture_claimed_retry(&store, &loss, ToolEffectClass::EffectFree).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_runner_retry_is_authorized",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_replacement_commits_with_successor_lease",
    )
    .execute(&pool)
    .await?;
    let mut stranded_replacement = pool.begin().await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'crash_lost'
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .execute(&mut *stranded_replacement)
    .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind)
         VALUES ($1, $2, $3, $4, $5, 'effect_free', 1, 'in_flight')",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.attempt))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.request))
    .bind(uuid(SESSION))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.turn))
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.turn + RELATED_IDENTITY_OFFSET))
    .execute(&mut *stranded_replacement)
    .await?;
    let stranded = stranded_replacement
        .commit()
        .await
        .expect_err("a reserved replacement attempt cannot commit without its successor lease");
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let (_batch, retired, retry_authorization) = replacement.into_parts();
    let retry = pin
        .placement
        .offer_retry(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            loss,
            retry_authorization,
        )
        .expect("claimed pure work re-leases at the successor generation");
    store_fixture_claimed_retry_replacement(&store, &pool, &retired, &retry).await?;
    let retired_source_facts: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .fetch_one(&pool)
    .await?;
    let fresh_attempts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.request))
    .fetch_all(&pool)
    .await?;

    assert_check_violation(stranded);
    assert_eq!(lost_source_facts, ("in_flight".to_owned(), None, None));
    assert_eq!(
        lost_source_current,
        vec![uuid(INITIAL_PHYSICAL_ATTEMPT.attempt)]
    );
    assert_eq!(
        retired_source_facts,
        (
            "terminal".to_owned(),
            Some("known_failed".to_owned()),
            Some("crash_lost".to_owned())
        )
    );
    assert_eq!(fresh_attempts, vec![uuid(RETRY_PHYSICAL_ATTEMPT.attempt)]);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv004_inv043_idempotent_claimed_loss_retires_physical_attempt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: permission_overrides(RunnerToolPermissionOverride::Auto),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/idempotent".to_owned())
                .expect("the idempotent fixture directory is valid"),
            None,
            authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::ExternalEffect),
            offer_request(),
        )
        .expect("the idempotent registration pins its external-effect attempt");
    store.store_pin(&pin, &registration).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact idempotent lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed idempotent work admits a checked retry");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;
    let replacement =
        authorize_fixture_claimed_retry(&store, &loss, ToolEffectClass::ExternalEffect).await?;
    let (_batch, retired, retry_authorization) = replacement.into_parts();
    let retry = pin
        .placement
        .offer_retry(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            loss,
            retry_authorization,
        )
        .expect("claimed idempotent work re-leases at the successor generation");
    store_fixture_claimed_retry_replacement(&store, &pool, &retired, &retry).await?;
    let retired_facts: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .fetch_one(&pool)
    .await?;
    let fresh_attempt: Uuid = sqlx::query_scalar(
        "SELECT attempt_id
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(uuid(RETRY_PHYSICAL_ATTEMPT.request))
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        retired_facts,
        ("terminal".to_owned(), Some("ambiguous".to_owned()), None)
    );
    assert_eq!(fresh_attempt, uuid(RETRY_PHYSICAL_ATTEMPT.attempt));
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
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
    let offered = duplicate_lease(&pin.lease, registration.registration());
    store.store_pin(&pin, &registration).await?;
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed.lose().expect("the claimed pure lease may be lost");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;
    let lost = store
        .load_lease_loss(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("the durable loss reconstitutes checked retry authority");
    assert_eq!(
        lost.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    let initially_prepared =
        authorize_fixture_claimed_retry(&store, &lost, ToolEffectClass::EffectFree).await?;
    let reserved = store
        .load_claimed_retry_attempt_reservation(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("the interrupted retry retains its exact durable reservation");
    assert_eq!(reserved, initially_prepared.replacement());
    let resumable_loss = store
        .load_lease_loss(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("a reservation without its attempt remains resumable");
    let resumed_replacement = resumable_loss
        .retry()
        .expect("the incomplete reservation has not consumed retry authority")
        .prepare_claimed_attempt(
            claimed_batch_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::EffectFree),
            reserved.attempt(),
        )
        .expect("the exact reserved replacement can be reconstructed");
    store
        .store_claimed_retry_attempt_authority(&resumable_loss, &resumed_replacement)
        .await?;
    let (_batch, retired, retry_authorization) = resumed_replacement.into_parts();
    let retry = pin
        .placement
        .offer_retry(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            resumable_loss,
            retry_authorization,
        )
        .expect("claimed pure work requires a fresh physical attempt");
    store_fixture_claimed_retry_replacement(&store, &pool, &retired, &retry).await?;
    let consumed_loss = store
        .load_lease_loss(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("the consumed durable loss remains readable");
    let duplicate_preparation = consumed_loss
        .retry()
        .expect("the consumed loss retains its retry identity")
        .prepare_claimed_attempt(
            claimed_batch_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::EffectFree),
            ToolAttemptId::from_uuid(uuid(RETRY_ATTEMPT)),
        );
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

    assert_eq!(duplicate_preparation, Err(RunnerDomainError::InvalidState));
    assert_eq!(reconstituted, retry);
    assert_eq!(batch_attempts, vec![retry.attempt().into_uuid()]);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_adapter_rejects_caller_reconstituted_no_execution_proof()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let correlation = pin.lease.correlation();
    let credential_authorization = pin.lease.credential_authorization().cloned();
    let reconstructed = RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease: correlation.lease,
            dispatch: correlation.dispatch,
            runner: correlation.runner,
            tool: correlation.tool.clone(),
            effect: pin.lease.effect(),
            credential_authorization: credential_authorization.clone(),
            generation: correlation.generation,
            state: signalbox_domain::RunnerLeaseState::LostUnclaimed,
            recorded_correlation: correlation.clone(),
            recorded_session: correlation.dispatch.session(),
            recorded_effect: pin.lease.effect(),
            recorded_credential_authorization: credential_authorization,
            recorded_state: signalbox_domain::RunnerLeaseState::LostUnclaimed,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration.registration(),
    )
    .expect("the caller-controlled loss facts are internally correlated");
    let forged = reconstructed
        .into_reconstituted_loss(Some(correlation), RunnerLeaseRetryPreparation::Available)
        .expect("the copied correlation fabricates process-local proof");
    let rejected = store
        .store_lease_loss(&forged)
        .await
        .expect_err("caller-reconstituted facts cannot originate durable no-execution proof");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_unclaimed_retry_authority_survives_reconstitution() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let correlation = pin.lease.correlation();
    let mut durable_loss = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'lost_unclaimed')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *durable_loss)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 2
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *durable_loss)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_no_execution_proof
            (lease_id, generation, attempt_id, session_id,
             runner_id, tool_name, turn_id,
             issuing_turn_attempt_id, request_id, dispatch_generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(correlation.tool.as_str())
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(correlation.dispatch.request().into_uuid())
    .bind(Decimal::from(correlation.dispatch.generation().as_u64()))
    .execute(&mut *durable_loss)
    .await?;
    durable_loss.commit().await?;
    let restored = store
        .load_lease_loss(
            RunnerLeaseId::from_uuid(uuid(LEASE)),
            RunnerGeneration::one(),
        )
        .await?
        .expect("the durable proof restores unclaimed retry authority");

    assert_eq!(
        restored.lost().state(),
        signalbox_domain::RunnerLeaseState::LostUnclaimed
    );
    assert_eq!(
        restored
            .no_execution_proof()
            .expect("the restored unclaimed loss retains its proof")
            .correlation(),
        &restored.lost().correlation()
    );
    assert_eq!(
        restored
            .retry()
            .expect("the restored unclaimed loss is retryable")
            .generation(),
        RunnerGeneration::try_from_u64(2).expect("two is positive")
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_unclaimed_loss_requires_live_source_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_store, _, _, pin) = stored_pin_fixture(&pool).await?;
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let correlation = pin.lease.correlation();

    let rejected = sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'lost_unclaimed')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&pool)
    .await
    .expect_err("a lost-unclaimed event requires its live never-executed source attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_inv043_retryable_loss_serializes_with_attempt_termination()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact first lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed pure work may enter durable retry classification");
    let mut termination = pool.begin().await?;
    sqlx::query(
        "SELECT attempt_id
           FROM tool_attempt
          WHERE attempt_id = $1
            FOR UPDATE",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .fetch_one(&mut *termination)
    .await?;
    let mut loss_store = Box::pin(store.store_lease_loss(&loss));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut loss_store)
        .await
        .expect_err("the retryable loss must wait for the locked source attempt row");
    termination.commit().await?;
    loss_store.await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_inv043_first_generation_requires_null_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let malformed = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT $2, 1, attempt_id, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, 0
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(pin.lease.correlation().lease.into_uuid())
    .bind(uuid(LEASE + 99))
    .execute(&pool)
    .await
    .expect_err("the first lease generation cannot name a predecessor");
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let database_error = malformed
        .as_database_error()
        .expect("PostgreSQL reports the predecessor constraint");

    assert_eq!(database_error.code().as_deref(), Some("23514"));
    assert_eq!(
        database_error.constraint(),
        Some("runner_lease_predecessor_shape")
    );
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: Some(profile()),
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: no_permission_overrides(),
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
    let offered = duplicate_lease(&pin.lease, registration.registration());
    store.store_pin(&pin, &registration).await?;
    let correlation = offered.correlation();
    let claimed = offered
        .claim(correlation)
        .expect("the exact lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed.lose().expect("the claimed pure lease may be lost");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;

    let error = sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT lease_id, generation + 1, attempt_id, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, generation
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = 1",
    )
    .bind(uuid(LEASE))
    .execute(&pool)
    .await
    .expect_err("claimed retry cannot reuse its physical attempt identity");

    assert_check_violation(error);
    let _replacement =
        authorize_fixture_claimed_retry(&store, &loss, ToolEffectClass::EffectFree).await?;
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
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, 'idempotent', placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, NULL
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
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT $2, 1, $3, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, NULL
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
        "ALTER TABLE tool_attempt
         DISABLE TRIGGER tool_attempt_requires_approval",
    )
    .execute(&pool)
    .await?;
    let mut valid_retry = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT lease_id, 2, $2, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, 1
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
    sqlx::query(
        "ALTER TABLE tool_attempt
         ENABLE TRIGGER tool_attempt_requires_approval",
    )
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
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         SELECT lease_id, 3, $2, session_id, runner_id,
                tool_name, effect_class, placement_event_ordinal,
                registration_enrollment_id, registration_revision,
                credential_profile_name,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision, credential_approval_kind, 2
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
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
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
        .load_registration(&expected_enrollment, stored.revision())
        .await
        .expect_err("cross-wired canonical identity fails closed");

    assert!(matches!(error, RunnerProtocolStoreError::Domain(_)));
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_reconstitution_requires_trusted_catalog_declarations()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query("ALTER TABLE runner_registration_tool DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_registration_tool
            SET model_input_schema = $4
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .bind(r#"{"different":0}"#)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = store
        .load_registration(&expected_enrollment, stored.revision())
        .await
        .expect_err("stored declarations cannot bootstrap their own catalog authority");

    assert_store_domain_error(error, RunnerDomainError::CorruptStoredFacts);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_reconstitution_rejects_noncanonical_tool_schema() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query("ALTER TABLE runner_registration_tool DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = sqlx::query(
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
    .await
    .expect_err("noncanonical schema text is rejected at the durable boundary");
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_idempotent_registration_tool_requires_runner_only_locus()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
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
async fn s30_inv042_registration_tool_requires_selector_discriminator() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query(
        "ALTER TABLE runner_registration_tool
         DISABLE TRIGGER runner_registration_tool_is_append_only",
    )
    .execute(&pool)
    .await?;
    let missing_discriminator = sqlx::query(
        "UPDATE runner_registration_tool
            SET selector_kind = NULL
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND tool_name = $3",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("a stored selector payload requires its closed discriminator");
    sqlx::query(
        "ALTER TABLE runner_registration_tool
         ENABLE TRIGGER runner_registration_tool_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(missing_discriminator);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv042_registration_profile_approval_requires_tool_name_shape()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;
    sqlx::query(
        "ALTER TABLE runner_registration_profile_approval
         DISABLE TRIGGER runner_registration_profile_approval_is_append_only",
    )
    .execute(&pool)
    .await?;
    let invalid_tool = sqlx::query(
        "UPDATE runner_registration_profile_approval
            SET tool_name = ''
          WHERE enrollment_id = $1
            AND registration_revision = $2
            AND credential_profile_name = $3
            AND tool_name = $4",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(stored.revision().get()))
    .bind(profile().as_str())
    .bind(tool("inspect").as_str())
    .execute(&pool)
    .await
    .expect_err("profile approval tools use the checked ToolName vocabulary");
    sqlx::query(
        "ALTER TABLE runner_registration_profile_approval
         ENABLE TRIGGER runner_registration_profile_approval_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(invalid_tool);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_inv001_reconstitution_rejects_cross_wired_enrollment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
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
