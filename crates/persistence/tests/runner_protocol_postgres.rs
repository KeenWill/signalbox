//! Feature-gated PostgreSQL coverage for runner enrollment, leases, placement, and grants.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, ApprovedToolRequest,
    CancelledModelCallTurnIdentities, CanonicalCloneUrlDigest, ContextFrontierId, CreateSession,
    CredentialProfileGrant, CredentialProfileGrantReconstitutionInput, CredentialProfileName,
    CredentialProfilePolicy, CredentialToolApproval, DecideToolRequest, DeliveryRequest,
    DescendantTerminationScope, DirectModelSelection, DurableCommandId, EndedToolAttempt,
    ModelCallId, ModelSelectionOverride, ModelSelectionRequest, NormalizedToolArguments,
    PerInputConfigurationChoices, ProvisionedWorkspace, ResolvedContextFrontierReconstitutionInput,
    RunnerAdvertisement, RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog,
    RunnerDomainError, RunnerEnrollment, RunnerEnrollmentId, RunnerGeneration, RunnerId,
    RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId, RunnerLeaseOfferRequest,
    RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation, RunnerLostBeforePin,
    RunnerPlacementReconstitutionHistory, RunnerRepositoryEntry, RunnerSandboxProfile,
    RunnerSelector, RunnerToolAttemptAuthorization, RunnerToolDeclaration, RunnerToolEffectClass,
    RunnerToolModelDefinition, RunnerToolPermissionOverride, RunnerToolPermissionOverrides,
    RunnerWorkingDirectory, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionRunnerPin, SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, SubmitInput, ToolAdmissibleLoci,
    ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolBatch,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestId, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TranscriptAncestry, TurnAttemptId, TurnId,
    TurnInstructionManifest, TurnInstructionManifestId, UserContent, ValidatedRunnerRegistration,
    WorkingDirectorySelection, WorkspaceCapability, WorkspaceManifestId, WorkspaceRecovery,
    WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement, WorkspaceRevision,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    outbox::{
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedRunnerState, OutboxCorruption,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
        RunnerStateTransitionOutboxTestEvent, RunnerStateTransitionOutboxTestSource,
        append_runner_state_transition_for_test,
    },
    process_read::{
        ProcessReadRepository, ProcessRunnerConnectionHealth, ProcessRunnerProjectionState,
    },
    runner_protocol::{
        RunnerConnectionCause, RunnerConnectionEpoch, RunnerConnectionLossSessionDisposition,
        RunnerConnectionState, RunnerConnectionTransition, RunnerProtocolCorruption,
        RunnerProtocolStore, RunnerProtocolStoreError, StoredValidatedRunnerRegistration,
    },
    session_credentials::{SessionCredentialPin, SessionModelCredential},
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

mod support;

use support::blocked_backends_reached;

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
const SECOND_SESSION: u128 = 0x9402;
const LEASE: u128 = 0x9500;
const ATTEMPT: u128 = 0x9600;
const RETRY_ATTEMPT: u128 = 0x9601;
const FOREIGN_RUNNER: u128 = 0x9202;
const RELATED_IDENTITY_OFFSET: u128 = 0x100;
const LOCK_WAIT_PROBE: Duration = Duration::from_millis(100);
const LOCK_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const SERIALIZATION_TEST_TIMEOUT: Duration = Duration::from_secs(90);
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
const SECOND_SESSION_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9606,
    request: 0x9704,
    turn: 0x9804,
};

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_db_name(DATABASE_NAME)
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
    let connection_options =
        local_test_connection_options(&database_url)?.statement_cache_capacity(0);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(connection_options)
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

async fn insert_empty_instruction_manifest(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<TurnInstructionManifestId, sqlx::Error> {
    let manifest_id = TurnInstructionManifestId::from_uuid(turn.into_uuid());
    let manifest = TurnInstructionManifest::empty_turn_start(manifest_id, session, turn);
    sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id,
             limit_set_version, classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 2, 0, 0, 0, 0, true)",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO turn_instruction_manifest
            (turn_instruction_manifest_id, session_id, turn_id,
             instruction_discovery_id, boundary_kind,
             eligibility_hash_algorithm, eligibility_hash,
             admitted_set_hash_algorithm, admitted_set_hash,
             manifest_hash_algorithm, manifest_hash)
         VALUES ($1, $2, $3, $4, 'turn_start',
                 'sha256_v1', $5, 'sha256_v1', $6, 'sha256_v1', $7)",
    )
    .bind(manifest_id.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(turn.into_uuid())
    .bind(manifest.eligibility_hash().as_bytes().as_slice())
    .bind(manifest.admitted_set_hash().as_bytes().as_slice())
    .bind(manifest.manifest_hash().as_bytes().as_slice())
    .execute(&mut *connection)
    .await?;
    Ok(manifest_id)
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

fn daemon_fallback_permission_overrides() -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([(
        tool("daemon_fallback"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the omitted combined-tool override fixture is valid")
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
    approved_request_for_session(SessionId::from_uuid(uuid(SESSION)), facts)
}

fn approved_request_for_session(
    session: SessionId,
    facts: PhysicalAttemptFacts,
) -> ApprovedToolRequest {
    let request = ToolRequestReconstitutionInput::new(
        ToolRequestId::from_uuid(uuid(facts.request)),
        session,
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
        .expect("the fixture request and user decision correlate");
    let signalbox_domain::DecideToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("the approving fixture user decision applies")
    };
    ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
        .expect("the fixture user approval matches its request")
}

fn authorization_from_approved(
    approved: ApprovedToolRequest,
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved_for_session(
        approved,
        SessionId::from_uuid(uuid(SESSION)),
        facts,
        effect,
    )
}

fn authorization_from_approved_for_session(
    approved: ApprovedToolRequest,
    session: SessionId,
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    let attempt_id = ToolAttemptId::from_uuid(uuid(facts.attempt));
    let attempt = ToolAttemptReconstitutionInput::new(
        attempt_id,
        ToolRequestId::from_uuid(uuid(facts.request)),
        session,
        TurnId::from_uuid(uuid(facts.turn)),
        TurnAttemptId::from_uuid(uuid(facts.turn + RELATED_IDENTITY_OFFSET)),
        effect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::InFlight,
    )
    .reconstitute()
    .expect("the fixture in-flight attempt reconstitutes");
    let batch = ToolBatchReconstitutionInput::new(
        session,
        TurnId::from_uuid(uuid(facts.turn)),
        ModelCallId::from_uuid(uuid(facts.turn + (RELATED_IDENTITY_OFFSET * 2))),
        ResolvedContextFrontierReconstitutionInput::new(
            session,
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

fn authorized_for_session(
    session: SessionId,
    facts: PhysicalAttemptFacts,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved_for_session(
        approved_request_for_session(session, facts),
        session,
        facts,
        ToolEffectClass::EffectFree,
    )
}

fn external_authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::ExternalEffect)
}

fn idempotent_authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::ExternalEffect)
}

fn offer_request() -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(LEASE)),
        tool: tool("inspect"),
    }
}

fn offer_request_for(lease: u128) -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(lease)),
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
            history: RunnerPlacementReconstitutionHistory::Initial,
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

fn exact_runner_directory() -> RunnerWorkingDirectory {
    RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
        .expect("the exact fixture directory is valid")
}

fn replacement_runner_directory() -> RunnerWorkingDirectory {
    RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
        .expect("the replacement fixture directory is valid")
}

fn exact_runner_request(runner: RunnerId) -> SessionRunnerPlacementRequest {
    exact_runner_request_with_directory(runner, exact_runner_directory())
}

fn exact_runner_request_with_directory(
    runner: RunnerId,
    working_directory: RunnerWorkingDirectory,
) -> SessionRunnerPlacementRequest {
    SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(runner),
        working_directory: WorkingDirectorySelection::Exact(working_directory),
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    }
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
    let daemon_fallback = RunnerToolDeclaration::new(
        tool("daemon_fallback"),
        model_definition(),
        ToolPermissionDefault::Confirm,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::DaemonOrRunner {
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
        [inspect, catalog_only, daemon_fallback],
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

fn always_confirm_catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition(),
        ToolPermissionDefault::AlwaysConfirm,
        RunnerToolEffectClass::Pure,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the explicit-approval fixture profile references its declared tool");
    let replacement_policy = CredentialProfilePolicy::try_new(
        replacement_profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the replacement profile references the explicit-approval tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect],
        [policy, replacement_policy],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
    )
    .expect("the explicit-approval fixture catalog is internally consistent")
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

fn side_effecting_catalog() -> RunnerCatalog {
    let inspect = RunnerToolDeclaration::new(
        tool("inspect"),
        model_definition(),
        ToolPermissionDefault::Auto,
        RunnerToolEffectClass::SideEffecting,
        ToolAdmissibleLoci::RunnerOnly {
            selector: RunnerSelector::CapabilityClass(class()),
        },
    );
    let policy = CredentialProfilePolicy::try_new(
        profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the side-effecting fixture profile references its declared tool");
    let replacement_policy = CredentialProfilePolicy::try_new(
        replacement_profile(),
        [(tool("inspect"), CredentialToolApproval::SessionPolicy)],
    )
    .expect("the side-effecting replacement profile references its declared tool");
    RunnerCatalog::try_new(
        [class()],
        [inspect],
        [policy, replacement_policy],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
    )
    .expect("the side-effecting fixture catalog is internally consistent")
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
    stored_pin_fixture_with_authorization(
        pool,
        authorized,
        catalog(),
        no_permission_overrides(),
        "effect_free",
    )
    .await
}

enum ActivePinEffectCase {
    EffectFree,
    IdempotentExternalEffect,
    SideEffectingExternalEffect,
}

async fn stored_active_pin_fixture_with_authorization(
    pool: &PgPool,
    effect_case: ActivePinEffectCase,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
        RunnerConnectionEpoch,
    ),
    Box<dyn Error>,
> {
    let (authorize, fixture_catalog, fixture_overrides, fixture_effect_kind): (
        fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
        RunnerCatalog,
        RunnerToolPermissionOverrides,
        &'static str,
    ) = match effect_case {
        ActivePinEffectCase::EffectFree => (
            authorized,
            catalog(),
            no_permission_overrides(),
            "effect_free",
        ),
        ActivePinEffectCase::IdempotentExternalEffect => (
            idempotent_authorized,
            idempotent_catalog(),
            permission_overrides(RunnerToolPermissionOverride::Auto),
            "external_effect",
        ),
        ActivePinEffectCase::SideEffectingExternalEffect => (
            external_authorized,
            side_effecting_catalog(),
            permission_overrides(RunnerToolPermissionOverride::Auto),
            "external_effect",
        ),
    };
    let (session, turn, turn_attempt) = insert_running_turn(pool).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.turn + (RELATED_IDENTITY_OFFSET * 2),
    ));
    let boundary = ContextFrontierId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.turn + (RELATED_IDENTITY_OFFSET * 3),
    ));
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1 AND state_kind = 'prepared'",
    )
    .bind(turn_attempt.into_uuid())
    .execute(pool)
    .await?;
    attach_continuing_tool_round_projection(
        pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        boundary,
    )
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_tool_round_call_id = $1
          WHERE session_id = $2 AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    insert_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    set_fixture_physical_attempt_effect(pool, INITIAL_PHYSICAL_ATTEMPT, fixture_effect_kind)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), fixture_catalog);
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: fixture_overrides,
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the active fixture working directory is valid"),
            None,
            authorize(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the active fixture registration pins the placement");
    store.store_pin(&pin, &registration).await?;
    Ok((
        store,
        expected_enrollment,
        registration,
        pin,
        connection.epoch(),
    ))
}

async fn stored_side_effecting_pin_fixture(
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
    stored_pin_fixture_with_authorization(
        pool,
        external_authorized,
        side_effecting_catalog(),
        permission_overrides(RunnerToolPermissionOverride::Auto),
        "external_effect",
    )
    .await
}

async fn stored_pin_fixture_with_authorization(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_overrides: RunnerToolPermissionOverrides,
    fixture_effect_kind: &'static str,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
    ),
    Box<dyn Error>,
> {
    let (store, expected_enrollment, registration, pin) = prepared_pin_fixture_with_authorization(
        pool,
        authorize,
        fixture_catalog,
        fixture_overrides,
        fixture_effect_kind,
    )
    .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store.store_pin(&pin, &registration).await?;
    Ok((store, expected_enrollment, registration, pin))
}

async fn prepared_pin_fixture_with_authorization(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_overrides: RunnerToolPermissionOverrides,
    fixture_effect_kind: &'static str,
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
    set_fixture_physical_attempt_effect(pool, INITIAL_PHYSICAL_ATTEMPT, fixture_effect_kind)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), fixture_catalog);
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
            permission_overrides: fixture_overrides,
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
            authorize(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration pins the placement");
    Ok((store, expected_enrollment, registration, pin))
}

async fn stored_credentialless_pin_fixture(
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
    store
        .open_connection(expected_enrollment.enrollment())
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
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the credentialless registration pins the placement");
    store.store_pin(&pin, &registration).await?;
    Ok((store, expected_enrollment, registration, pin))
}

async fn store_additional_credentialless_pin_fixture(
    pool: &PgPool,
    store: &RunnerProtocolStore,
    expected_enrollment: &RunnerEnrollment,
    registration: &StoredValidatedRunnerRegistration,
) -> Result<SessionRunnerPin, Box<dyn Error>> {
    let session = SessionId::from_uuid(uuid(SECOND_SESSION));
    insert_session_for(pool, session.into_uuid()).await?;
    insert_physical_attempt_for(pool, session, SECOND_SESSION_PHYSICAL_ATTEMPT).await?;
    let placement = SessionRunnerPlacement::new(
        session,
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
            expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second-session".to_owned())
                .expect("the second fixture working directory is valid"),
            None,
            authorized_for_session(session, SECOND_SESSION_PHYSICAL_ATTEMPT),
            offer_request_for(LEASE + RELATED_IDENTITY_OFFSET),
        )
        .expect("the second fixture registration pins the placement");
    store.store_pin(&pin, registration).await?;
    Ok(pin)
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
    stored_later_lease_fixture_with_authorization(
        pool,
        authorized,
        catalog(),
        no_permission_overrides(),
        "effect_free",
    )
    .await
}

async fn stored_side_effecting_later_lease_fixture(
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
    stored_later_lease_fixture_with_authorization(
        pool,
        external_authorized,
        side_effecting_catalog(),
        permission_overrides(RunnerToolPermissionOverride::Auto),
        "external_effect",
    )
    .await
}

async fn stored_later_lease_fixture_with_authorization(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_overrides: RunnerToolPermissionOverrides,
    fixture_effect_kind: &'static str,
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
    let (store, expected_enrollment, registration, pin) = stored_pin_fixture_with_authorization(
        pool,
        authorize,
        fixture_catalog,
        fixture_overrides,
        fixture_effect_kind,
    )
    .await?;
    terminalize_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    insert_physical_attempt(pool, LATER_LEASE_PHYSICAL_ATTEMPT).await?;
    set_fixture_physical_attempt_effect(pool, LATER_LEASE_PHYSICAL_ATTEMPT, fixture_effect_kind)
        .await?;
    let lease = pin
        .placement
        .offer_lease(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            authorize(LATER_LEASE_PHYSICAL_ATTEMPT),
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

/// The `creation_cause` a fixture writes.
///
/// `202608110001_user_role_storage_vocabulary` renamed the stored value, so a
/// fixture seeding a pool held at an earlier migration by `MIGRATOR.run_to`
/// must write the retired spelling: the `CHECK` in force there admits nothing
/// else, and the insert fails with `23514` before the migration under test
/// runs. Fully migrated pools take the current spelling.
const CURRENT_CREATION_CAUSE: &str = "interactive";
async fn insert_session_for(pool: &PgPool, session: Uuid) -> Result<(), sqlx::Error> {
    insert_session_for_with_creation_cause(pool, session, CURRENT_CREATION_CAUSE).await
}

async fn insert_session_for_with_creation_cause(
    pool: &PgPool,
    session: Uuid,
    creation_cause: &str,
) -> Result<(), sqlx::Error> {
    // One transaction: the lifecycle row, its ownership journal entry, and the
    // deferred invariant that ties them together all belong to the same commit,
    // which is how every production creation writes them.
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES ($1, $2, 'none')",
    )
    .bind(session)
    .bind(creation_cause)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ($1, 'created', false, false, 'operator')",
    )
    .bind(session)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ($1, 1, 'created_unmonitored', false, 'operator')",
    )
    .bind(session)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE session ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ($1)
         ON CONFLICT (session_id) DO NOTHING",
    )
    .bind(session)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_session(pool: &PgPool) -> Result<(), sqlx::Error> {
    insert_session_for(pool, uuid(SESSION)).await
}

fn propagation_session(ordinal: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(
        0xa200_0000_0000_0000_0000_0000_0000_0000 + ordinal,
    ))
}

async fn insert_uncommitted_exact_placement(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    runner: RunnerId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'workspace_restricted', 0, 'unpinned', 0)",
    )
    .bind(session.into_uuid())
    .bind(runner.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement
            (session_id, event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_bounded_propagation_session_fixture(
    pool: &PgPool,
    runner: RunnerId,
) -> Result<Vec<SessionId>, sqlx::Error> {
    let sessions: Vec<_> = (1..=65).map(propagation_session).collect();
    let session_uuids: Vec<_> = sessions.iter().copied().map(SessionId::into_uuid).collect();
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         SELECT session_id, 'interactive', 'none'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         SELECT session_id, 'created', false, false, 'operator'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         SELECT session_id, 1, 'created_unmonitored', false, 'operator'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE session ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         SELECT session_id FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count)
         SELECT session_id, 1, 1, 'created', 'identity', $2,
                'runner_default', 'none', 'workspace_restricted', 0,
                'unpinned', 0
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .bind(runner.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement
            (session_id, event_ordinal)
         SELECT session_id, 1
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(sessions)
}

async fn project_bounded_propagation_sessions(
    pool: &PgPool,
    sessions: &[SessionId],
) -> Result<(), sqlx::Error> {
    let session_uuids: Vec<_> = sessions.iter().copied().map(SessionId::into_uuid).collect();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT placement.session_id, placement.event_ordinal + 1,
                placement.placement_revision, 'runner_lost_before_pin',
                placement.selector_kind, placement.selector_runner_id,
                placement.selector_capability_class,
                placement.directory_selection_kind,
                placement.requested_working_directory,
                placement.requested_credential_profile_name,
                placement.workspace_requirement_kind,
                placement.requested_repository_key,
                placement.requested_sandbox_profile,
                placement.permission_override_count,
                'runner_lost_before_pin', placement.selector_runner_id,
                NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL
           FROM runner_session_placement_record AS placement
           JOIN runner_current_session_placement AS current_placement
             ON current_placement.session_id = placement.session_id
            AND current_placement.event_ordinal = placement.event_ordinal
          WHERE placement.session_id = ANY($1::uuid[])",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = ANY($1::uuid[])",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn dispatch_next_outbox_event(
    pool: &PgPool,
) -> Result<DispatchedOutboxEvent, Box<dyn Error>> {
    dispatch_next_outbox_event_at(pool, 1).await
}

async fn dispatch_next_outbox_event_at(
    pool: &PgPool,
    expected_sequence: u64,
) -> Result<DispatchedOutboxEvent, Box<dyn Error>> {
    let mut dispatched = None;
    let outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            dispatched = Some(event.clone());
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        outcome,
        OutboxDispatchOutcome::Delivered {
            sequence: expected_sequence,
        }
    );
    Ok(dispatched.expect("the delivered outcome carries its decoded event"))
}

async fn placement_outbox_facts(
    pool: &PgPool,
    session: SessionId,
    event_kind: &str,
) -> Result<(u64, RunnerGeneration), Box<dyn Error>> {
    let (event_ordinal, placement_revision): (Decimal, Decimal) = sqlx::query_as(
        "SELECT event_ordinal, placement_revision
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = $2",
    )
    .bind(session.into_uuid())
    .bind(event_kind)
    .fetch_one(pool)
    .await?;
    let event_ordinal = u64::try_from(event_ordinal.mantissa())?;
    let placement_revision = u64::try_from(placement_revision.mantissa())?;
    let placement_revision = RunnerGeneration::try_from_u64(placement_revision)
        .expect("the persisted placement fixture has a positive revision");
    Ok((event_ordinal, placement_revision))
}

async fn connection_outbox_source(
    pool: &PgPool,
    placement_event_ordinal: u64,
    enrollment: RunnerEnrollmentId,
    cause_kind: &str,
) -> Result<RunnerStateTransitionOutboxTestSource, Box<dyn Error>> {
    let (connection_epoch, event_ordinal): (Decimal, Decimal) = sqlx::query_as(
        "SELECT connection_epoch, event_ordinal
           FROM runner_connection_event
          WHERE enrollment_id = $1 AND cause_kind = $2",
    )
    .bind(enrollment.into_uuid())
    .bind(cause_kind)
    .fetch_one(pool)
    .await?;
    Ok(RunnerStateTransitionOutboxTestSource::connection(
        placement_event_ordinal,
        enrollment,
        RunnerConnectionEpoch::try_from_u64(u64::try_from(connection_epoch.mantissa())?)
            .expect("the persisted connection epoch is positive"),
        NonZeroU64::new(u64::try_from(event_ordinal.mantissa())?)
            .expect("the persisted connection event ordinal is positive"),
    ))
}

async fn insert_physical_attempt(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    insert_physical_attempt_for(pool, SessionId::from_uuid(uuid(SESSION)), facts).await
}

async fn insert_physical_attempt_for(
    pool: &PgPool,
    session: SessionId,
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
    .bind(session.into_uuid())
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
    .bind(session.into_uuid())
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

async fn set_fixture_physical_attempt_effect(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
    effect_kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET effect_class = $2
          WHERE attempt_id = $1",
    )
    .bind(uuid(facts.attempt))
    .bind(effect_kind)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn replace_approval_with_user_command(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    let command = uuid(facts.request + (RELATED_IDENTITY_OFFSET * 4));
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
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
            SET decision_source = 'user_command',
                user_command_id = $2
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

async fn insert_user_override_approval(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source,
             override_denied_request_id)
         VALUES ($1, 'approve', 'user_override', $2)",
    )
    .bind(uuid(facts.request))
    .bind(uuid(facts.request + (RELATED_IDENTITY_OFFSET * 5)))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
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
    loss_source: Option<&str>,
    lost_runner: Option<RunnerId>,
    interrupted_tool_attempt: Option<ToolAttemptId>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             interrupted_tool_attempt_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
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
                'runner_lost', COALESCE($3, pinned_runner_id), $2,
                pinned_runner_id,
                $4,
                pinned_working_directory, pinned_credential_profile_name,
                registration_enrollment_id, registration_revision,
                pinned_tool_count, workspace_repository_key,
                workspace_working_directory, workspace_manifest_id,
                workspace_placement_revision,
                workspace_clone_url_digest, workspace_credential_profile_name,
                workspace_sandbox_profile, workspace_relative_path,
                workspace_recovery_kind, workspace_branch_name, workspace_revision,
                credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .bind(loss_source)
    .bind(lost_runner.map(RunnerId::into_uuid))
    .bind(interrupted_tool_attempt.map(ToolAttemptId::into_uuid))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
         SELECT session_id, event_ordinal + 1, tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn append_runner_lost_projection(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut transaction,
        session,
        Some("connection"),
        None,
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn mark_interrupted_attempt_ambiguous(
    pool: &PgPool,
    interrupted_tool_attempt: ToolAttemptId,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET effect_class = 'external_effect', state_kind = 'terminal',
                terminal_disposition_kind = 'ambiguous', error_kind = NULL
          WHERE attempt_id = $1",
    )
    .bind(interrupted_tool_attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn record_execution_possible_lease_loss(
    pool: &PgPool,
    lease: &RunnerLease,
) -> Result<(), sqlx::Error> {
    let correlation = lease.correlation();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'lost_execution_possible')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 2
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn record_no_execution_lease_loss(
    pool: &PgPool,
    lease: &RunnerLease,
) -> Result<(), sqlx::Error> {
    let correlation = lease.correlation();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'lost_unclaimed')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 2
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
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
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn insert_runner_recovery_turn(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    interrupted_tool_attempt: Option<ToolAttemptId>,
    active_tool_round_call: Option<ModelCallId>,
) -> Result<(), sqlx::Error> {
    let starting_frontier =
        ContextFrontierId::from_uuid(uuid(turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET));
    let yielded_attempt = uuid(turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .execute(pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         ENABLE TRIGGER turn_lifecycle_runner_recovery_is_complete",
    )
    .execute(&mut *transaction)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind, start_lineage_kind,
             starting_frontier_id, active_phase_kind,
             active_tool_round_call_id, runner_recovery_runner_id,
             runner_recovery_placement_revision,
             runner_recovery_tool_attempt_id)
         VALUES ($1, $2, 'delegation', NULL, 1, 'active',
                 'first_in_session', $3, 'awaiting_runner_recovery',
                 $4, $5, $6, $7)",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(active_tool_round_call.map(ModelCallId::into_uuid))
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(interrupted_tool_attempt.map(ToolAttemptId::into_uuid))
    .execute(&mut *transaction)
    .await;
    inserted?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'ended', 'without_stop',
                 'yielded_to_durable_wait')",
    )
    .bind(yielded_attempt)
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

struct InterruptedLossRecoveryFacts {
    session: SessionId,
    turn: TurnId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    placement_interrupted_tool_attempt: ToolAttemptId,
    recovery_interrupted_tool_attempt: Option<ToolAttemptId>,
    active_tool_round_call: ModelCallId,
}

async fn insert_runner_recovery_turn_with_interrupted_loss(
    pool: &PgPool,
    facts: InterruptedLossRecoveryFacts,
) -> Result<(), sqlx::Error> {
    insert_runner_recovery_turn_with_interrupted_loss_boundary(pool, facts, "continuing").await
}

async fn insert_runner_recovery_turn_with_interrupted_loss_boundary(
    pool: &PgPool,
    facts: InterruptedLossRecoveryFacts,
    boundary_kind: &str,
) -> Result<(), sqlx::Error> {
    let starting_frontier = ContextFrontierId::from_uuid(uuid(
        facts.turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET,
    ));
    let yielded_attempt = uuid(facts.turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(facts.session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .execute(pool)
    .await?;
    let mut transaction = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut transaction,
        facts.session,
        Some("connection"),
        None,
        Some(facts.placement_interrupted_tool_attempt),
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(facts.session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE model_call DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         ENABLE TRIGGER turn_lifecycle_runner_recovery_is_complete",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind, start_lineage_kind,
             starting_frontier_id, active_phase_kind,
             pinned_provider_model_identity_id,
             active_tool_round_call_id, runner_recovery_runner_id,
             runner_recovery_placement_revision,
             runner_recovery_tool_attempt_id)
         VALUES ($1, $2, 'delegation', NULL, 1, 'active',
                 'first_in_session', $3, 'awaiting_runner_recovery',
                 $4, $5, $6, $7, $8)",
    )
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(uuid(0xa159))
    .bind(facts.active_tool_round_call.into_uuid())
    .bind(facts.runner.into_uuid())
    .bind(Decimal::from(facts.placement_revision.get()))
    .bind(
        facts
            .recovery_interrupted_tool_attempt
            .map(ToolAttemptId::into_uuid),
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'ended', 'without_stop',
                 'yielded_to_durable_wait')",
    )
    .bind(yielded_attempt)
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let instruction_manifest =
        insert_empty_instruction_manifest(&mut transaction, facts.session, facts.turn).await?;
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id,
             resolved_provider_model_identity_id, context_frontier_id,
             credential_reference, state_kind, terminal_disposition_kind,
             turn_instruction_manifest_id)
         VALUES ($1, $2, $3, $4, 'direct', $5, $6, $7,
                 'synthetic-runner-recovery-test', 'terminal', 'completed', $8)",
    )
    .bind(facts.active_tool_round_call.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .bind(yielded_attempt)
    .bind(uuid(0xa101))
    .bind(uuid(0xa159))
    .bind(starting_frontier.into_uuid())
    .bind(instruction_manifest.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, $4, $5, 1, 1)",
    )
    .bind(facts.active_tool_round_call.into_uuid())
    .bind(facts.session.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(boundary_kind)
    .bind(starting_frontier.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE model_call ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_running_turn(
    pool: &PgPool,
) -> Result<(SessionId, TurnId, TurnAttemptId), Box<dyn Error>> {
    let session = SessionId::from_uuid(uuid(SESSION));
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let attempt = TurnAttemptId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.turn + RELATED_IDENTITY_OFFSET,
    ));
    let selection = DirectModelSelection::from_uuid(uuid(0xa101));
    let credentials = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-model-family",
        "fixture-credential-reference",
    )])
    .expect("the fixture credential pin is valid");
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(uuid(0xa102)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(session)
    .expect("the fixture session creation is preparable");
    CreateSessionRepository::new(pool.clone(), credentials)
        .handle(creation)
        .await?;
    let starting_input = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0xa103)),
        session,
        UserContent::try_text(String::from("runner recovery fixture"))
            .expect("the fixture input is valid"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            starting_input,
            AcceptedInputId::from_uuid(uuid(0xa104)),
            Some(turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa105)),
                ContextFrontierId::from_uuid(uuid(0xa106)),
            ),
            |_| TurnId::from_uuid(uuid(0xa107)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa108))),
        )
        .await?;
    StartEligibleTurnRepository::new(pool.clone())
        .handle(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa109)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa10a)),
                ContextFrontierId::from_uuid(uuid(0xa10b)),
                attempt,
            ),
        )
        .await?;
    Ok((session, turn, attempt))
}

async fn attach_continuing_tool_round_projection(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    turn_attempt: TurnAttemptId,
    producing_call: ModelCallId,
    request: ToolRequestId,
    boundary: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    let (starting_frontier, source_member_count): (Uuid, Decimal) = sqlx::query_as(
        "SELECT lifecycle.starting_frontier_id, frontier.member_count
               FROM turn_lifecycle AS lifecycle
               JOIN context_frontier AS frontier
                 ON frontier.owning_session_id = lifecycle.session_id
                AND frontier.context_frontier_id = lifecycle.starting_frontier_id
              WHERE lifecycle.session_id = $1 AND lifecycle.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await?;
    let provider = uuid(0xa159);
    let assistant_entry = uuid(producing_call.into_uuid().as_u128() + 2);
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE model_call DISABLE TRIGGER ALL;
         ALTER TABLE tool_round DISABLE TRIGGER ALL;
         ALTER TABLE tool_request DISABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET pinned_provider_model_identity_id = $1
          WHERE session_id = $2 AND turn_id = $3",
    )
    .bind(provider)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(pool)
    .await?;
    let mut connection = pool.acquire().await?;
    let instruction_manifest =
        insert_empty_instruction_manifest(&mut connection, session, turn).await?;
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id,
             resolved_provider_model_identity_id, context_frontier_id,
             credential_reference, state_kind, terminal_disposition_kind,
             turn_instruction_manifest_id)
         VALUES ($1, $2, $3, $4, 'direct', $5, $6, $7,
                 'synthetic-runner-recovery-test', 'terminal', 'completed', $8)",
    )
    .bind(producing_call.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn_attempt.into_uuid())
    .bind(uuid(0xa101))
    .bind(provider)
    .bind(starting_frontier)
    .bind(instruction_manifest.into_uuid())
    .execute(&mut *connection)
    .await?;
    drop(connection);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id,
             prefix_context_frontier_id, member_count)
         VALUES ($1, $2, $3, $4 + 1)",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .bind(starting_frontier)
    .bind(source_member_count)
    .execute(pool)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
    )
    .bind(producing_call.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(boundary.into_uuid())
    .execute(pool)
    .await;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'inspect', 'json', '{}')
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(request.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(producing_call.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, 'approve', 'policy_auto', NULL, NULL)",
    )
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             producing_model_call_id, assistant_tool_request_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_tool_use', $3, $4, 0, NULL)",
    )
    .bind(session.into_uuid())
    .bind(assistant_entry)
    .bind(producing_call.into_uuid())
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, $3 + 1, $1, $4)",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .bind(source_member_count)
    .bind(assistant_entry)
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE model_call ENABLE TRIGGER ALL;
         ALTER TABLE tool_round ENABLE TRIGGER ALL;
         ALTER TABLE tool_request ENABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    inserted?;
    Ok(())
}

async fn append_denied_request_to_continuing_tool_round_projection(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    request: ToolRequestId,
    boundary: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    let member_count: Decimal = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .fetch_one(pool)
    .await?;
    let assistant_entry = uuid(request.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET);
    sqlx::raw_sql(
        "ALTER TABLE tool_round DISABLE TRIGGER ALL;
         ALTER TABLE tool_request DISABLE TRIGGER ALL;
         ALTER TABLE decide_tool_request_command DISABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE tool_round
            SET response_part_count = 2, request_count = 2
          WHERE producing_model_call_id = $1",
    )
    .bind(producing_call.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 1, 'inspect', 'json', '{}')",
    )
    .bind(request.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(producing_call.into_uuid())
    .execute(pool)
    .await?;
    let command = uuid(request.into_uuid().as_u128() + (RELATED_IDENTITY_OFFSET * 2));
    let mut decision = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *decision)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2, 'deny', NULL,
                 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(request.into_uuid())
    .execute(&mut *decision)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, 'deny', 'user_command', NULL, $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *decision)
    .await?;
    decision.commit().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             producing_model_call_id, assistant_tool_request_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_tool_use', $3, $4, 1, NULL)",
    )
    .bind(session.into_uuid())
    .bind(assistant_entry)
    .bind(producing_call.into_uuid())
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE context_frontier
            SET member_count = member_count + 1
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, $3 + 1, $1, $4)",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .bind(member_count)
    .bind(assistant_entry)
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE tool_round ENABLE TRIGGER ALL;
         ALTER TABLE tool_request ENABLE TRIGGER ALL;
         ALTER TABLE decide_tool_request_command ENABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn convert_running_turn_to_delegated_runner_recovery(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
) -> Result<ContextFrontierId, sqlx::Error> {
    let spawning_request = uuid(0xa130);
    let parent_session = uuid(FOREIGN_SESSION);
    let parent_turn = uuid(0xa132);
    let task_entry = uuid(0xa133);
    let starting_frontier = ContextFrontierId::from_uuid(uuid(0xa134));
    let selection = uuid(0xa101);
    insert_session_for(pool, parent_session).await?;
    let accepted_starting_frontier: Uuid = sqlx::query_scalar(
        "SELECT starting_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE queued_input_origin DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(accepted_starting_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(accepted_starting_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM semantic_transcript_entry
          WHERE source_session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request)
    .bind(parent_session)
    .bind(parent_turn)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind,
             on_parent_stopped, on_parent_cancelled)
         VALUES ($1, $2, $3, $4, 'background', NULL, NULL)",
    )
    .bind(spawning_request)
    .bind(parent_session)
    .bind(parent_turn)
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(session.into_uuid())
    .bind(task_entry)
    .bind(spawning_request)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 1)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(task_entry)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL,
                starting_frontier_id = $1,
                active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(starting_frontier.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM queued_input_origin
          WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1,
                 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(task_entry)
    .bind(selection)
    .bind("delegated runner recovery fixture")
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE queued_input_origin ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(starting_frontier)
}

async fn make_accepted_turn_direct_root(
    pool: &PgPool,
    command: DurableCommandId,
    accepted_input: AcceptedInputId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE submit_input_command DISABLE TRIGGER ALL;
         ALTER TABLE accepted_input DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE submit_input_command
            SET delivery_kind = 'start_when_no_active_turn',
                expected_active_turn_id = NULL
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET delivery_kind = 'start_when_no_active_turn',
                expected_active_turn_id = NULL
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE accepted_input ENABLE TRIGGER ALL;
         ALTER TABLE submit_input_command ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn append_runner_registration_loss_projection(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut transaction,
        session,
        Some("registration"),
        None,
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn append_same_runner_replacement_projection(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    requested_directory: Option<&RunnerWorkingDirectory>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'runner_replaced', selector_kind, selector_runner_id,
                selector_capability_class,
                CASE WHEN $2::text IS NULL
                     THEN directory_selection_kind ELSE 'exact' END,
                COALESCE($2::text, requested_working_directory),
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'pinned', NULL, NULL, pinned_runner_id,
                COALESCE($2::text, pinned_working_directory),
                pinned_credential_profile_name,
                registration_enrollment_id, registration_revision,
                pinned_tool_count, workspace_repository_key,
                workspace_working_directory, workspace_manifest_id,
                workspace_placement_revision,
                workspace_clone_url_digest, workspace_credential_profile_name,
                workspace_sandbox_profile, workspace_relative_path,
                workspace_recovery_kind, workspace_branch_name,
                workspace_revision, credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision + 1
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .bind(requested_directory.map(RunnerWorkingDirectory::as_str))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
         SELECT session_id, event_ordinal + 1, tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn append_runner_lost_before_pin_projection(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision,
                'runner_lost_before_pin', selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'runner_lost_before_pin', selector_runner_id, NULL, NULL, NULL,
                NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn append_pre_pin_replacement_without_advancing_head(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    successor: RunnerId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'pre_pin_replaced', 'identity', $2, NULL,
                directory_selection_kind, requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'unpinned', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .bind(successor.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn append_pre_pin_replacement_projection(
    pool: &PgPool,
    session: SessionId,
    successor: RunnerId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    append_pre_pin_replacement_without_advancing_head(&mut transaction, session, successor).await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

async fn append_abandoned_projection(
    pool: &PgPool,
    session: SessionId,
    requested_directory_override: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision,
                'abandoned', selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                COALESCE($2::text, requested_working_directory),
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'runner_abandoned', lost_runner_id, loss_source_kind,
                pinned_runner_id, pinned_working_directory,
                pinned_credential_profile_name, registration_enrollment_id,
                registration_revision, pinned_tool_count,
                workspace_repository_key, workspace_working_directory,
                workspace_manifest_id, workspace_placement_revision,
                workspace_clone_url_digest, workspace_credential_profile_name,
                workspace_sandbox_profile, workspace_relative_path,
                workspace_recovery_kind, workspace_branch_name,
                workspace_revision, credential_grant_runner_id,
                credential_grant_lineage_origin_ordinal,
                credential_grant_revision
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .bind(requested_directory_override)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_tool
         SELECT session_id, event_ordinal + 1, tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
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

/// a new loss owns a pending cursor whose ordered read page is capped
/// at 64 sessions and resumes strictly after its durable session identity.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_cursor_pages_sixty_four_sessions() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");
    let first_page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(first_page.loss(), loss);
    assert_eq!(first_page.propagated_through(), None);
    assert_eq!(first_page.sessions(), &expected_sessions[..64]);
    assert!(!first_page.is_complete());

    project_bounded_propagation_sessions(&pool, &expected_sessions[..64]).await?;
    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await?;
    let second_page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(second_page.loss(), loss);
    assert_eq!(
        second_page.propagated_through(),
        Some(expected_sessions[63])
    );
    assert_eq!(second_page.sessions(), &expected_sessions[64..]);
    assert!(!second_page.is_complete());

    drop(pool);
    Ok(())
}

/// bounded propagation pages have indexes for both enrollment-fenced
/// and pre-enrollment exact-runner placement branches.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_page_has_affected_set_indexes() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'runner_session_placement_loss_propagation_page'",
    )
    .fetch_one(&pool)
    .await?;
    let exact_definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'runner_session_placement_exact_loss_propagation_page'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(
        definition.contains("(loss_fence_enrollment_id, session_id, event_ordinal)"),
        "the loss page index must lead with enrollment and preserve session order"
    );
    assert!(
        exact_definition.contains("(selector_runner_id, session_id, event_ordinal)"),
        "the exact-selection page index must lead with runner and preserve session order"
    );
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot advance past an affected session that
/// has not received the loss projection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_cursor_rejects_skipped_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");

    let skipped = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot skip an affected session");

    assert_check_violation(skipped);
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot rewind behind its durable session.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_cursor_rejects_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");
    project_bounded_propagation_sessions(&pool, &expected_sessions[..64]).await?;
    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await?;

    let rewound = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[62].into_uuid())
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot rewind its session identity");

    assert_check_violation(rewound);
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot complete while an affected session
/// still retains an older loss baseline.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_cursor_rejects_premature_completion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");

    let premature_completion = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET state_kind = 'completed'
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot complete before its final session");

    assert_check_violation(premature_completion);
    drop(pool);
    Ok(())
}

/// an exact-identity placement that observes enrollment absence
/// commits before the matching enrollment can create or complete a loss cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_pre_enrollment_placement_serializes_loss_cursor_creation() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let expected_session = SessionId::from_uuid(uuid(SESSION));
    let mut placement = pool.begin().await?;
    insert_uncommitted_exact_placement(
        &mut placement,
        expected_session,
        expected_enrollment.runner(),
    )
    .await?;
    let mut enrollment_insert = Box::pin(store.insert_enrollment(&expected_enrollment));

    tokio::time::timeout(LOCK_WAIT_PROBE, &mut enrollment_insert)
        .await
        .expect_err("enrollment must wait for the absent-baseline placement");
    placement.commit().await?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, enrollment_insert).await??;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the serialized terminal connection owns its loss cursor");
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(page.sessions(), &[expected_session]);
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// exact-identity placement takes the runner-identity fence before
/// enrollment authority, so cursor completion cannot form an opposing edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_loss_cursor_completion_serializes_on_runner_identity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let mut enrollment_authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&mut *enrollment_authority)
    .await?;
    let placement_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let placement_insert = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            placement_store.store_placement(&placement, None, None),
        )
        .await
    });
    let placement_blocked =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
            .await
            .expect("placement enrollment-lock observation must remain bounded")?;
    let mut completion = Box::pin(store.complete_connection_loss_propagation(loss));

    tokio::time::timeout(LOCK_WAIT_PROBE, &mut completion)
        .await
        .expect_err("cursor completion must wait for placement's identity fence");
    enrollment_authority.commit().await?;
    placement_insert
        .await
        .expect("the placement task remains joinable")
        .expect("the placement insert must finish within its operation timeout")?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, completion).await??;
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert!(
        placement_blocked,
        "placement must reach enrollment authority after taking identity"
    );
    assert_eq!(page.sessions(), &[]);
    assert!(page.is_complete());
    drop(pool);
    Ok(())
}

/// a fully projected loss cursor may transition once to completed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_cursor_completes_after_final_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its pending propagation cursor");
    project_bounded_propagation_sessions(&pool, &expected_sessions).await?;

    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET state_kind = 'completed'
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await?;
    let completed = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(completed.loss(), loss);
    assert_eq!(completed.propagated_through(), None);
    assert!(completed.sessions().is_empty());
    assert!(completed.is_complete());
    drop(pool);
    Ok(())
}

/// the bounded loss transaction projects an exact unpinned
/// identity loss, its follower event, and its cursor advancement atomically.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_transaction_projects_exact_unpinned_session() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    let expected_revision = placement.revision();
    store.store_placement(&placement, None, None).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the exact runner loss owns its propagation cursor");
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let replay = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    store.complete_connection_loss_propagation(loss).await?;
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the transaction installs the exact loss-before-pin record");
    let completed = store.load_connection_loss_propagation_page(loss).await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLostBeforePin,
            interrupted_tool_attempt: None,
        }
    );
    assert_eq!(replay, RunnerConnectionLossSessionDisposition::Replayed);
    assert_eq!(
        loaded.placement().state(),
        &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
            expected_enrollment.runner(),
        ))
    );
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    assert_eq!(completed.propagated_through(), Some(session));
    assert!(completed.is_complete());
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: expected_enrollment.runner(),
            placement_revision: expected_revision,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: Some(exact_runner_directory()),
            state: DispatchedRunnerState::RunnerLostBeforePin,
        }
    );
    drop(pool);
    Ok(())
}

/// an exact-runner placement stored before enrollment uses
/// the same runner-identity fallback during projection that selected its page.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_transaction_projects_pre_enrollment_session() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the exact runner loss owns its propagation cursor");

    assert_eq!(
        store
            .propagate_connection_loss_session(loss, session)
            .await?,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLostBeforePin,
            interrupted_tool_attempt: None,
        }
    );
    assert_eq!(
        store
            .load_placement(session)
            .await?
            .expect("the loss projection remains readable")
            .placement()
            .state(),
        &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
            expected_enrollment.runner(),
        ))
    );
    drop(pool);
    Ok(())
}

/// a placement change serialized after paging makes the old loss
/// subject superseded and advances the cursor without a second projection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_transaction_advances_a_superseded_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the exact runner loss owns its propagation cursor");
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Superseded
    );
    assert_eq!(page.propagated_through(), Some(session));
    assert!(page.sessions().is_empty());
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// cursor completion rechecks the affected placement set and cannot
/// hide a session that has not crossed the atomic propagation boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_transaction_rejects_premature_completion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the exact runner loss owns its propagation cursor");
    let rejected = store
        .complete_connection_loss_propagation(loss)
        .await
        .expect_err("completion cannot skip the affected placement");
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_store_check_violation(rejected);
    assert_eq!(page.propagated_through(), None);
    assert_eq!(page.sessions(), &[session]);
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// an offered lease becomes exact
/// no-execution loss while its physical attempt and yielded turn wait remain
/// correlated to the same placement boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_transaction_retires_offered_lease() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let attempt = pin.lease.attempt();
    let lease_id = pin.lease.correlation().lease;
    let lease_generation = pin.lease.generation();
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the offered lease loss owns its exact cursor");
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(lease_id, lease_generation)
        .await?
        .expect("the offered lease is durably classified as lost");
    let wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the issuing turn yields to runner recovery");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLost,
            interrupted_tool_attempt: Some(attempt),
        }
    );
    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostUnclaimed
    );
    assert_eq!(
        loaded_loss
            .no_execution_proof()
            .map(|proof| proof.correlation()),
        Some(&loaded_loss.lost().correlation())
    );
    assert_eq!(wait.interrupted_tool_attempt(), Some(attempt));
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// profile replacement does not hide the live
/// lease offered against its pinned predecessor from later loss propagation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_finds_lease_before_profile_replacement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
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
    store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            Some(&replacement_grant),
        )
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the offered lease loss owns its exact cursor");
    let attempt = pin.lease.attempt();

    assert_eq!(
        store
            .propagate_connection_loss_session(loss, pin.placement.session())
            .await?,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLost,
            interrupted_tool_attempt: Some(attempt),
        }
    );
    assert_eq!(
        store
            .load_lease_loss(pin.lease.correlation().lease, pin.lease.generation())
            .await?
            .expect("the predecessor lease is classified by the loss")
            .lost()
            .state(),
        signalbox_domain::RunnerLeaseState::LostUnclaimed
    );
    assert_eq!(
        store
            .load_runner_recovery_wait(pin.placement.session())
            .await?
            .expect("the active turn moves to runner recovery")
            .interrupted_tool_attempt(),
        Some(attempt)
    );
    drop(pool);
    Ok(())
}

/// refusing the follower event rolls
/// placement, lease, turn wait, and propagation-cursor mutation back together.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_transaction_rolls_back_as_one_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let expected_placement_state = pin.placement.state().clone();
    let lease_id = pin.lease.correlation().lease;
    let lease_generation = pin.lease.generation();
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the rollback fixture owns its exact loss cursor");
    sqlx::raw_sql(
        "CREATE FUNCTION reject_runner_loss_outbox_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'synthetic runner outbox refusal'
                 USING ERRCODE = '23514';
         END;
         $$;
         CREATE TRIGGER reject_runner_loss_outbox_for_test
         BEFORE INSERT ON runner_state_transition_outbox_event
         FOR EACH ROW EXECUTE FUNCTION reject_runner_loss_outbox_for_test();",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .propagate_connection_loss_session(loss, session)
        .await
        .expect_err("the injected follower-event refusal aborts propagation");
    let loaded_placement = store
        .load_placement(session)
        .await?
        .expect("the original pinned placement remains current");
    let loaded_lease = store
        .load_lease(lease_id, lease_generation)
        .await?
        .expect("the original offered lease remains current");
    let page = store.load_connection_loss_propagation_page(loss).await?;
    let wait = store.load_runner_recovery_wait(session).await?;

    assert_store_check_violation(rejected);
    assert_eq!(
        loaded_placement.placement().state(),
        &expected_placement_state
    );
    assert_eq!(
        loaded_lease.state(),
        signalbox_domain::RunnerLeaseState::Offered
    );
    assert_eq!(page.propagated_through(), None);
    assert_eq!(page.sessions(), &[session]);
    assert_eq!(wait, None);
    drop(pool);
    Ok(())
}

/// claimed pure work remains retryable and
/// in-flight while the turn yields to the exact runner-recovery wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_transaction_retains_claimed_pure_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the offered pure lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the claimed pure lease loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the claimed pure lease is durably lost");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert!(loaded_loss.retry().is_some());
    assert_eq!(loaded_loss.crash_attempt(), None);
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// claimed idempotent work retains
/// retry authority without erasing the fact that execution may have occurred.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_transaction_retains_idempotent_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(
            &pool,
            ActivePinEffectCase::IdempotentExternalEffect,
        )
        .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the idempotent lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the idempotent lease loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the idempotent lease is durably lost");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert!(loaded_loss.retry().is_some());
    assert_eq!(loaded_loss.crash_attempt(), None);
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// claimed side-effecting work keeps
/// execution ambiguity instead of being rewritten as known failure.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_runner_loss_transaction_preserves_side_effect_ambiguity() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(
            &pool,
            ActivePinEffectCase::SideEffectingExternalEffect,
        )
        .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the side-effecting lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the side-effecting loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the side-effecting lease is durably lost");
    let (attempt_state, disposition): (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt WHERE attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the ambiguous attempt remains named by runner recovery");

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert_eq!(loaded_loss.retry(), None);
    assert_eq!(loaded_loss.crash_attempt(), Some(attempt));
    assert_eq!(attempt_state, "terminal");
    assert_eq!(disposition.as_deref(), Some("ambiguous"));
    assert_eq!(wait.interrupted_tool_attempt(), Some(attempt));
    drop(pool);
    Ok(())
}

/// a runner-loss propagation cursor is durable evidence and cannot be
/// deleted independently of its exact loss epoch.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_propagation_cursor_rejects_delete() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its durable cursor");
    let deleted = sqlx::query(
        "DELETE FROM runner_connection_loss_propagation
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await
    .expect_err("a durable runner-loss cursor cannot be deleted");

    assert_check_violation(deleted);
    drop(pool);
    Ok(())
}

/// bulk truncation cannot bypass runner-loss cursor durability.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_propagation_cursor_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let truncated = sqlx::query("TRUNCATE runner_connection_loss_propagation")
        .execute(&pool)
        .await
        .expect_err("durable runner-loss cursors cannot be truncated");

    assert_check_violation(truncated);
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
async fn s30_registration_round_trips_canonical_evidence() -> Result<(), Box<dyn Error>> {
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

/// revoking an enrollment with a live physical connection advances
/// the exact loss epoch in the same transaction as terminalization.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_revocation_advances_live_connection_loss_epoch() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let mut expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let live_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    assert!(store.revoke_enrollment(&mut expected_enrollment).await?);
    let revoked_connection = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("revocation retains its terminal connection source");
    let revocation_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("revocation advances the connection loss epoch");

    assert_eq!(revoked_connection.epoch(), live_connection.epoch());
    assert_eq!(revoked_connection.state(), RunnerConnectionState::Lost);
    assert_eq!(
        revoked_connection.cause(),
        RunnerConnectionCause::EnrollmentRevoked
    );
    assert_eq!(
        revocation_loss.connection_epoch(),
        revoked_connection.epoch()
    );
    assert_eq!(
        revocation_loss.connection_event_ordinal(),
        revoked_connection.event_ordinal()
    );
    drop(pool);
    Ok(())
}

/// first-pin authority decodes the closed enrollment discriminator
/// before applying active-enrollment policy.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_store_pin_rejects_corrupt_enrollment_discriminator() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _) = stored_pin_fixture(&pool).await?;
    let session = SessionId::from_uuid(uuid(SECOND_SESSION));
    insert_session_for(&pool, session.into_uuid()).await?;
    insert_physical_attempt_for(&pool, session, SECOND_SESSION_PHYSICAL_ATTEMPT).await?;
    let placement = SessionRunnerPlacement::new(
        session,
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
            RunnerWorkingDirectory::try_new("/workspace/second-session".to_owned())
                .expect("the second fixture working directory is valid"),
            None,
            authorized_for_session(session, SECOND_SESSION_PHYSICAL_ATTEMPT),
            offer_request_for(LEASE + RELATED_IDENTITY_OFFSET),
        )
        .expect("the second fixture registration prepares a pin");
    sqlx::query(
        "ALTER TABLE runner_enrollment
         DROP CONSTRAINT runner_enrollment_state_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_enrollment DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_enrollment
            SET state_kind = 'corrupt'
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("an unknown enrollment discriminator is durable corruption");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn always_confirm_registration_persists_under_the_closed_constraint()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool, always_confirm_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;

    let stored = store
        .register(&expected_enrollment, advertisement())
        .await?;

    assert_eq!(
        stored
            .registration()
            .tool(&tool("inspect"))
            .expect("the registered explicit-approval tool is present")
            .permission(),
        ToolPermissionDefault::AlwaysConfirm
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_store_rejects_oversized_repository_inventory_before_write()
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
async fn s30_failed_registration_write_preserves_prior_authority() -> Result<(), Box<dyn Error>> {
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
async fn s30_insert_enrollment_requires_pristine_registration_authority()
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
async fn s30_outstanding_preparation_fails_registration_before_durable_writes()
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
async fn s30_historical_registration_load_remains_stale() -> Result<(), Box<dyn Error>> {
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
async fn s30_stale_loaded_enrollment_cannot_bind_historical_registration()
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
async fn s30_orphan_revocation_audit_cannot_commit() -> Result<(), Box<dyn Error>> {
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
async fn s30_historical_enrollment_audit_rechecks_its_own_revision() -> Result<(), Box<dyn Error>> {
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
async fn s30_current_registration_gates_new_leases() -> Result<(), Box<dyn Error>> {
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
async fn s31_current_registration_preserves_complete_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s31_current_registration_preserves_profile() -> Result<(), Box<dyn Error>> {
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
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s31_current_registration_preserves_workspace() -> Result<(), Box<dyn Error>> {
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
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
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
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/1/repo",
                    uuid(SESSION)
                ))
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
async fn s30_registration_replacement_serializes_later_lease_admission()
-> Result<(), Box<dyn Error>> {
    struct SerializationOutcome {
        replacement_result: Result<
            Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError>,
            tokio::time::error::Elapsed,
        >,
        replacement_observation: Result<Result<bool, sqlx::Error>, tokio::time::error::Elapsed>,
        lease_observation: Result<Result<bool, sqlx::Error>, tokio::time::error::Elapsed>,
        blocker_commit: Result<Result<(), sqlx::Error>, tokio::time::error::Elapsed>,
        lease_result: Result<Result<(), RunnerProtocolStoreError>, tokio::time::error::Elapsed>,
    }

    struct LeaseAdmissionOutcome {
        replacement_observation: Result<Result<bool, sqlx::Error>, tokio::time::error::Elapsed>,
        lease_observation: Result<Result<bool, sqlx::Error>, tokio::time::error::Elapsed>,
        blocker_commit: Result<Result<(), sqlx::Error>, tokio::time::error::Elapsed>,
        lease_result: Result<Result<(), RunnerProtocolStoreError>, tokio::time::error::Elapsed>,
    }

    let (_container, pool) = migrated_postgres().await?;
    let serialization = tokio::time::timeout(SERIALIZATION_TEST_TIMEOUT, async {
        let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query(
            "SELECT enrollment_id
               FROM runner_current_registration
              WHERE enrollment_id = $1
              FOR UPDATE",
        )
        .bind(expected_enrollment.enrollment().into_uuid())
        .fetch_one(&mut *blocker)
        .await?;
        let replacement_store = RunnerProtocolStore::new(pool.clone(), catalog());
        let replacement = async move {
            tokio::time::timeout(
                LOCK_COMPLETION_TIMEOUT,
                replacement_store.register(&expected_enrollment, narrowed_advertisement()),
            )
            .await
        };
        let lease_admission = async {
            let replacement_observation =
                tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
                    .await;
            let lease_store = async move {
                tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, store.store_lease(&lease)).await
            };
            let release_blocker = async {
                let lease_observation = tokio::time::timeout(
                    LOCK_COMPLETION_TIMEOUT,
                    blocked_backends_reached(&pool, 2),
                )
                .await;
                let blocker_commit =
                    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocker.commit()).await;
                (lease_observation, blocker_commit)
            };
            let (lease_result, (lease_observation, blocker_commit)) =
                tokio::join!(lease_store, release_blocker);
            LeaseAdmissionOutcome {
                replacement_observation,
                lease_observation,
                blocker_commit,
                lease_result,
            }
        };
        let (replacement_result, lease_admission) = tokio::join!(replacement, lease_admission);
        Ok::<_, Box<dyn Error>>(SerializationOutcome {
            replacement_result,
            replacement_observation: lease_admission.replacement_observation,
            lease_observation: lease_admission.lease_observation,
            blocker_commit: lease_admission.blocker_commit,
            lease_result: lease_admission.lease_result,
        })
    })
    .await;
    let pool_close = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, pool.close()).await;
    let outcome = serialization
        .expect("registration replacement serialization must finish within its test deadline")?;
    pool_close.expect("registration replacement pool cleanup must remain bounded");
    let replacement_blocked = outcome
        .replacement_observation
        .expect("registration replacement lock observation must remain bounded")?;
    let lease_blocked = outcome
        .lease_observation
        .expect("lease admission lock observation must remain bounded")?;
    outcome
        .blocker_commit
        .expect("registration-head blocker commit must remain bounded")?;
    outcome
        .replacement_result
        .expect("registration replacement must finish within its operation timeout")?;
    let rejected = outcome
        .lease_result
        .expect("lease admission must finish within its operation timeout")
        .expect_err("withdrawn current availability cannot authorize the later lease");

    assert!(
        replacement_blocked,
        "registration replacement must reach registration-head authority"
    );
    assert!(
        lease_blocked,
        "lease admission must wait behind registration replacement"
    );
    assert_store_check_violation(rejected);
    Ok(())
}

/// the atomic initial pin takes the session scheduler
/// before the placement head, matching every later lease append.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s30_initial_pin_locks_scheduler_before_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let serialization = tokio::time::timeout(SERIALIZATION_TEST_TIMEOUT, async {
        insert_session(&pool).await?;
        insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
        let store = RunnerProtocolStore::new(pool.clone(), catalog());
        let expected_enrollment = enrollment();
        store.insert_enrollment(&expected_enrollment).await?;
        let registration = store
            .register(&expected_enrollment, advertisement())
            .await?;
        store
            .open_connection(expected_enrollment.enrollment())
            .await?;
        let session = SessionId::from_uuid(uuid(SESSION));
        let placement = SessionRunnerPlacement::new(
            session,
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
                RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                    .expect("the fixture working directory is valid"),
                None,
                authorized(INITIAL_PHYSICAL_ATTEMPT),
                offer_request(),
            )
            .expect("the validated registration pins the placement");
        let mut scheduler_lock_holder = pool.begin().await?;
        sqlx::query(
            "SELECT session_id
               FROM session_scheduler
              WHERE session_id = $1
              FOR UPDATE",
        )
        .bind(session.into_uuid())
        .fetch_one(&mut *scheduler_lock_holder)
        .await?;
        let pin_store = tokio::spawn(async move {
            tokio::time::timeout(
                LOCK_COMPLETION_TIMEOUT,
                store.store_pin(&pin, &registration),
            )
            .await
        });
        let pin_blocked =
            tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
                .await
                .expect("initial pin lock observation must remain bounded")?;
        let locked_placement: Uuid = tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            sqlx::query_scalar(
                "SELECT session_id
                   FROM runner_current_session_placement
                  WHERE session_id = $1
                  FOR UPDATE",
            )
            .bind(session.into_uuid())
            .fetch_one(&mut *scheduler_lock_holder),
        )
        .await
        .expect("the scheduler lock holder must acquire placement before the queued pin")?;
        scheduler_lock_holder.commit().await?;
        pin_store
            .await
            .expect("the initial pin task must remain joinable")
            .expect("the initial pin must finish within its task-owned timeout")?;

        assert!(pin_blocked, "the initial pin must wait for the scheduler");
        assert_eq!(locked_placement, session.into_uuid());
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    drop(pool);
    serialization.expect("initial pin lock ordering must finish within its test deadline")
}

/// every generic placement projection takes the session
/// scheduler before the placement head, matching a concurrent lease writer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_placement_projection_locks_scheduler_before_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let serialization = tokio::time::timeout(SERIALIZATION_TEST_TIMEOUT, async {
        let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
        let session = pin.placement.session();
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
        let replacement_grant =
            duplicate_grant(&replacement.grant.grant, registration.registration());
        let mut scheduler_lock_holder = pool.begin().await?;
        sqlx::query(
            "SELECT session_id
               FROM session_scheduler
              WHERE session_id = $1
              FOR UPDATE",
        )
        .bind(session.into_uuid())
        .fetch_one(&mut *scheduler_lock_holder)
        .await?;
        let replacement_store = tokio::spawn(async move {
            tokio::time::timeout(
                LOCK_COMPLETION_TIMEOUT,
                store.store_placement(
                    &replacement.placement,
                    Some(&registration),
                    Some(&replacement_grant),
                ),
            )
            .await
        });
        let replacement_blocked =
            tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
                .await
                .expect("placement projection lock observation must remain bounded")?;
        let locked_placement: Uuid = tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            sqlx::query_scalar(
                "SELECT session_id
                   FROM runner_current_session_placement
                  WHERE session_id = $1
                  FOR UPDATE",
            )
            .bind(session.into_uuid())
            .fetch_one(&mut *scheduler_lock_holder),
        )
        .await
        .expect("the scheduler lock holder must acquire placement before the queued projection")?;
        scheduler_lock_holder.commit().await?;
        replacement_store
            .await
            .expect("the placement projection task must remain joinable")
            .expect("the placement projection must finish within its task-owned timeout")?;
        let event_kind: String = sqlx::query_scalar(
            "SELECT event_kind
               FROM runner_session_placement_record
              WHERE session_id = $1
              ORDER BY event_ordinal DESC
              LIMIT 1",
        )
        .bind(session.into_uuid())
        .fetch_one(&pool)
        .await?;

        assert!(
            replacement_blocked,
            "the placement projection must wait for the scheduler"
        );
        assert_eq!(locked_placement, session.into_uuid());
        assert_eq!(event_kind, "profile_replaced");
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    drop(pool);
    serialization.expect("placement projection lock ordering must finish within its test deadline")
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_current_registration_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
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
async fn s30_current_registration_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s30_enrollment_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s30_enrollment_audit_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s30_registration_inventories_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s30_appended_registration_must_advance_current_head() -> Result<(), Box<dyn Error>> {
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
async fn s31_concurrent_attempt_binding_has_one_lease_lineage() -> Result<(), Box<dyn Error>> {
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
async fn s31_request_cannot_start_second_lease_lineage() -> Result<(), Box<dyn Error>> {
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

/// a later lease offered on a live connection retains the
/// exact offer authority required by its subsequent claim.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_connected_later_lease_offer_admits_exact_claim() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact live-connection lease correlation claims");
    store.store_lease(&claimed).await?;
    let loaded = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the offer connection remains current");

    assert_eq!(loaded, connection);
    assert_eq!(
        store
            .load_lease(lease.correlation().lease, lease.correlation().generation)
            .await?,
        Some(claimed)
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_orphan_request_lease_binding_cannot_commit() -> Result<(), Box<dyn Error>> {
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
async fn s31_request_lease_binding_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s31_orphan_physical_attempt_binding_cannot_commit() -> Result<(), Box<dyn Error>> {
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
async fn s31_concurrent_enrollment_revocation_blocks_a_later_lease() -> Result<(), Box<dyn Error>> {
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
async fn s31_direct_lease_admission_serializes_enrollment_revocation() -> Result<(), Box<dyn Error>>
{
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
async fn s31_concurrent_grant_revocation_blocks_a_later_lease() -> Result<(), Box<dyn Error>> {
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
async fn s32_grant_revocation_serializes_profile_replacement() -> Result<(), Box<dyn Error>> {
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

/// S32: profile replacement stays durable after an
/// availability-equivalent re-registration. The domain validates the
/// replacement against the enrollment-owned current revision while the
/// placement record carries the pinned registration snapshot forward.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_replacement_survives_equivalent_reregistration() -> Result<(), Box<dyn Error>>
{
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

/// later lease admission takes enrollment authority before
/// the placement head, matching profile replacement's durable lock order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_lease_offer_locks_enrollment_before_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin, lease) =
        stored_later_lease_fixture(&pool).await?;
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
    let enrollment = expected_enrollment.enrollment();
    let mut enrollment_blocker = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *enrollment_blocker)
    .await?;
    let lease_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let lease_task = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, lease_store.store_lease(&lease)).await
    });
    let lease_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let mut placement_probe = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM runner_current_session_placement
          WHERE session_id = $1
          FOR UPDATE NOWAIT",
    )
    .bind(pin.placement.session().into_uuid())
    .fetch_one(&mut *placement_probe)
    .await?;
    placement_probe.rollback().await?;
    let replacement_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let replacement_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            replacement_store.store_placement(
                &replacement.placement,
                Some(&registration),
                Some(&replacement_grant),
            ),
        )
        .await
    });
    let replacement_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let blocker_commit =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, enrollment_blocker.commit()).await;
    let lease_result = lease_task.await;
    let replacement_result = replacement_task.await;
    let lease_blocked = lease_observation.expect("lease lock observation must remain bounded")?;
    let replacement_blocked =
        replacement_observation.expect("replacement lock observation must remain bounded")?;
    blocker_commit.expect("enrollment blocker commit must remain bounded")?;
    lease_result
        .expect("lease task must remain joinable")
        .expect("lease admission must finish within its task-owned timeout")?;
    replacement_result
        .expect("replacement task must remain joinable")
        .expect("profile replacement must finish within its task-owned timeout")?;

    assert!(
        lease_blocked,
        "lease admission must reach enrollment authority"
    );
    assert!(
        replacement_blocked,
        "profile replacement must queue behind the lease's scheduler authority"
    );
    drop(store);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_combined_tool_override_survives_omitted_runner_availability()
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
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: daemon_fallback_permission_overrides(),
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
        .expect("daemon policy admits the override while inspect dispatches");
    store.store_pin(&pin, &registration).await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the omitted combined-tool override is durable");

    assert_eq!(loaded.placement(), &pin.placement);
    sqlx::query(
        "ALTER TABLE runner_session_placement_permission_override
         DISABLE TRIGGER runner_session_placement_permission_override_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_permission_override
            SET tool_name = $2
          WHERE session_id = $1
            AND tool_name = $3",
    )
    .bind(uuid(SESSION))
    .bind(tool("future").as_str())
    .bind(tool("daemon_fallback").as_str())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_permission_override
         ENABLE TRIGGER runner_session_placement_permission_override_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupt = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await
        .expect_err("an override outside daemon catalog authority fails on load");

    assert_store_domain_error(corrupt, RunnerDomainError::ToolUndeclared(tool("future")));
    drop(pool);
    Ok(())
}

/// S31: a session-policy tool/profile pair admits a lease
/// only with confirmed approval provenance; policy-auto provenance is
/// rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_session_policy_lease_requires_confirmed_provenance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
    replace_approval_with_user_command(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
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

/// S31: a one-shot user override is the user confirming
/// one exact command in advance, so its provenance admits a session-policy
/// lease exactly as an applied user command does.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_session_policy_lease_admits_user_override_provenance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
    insert_user_override_approval(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    store.store_pin(&pin, &registration).await?;
    let admitted: Decimal = sqlx::query_scalar(
        "SELECT generation
           FROM runner_lease_generation
          WHERE lease_id = $1",
    )
    .bind(uuid(LEASE))
    .fetch_one(&pool)
    .await?;

    assert_eq!(admitted, Decimal::from(1u64));
    drop(pool);
    Ok(())
}

/// S31: a profileless lease on a Confirm-permission tool
/// admits only confirmed approval provenance; policy-auto provenance is
/// rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_profileless_confirm_lease_requires_confirmed_provenance() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), confirm_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
    replace_approval_with_user_command(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
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
async fn s32_replaced_grant_is_not_a_current_revocation_target() -> Result<(), Box<dyn Error>> {
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
async fn s30_profile_replacement_requires_current_registration() -> Result<(), Box<dyn Error>> {
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
async fn s32_generic_store_rejects_runner_replacement_without_command_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let successor_registration = store.register(&successor, advertisement()).await?;
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            pin.grant,
        )
        .expect("the current registration can prepare a replacement");
    let rejected = store
        .store_placement(
            &replacement.placement,
            Some(&successor_registration),
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("the generic writer cannot invent replacement-command authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_checked_runner_replacement_requires_a_live_successor_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let successor_registration = store.register(&successor, advertisement()).await?;
    let connection = store.open_connection(successor.enrollment()).await?;
    store
        .transition_connection(
            successor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            pin.grant,
        )
        .expect("the caller-held registration can prepare a replacement");
    let rejected = store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a disconnected successor cannot install replacement authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_checked_runner_replacement_rejects_a_successor_without_a_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let successor_registration = store.register(&successor, advertisement()).await?;
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            pin.grant,
        )
        .expect("the caller-held registration can prepare a replacement");
    let rejected = store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a successor without a connection cannot install replacement authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

/// the relational replacement shape preserves the checked future
/// same-runner recovery reserved exclusively for registration-triggered loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_registration_loss_admits_same_runner_replacement_shape() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_credentialless_pin_fixture(&pool).await?;
    append_runner_registration_loss_projection(&pool, pin.placement.session()).await?;
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(&mut replacement, pin.placement.session(), None)
        .await?;
    replacement.commit().await?;
    let loaded_replacement = store
        .load_placement(pin.placement.session())
        .await?
        .expect("the committed same-runner replacement remains loadable");

    assert_eq!(
        loaded_replacement.placement().state(),
        pin.placement.state()
    );
    assert_eq!(
        loaded_replacement.placement().request(),
        pin.placement.request()
    );
    assert_eq!(loaded_replacement.registration(), Some(&registration));
    assert_eq!(loaded_replacement.grant(), None);
    drop(pool);
    Ok(())
}

/// connection loss retains the different-runner replacement rule.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_connection_loss_rejects_same_runner_replacement_shape() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    let mut malformed = pool.begin().await?;
    let rejected =
        append_same_runner_replacement_projection(&mut malformed, pin.placement.session(), None)
            .await
            .expect_err("only registration loss may retain the runner identity");

    assert_check_violation(rejected);
    drop(malformed);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s30_first_placement_record_is_created_unpinned() -> Result<(), Box<dyn Error>> {
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
async fn s30_placement_required_flag_matches_registered_locus() -> Result<(), Box<dyn Error>> {
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
async fn s30_initial_pin_requires_loadable_offered_lease() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
             workspace_manifest_id, workspace_placement_revision,
             workspace_clone_url_digest,
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
                workspace_manifest_id, workspace_placement_revision,
                workspace_clone_url_digest,
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
async fn s32_credential_relations_admit_names_and_audit_only() -> Result<(), Box<dyn Error>> {
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
async fn s32_grant_lineage_origin_is_part_of_every_durable_identity() -> Result<(), Box<dyn Error>>
{
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
async fn s32_pinned_affinity_and_grant_round_trip() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the credential-bearing pin has its grant");
    let revoked = store
        .revoke_grant(
            lost.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?
        .expect("the active grant revokes exactly once");
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            Some(revoked),
        )
        .expect("the domain records a successor grant revision");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
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

/// an exact selection that predates enrollment retains its absent
/// baseline when that late enrollment is lost before pin.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_lost_before_pin_round_trips_exact_identity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let runner = expected_enrollment.runner();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let baseline: (Option<Uuid>, Option<Decimal>) = sqlx::query_as(
        "SELECT loss_fence_enrollment_id, observed_runner_loss_epoch
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(lost.session().into_uuid())
    .fetch_one(&pool)
    .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the lost-before-pin placement is present");

    assert_eq!(baseline, (None, None));
    assert_eq!(loaded.placement(), &lost);
    assert_eq!(loaded.registration(), None);
    assert_eq!(loaded.grant(), None);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_transcript_snapshot_authenticates_current_pre_pin_runner_loss()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(lost.session())
        .await?
        .expect("the fixture session has a transcript snapshot");
    let projection = snapshot
        .runner()
        .expect("the runner-placed session projects its current placement");

    assert_eq!(projection.selector(), &lost.request().selector);
    assert_eq!(projection.runner(), Some(runner));
    assert_eq!(projection.placement_revision(), lost.revision());
    assert_eq!(projection.sandbox(), lost.request().sandbox);
    assert_eq!(projection.credential_profile(), None);
    assert_eq!(projection.repository(), None);
    assert_eq!(projection.working_directory(), Some(&expected_directory));
    assert_eq!(
        projection.state(),
        ProcessRunnerProjectionState::RunnerLostBeforePin
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_session_summary_authenticates_current_pre_pin_runner_loss()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let session = SessionId::from_uuid(uuid(SESSION));
    let selection = DirectModelSelection::from_uuid(uuid(0xa141));
    let credentials = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-model-family",
        "fixture-credential-reference",
    )])
    .expect("the fixture credential pin is valid");
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(uuid(0xa142)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(session)
    .expect("the fixture session creation is preparable");
    CreateSessionRepository::new(pool.clone(), credentials)
        .handle(creation)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        session,
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let mut summaries = ProcessReadRepository::new(pool.clone())
        .open_session_summaries()
        .await?;
    let summary = summaries
        .next_summary()
        .await?
        .expect("the fixture session has a session summary");
    let projection = summary
        .runner()
        .expect("the runner-placed session projects its current placement");

    assert_eq!(projection.selector(), &lost.request().selector);
    assert_eq!(projection.runner(), Some(runner));
    assert_eq!(projection.placement_revision(), lost.revision());
    assert_eq!(projection.sandbox(), lost.request().sandbox);
    assert_eq!(projection.credential_profile(), None);
    assert_eq!(projection.repository(), None);
    assert_eq!(projection.working_directory(), Some(&expected_directory));
    assert_eq!(
        projection.state(),
        ProcessRunnerProjectionState::RunnerLostBeforePin
    );
    drop(summaries);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_transcript_snapshot_authenticates_current_runner_suspicion()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(pin.placement.session())
        .await?
        .expect("the pinned fixture session has a transcript snapshot");
    let projection = snapshot
        .runner()
        .expect("the runner-placed session projects its current placement");

    assert_eq!(projection.state(), ProcessRunnerProjectionState::Pinned);
    assert_eq!(projection.runner(), Some(pin.lease.runner()));
    assert_eq!(
        projection.connection_health(),
        Some(ProcessRunnerConnectionHealth::Suspect)
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_revision_one_loss_authenticates_the_creation_request() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET selector_runner_id = $2,
                lost_runner_id = $2
          WHERE session_id = $1
            AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .bind(uuid(LATER_RUNNER))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("revision-one loss cannot replace the creation request");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pinned_facts_on_loss_before_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET pinned_runner_id = lost_runner_id
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("loss before pin cannot discard contradictory pinned authority");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_loss_with_another_event_kind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'abandoned'
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("loss state cannot normalize another event vocabulary");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

/// a later current record cannot impersonate the unique
/// revision-one placement creation event after relational guards are bypassed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_later_created_record() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'created', state_kind = 'unpinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("only the ordinal-one row may be the creation event");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

/// a pre-pin replacement cannot impersonate revision-one
/// creation after relational guards are bypassed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_revision_one_pre_pin_replacement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_pre_pin_replacement_projection(
        &pool,
        placement.session(),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET placement_revision = 1
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("a replacement event cannot carry the initial revision");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pre_pin_replacement_round_trips_append_only_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let successor_request = exact_runner_request(successor_enrollment.runner());
    let replacement = lost
        .replace_lost_runner_before_pin(successor_request, successor_registration.registration())
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor_registration.registration().runner(),
    )
    .await?;
    let successor_lost = replacement
        .placement
        .mark_runner_lost_before_pin(successor_enrollment.runner())
        .expect("the exact successor may also be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, successor_lost.session()).await?;
    let returning_enrollment = enrollment();
    store.insert_enrollment(&returning_enrollment).await?;
    let returning_registration = store
        .register(&returning_enrollment, advertisement())
        .await?;
    store
        .open_connection(returning_enrollment.enrollment())
        .await?;
    let second_replacement = successor_lost
        .replace_lost_runner_before_pin(
            exact_runner_request(returning_enrollment.runner()),
            returning_registration.registration(),
        )
        .expect("the distinct live original runner installs the next successor request");
    append_pre_pin_replacement_projection(
        &pool,
        second_replacement.placement.session(),
        returning_enrollment.runner(),
    )
    .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the second successor unpinned placement is present");

    assert_eq!(loaded.placement(), &second_replacement.placement);
    assert_eq!(loaded.registration(), None);
    assert_eq!(loaded.grant(), None);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_malformed_pre_pin_replacement_predecessor() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_requires_tools,
         DISABLE TRIGGER runner_session_placement_requires_permission_overrides",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET loss_source_kind = 'connection'
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(replacement.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("replacement history cannot normalize a malformed predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_malformed_pre_pin_replacement_origin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_requires_tools,
         DISABLE TRIGGER runner_session_placement_requires_permission_overrides",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET pinned_runner_id = selector_runner_id
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(replacement.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("replacement history cannot normalize a malformed origin");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pre_pin_replacement_rejects_retained_lost_selector() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let malformed = sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id, pinned_working_directory,
             pinned_credential_profile_name, registration_enrollment_id,
             registration_revision, pinned_tool_count,
             workspace_repository_key, workspace_working_directory,
             workspace_manifest_id, workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name,
             workspace_revision, credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'pre_pin_replaced', selector_kind, selector_runner_id,
                selector_capability_class, directory_selection_kind,
                requested_working_directory, requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'unpinned', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(lost.session().into_uuid())
    .execute(&pool)
    .await
    .expect_err("pre-pin replacement must select a distinct exact successor");

    assert_check_violation(malformed);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_requires_a_closed_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    let rejected = append_runner_lost_without_advancing_head(
        &mut malformed,
        pin.placement.session(),
        None,
        None,
        None,
    )
    .await
    .expect_err("runner loss requires its exact closed source");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_requires_the_exact_pinned_runner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    let rejected = append_runner_lost_without_advancing_head(
        &mut malformed,
        pin.placement.session(),
        Some("connection"),
        Some(RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER))),
        None,
    )
    .await
    .expect_err("runner loss cannot name a runner other than the pin");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_abandonment_retains_the_complete_lost_request() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    let rejected =
        append_abandoned_projection(&pool, placement.session(), Some("/workspace/tampered"))
            .await
            .expect_err("abandonment cannot change a retained request axis");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_missing_pre_pin_replacement_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        registration.registration().runner(),
    )
    .await?;
    let lost_again = replacement
        .placement
        .mark_runner_lost_before_pin(successor.runner())
        .expect("the unpinned successor may lose its selected runner");
    append_runner_lost_before_pin_projection(&pool, lost_again.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(lost_again.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost_again.session())
        .await
        .expect_err("revision two requires its exact append-only replacement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// reconstitution history is restricted to the current
/// placement head's physical event prefix.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pre_pin_replacement_proof_after_current_head()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        registration.registration().runner(),
    )
    .await?;
    let lost_again = replacement
        .placement
        .mark_runner_lost_before_pin(successor.runner())
        .expect("the unpinned successor may lose its selected runner");
    append_runner_lost_before_pin_projection(&pool, lost_again.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_ordinal = event_ordinal + 3
          WHERE session_id = $1 AND event_ordinal IN (2, 3)",
    )
    .bind(lost_again.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost_again.session())
        .await
        .expect_err("replacement proof after the current head is not history");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_loss_metadata_on_a_pinned_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = pinned_runner_id
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a pinned placement cannot carry discarded loss metadata");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_loss_for_a_runner_other_than_the_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = $2
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(uuid(REPLACEMENT_RUNNER))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("runner loss must name the exact pinned runner");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_generic_store_rejects_pre_pin_loss_without_transactional_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    let rejected = store
        .store_placement(&lost, None, None)
        .await
        .expect_err("the generic writer cannot invent connection-loss authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_generic_store_rejects_pinned_loss_without_transactional_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    let rejected = store
        .store_placement(&lost, Some(&registration), pin.grant.as_ref())
        .await
        .expect_err("the generic writer cannot invent connection-loss authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

/// the ordinary all-trigger lifecycle transition admits an
/// active running turn at the exact pre-pin runner-loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_running_turn_enters_runner_recovery_with_all_triggers() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the all-trigger transition stores its runner recovery wait");

    assert_eq!(loaded.turn(), turn);
    assert_eq!(loaded.runner(), runner);
    assert_eq!(loaded.placement_revision(), placement.revision());
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// runner recovery is available only after the exact live
/// turn attempt has yielded to its durable loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_non_yielded_turn_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await
    .expect_err("a non-yielded attempt cannot become a runner recovery wait");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a retained continuing tool round must have been
/// produced by the unique yielded chain-tip turn attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_stale_tool_round_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa16a));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(0xa16b)),
        ContextFrontierId::from_uuid(uuid(0xa16c)),
    )
    .await?;
    let chain_tip = TurnAttemptId::from_uuid(uuid(0xa16d));
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let mut corrupted_source = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop',
                 'yielded_to_durable_wait')",
    )
    .bind(chain_tip.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn_attempt.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2 AND session_id = $3",
    )
    .bind(chain_tip.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    corrupted_source.commit().await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE turn_attempt ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot retain an older attempt's tool round");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait can retain only a
/// continuing tool round, never a round already closed by turn end.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_nullable_runner_wait_rejects_closed_tool_round_boundary() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa16e));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(0xa16f)),
        ContextFrontierId::from_uuid(uuid(0xa170)),
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_round
            SET boundary_kind = 'closed_by_turn_end'
          WHERE producing_model_call_id = $1",
    )
    .bind(producing_call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot retain a closed tool round");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a runner-recovery wait naming the interrupted physical
/// attempt also requires that attempt's tool round to remain continuing.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_interrupted_runner_wait_rejects_closed_tool_round_boundary()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss_boundary(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: producing_call,
        },
        "closed_by_turn_end",
    )
    .await
    .expect_err("runner recovery cannot retain an interrupted attempt from a closed tool round");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot erase the
/// continuing tool-round boundary produced by its yielded chain-tip attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_nullable_runner_wait_rejects_hidden_tool_round() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        ModelCallId::from_uuid(uuid(0xa17c)),
        ToolRequestId::from_uuid(uuid(0xa17d)),
        ContextFrontierId::from_uuid(uuid(0xa17e)),
    )
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot erase its yielded tool-round boundary");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a tool round inserted after a nullable runner wait
/// rechecks and rejects the now-hidden yielded boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_late_tool_round_rechecks_nullable_runner_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa17f));
    let request = ToolRequestId::from_uuid(uuid(0xa180));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa181));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM tool_round WHERE producing_model_call_id = $1")
        .bind(producing_call.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
    )
    .bind(producing_call.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(boundary.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a late continuing round must expose the hidden runner-wait boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a tool-round writer takes the scheduler rendezvous
/// before inserting a round that would invalidate a nullable runner wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_serializes_tool_round_inserts() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa182));
    let request = ToolRequestId::from_uuid(uuid(0xa183));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa184));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM tool_round WHERE producing_model_call_id = $1")
        .bind(producing_call.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut late_round = Box::pin(
        sqlx::query(
            "INSERT INTO tool_round
                (producing_model_call_id, session_id, turn_id, boundary_kind,
                 boundary_frontier_id, response_part_count, request_count)
             VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
        )
        .bind(producing_call.into_uuid())
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .bind(boundary.into_uuid())
        .execute(&pool),
    );
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut late_round)
        .await
        .expect_err("tool-round insertion must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut late_round)
        .await
        .expect("tool-round admission finishes after the scheduler is released")
        .expect_err("the hidden runner-wait boundary still rejects the tool round");

    assert_check_violation(rejected);
    drop(late_round);
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot hide a live
/// physical attempt in its retained tool round.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_nullable_runner_wait_rejects_unrecorded_physical_attempt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa16e)),
    )
    .await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("nullable runner recovery cannot omit a live physical attempt");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot retain a
/// prepared physical attempt that stop handling would classify as current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_nullable_runner_wait_rejects_prepared_physical_attempt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa16f)),
    )
    .await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'prepared'
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("nullable runner recovery cannot retain a prepared attempt");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a retired claimed-retry predecessor is historical
/// inventory and does not make a resolved current round ambiguous.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_nullable_runner_wait_ignores_retired_claimed_retry_attempt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa170)),
    )
    .await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
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
    terminalize_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let mut runner_loss = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut runner_loss,
        session,
        Some("connection"),
        None,
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *runner_loss)
    .await?;
    runner_loss.commit().await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = NULL
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(expected_enrollment.runner().into_uuid())
    .bind(Decimal::from(pin.placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *recovery)
        .await?;
    recovery.commit().await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the nullable wait ignores the retired predecessor");

    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// an accepted-input interrupt terminalizes a runner-loss
/// wait and leaves the placement's runner-effect evidence untouched.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_terminalizes_runner_recovery_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let interrupt = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0xa120)),
        session,
        UserContent::try_text(String::from("stop runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa121)),
            Some(TurnId::from_uuid(uuid(0xa122))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa123)),
                ContextFrontierId::from_uuid(uuid(0xa124)),
            ),
            |_| TurnId::from_uuid(uuid(0xa125)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa126))),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa127)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa128)),
                ContextFrontierId::from_uuid(uuid(0xa129)),
                TurnAttemptId::from_uuid(uuid(0xa12a)),
            ),
        )
        .await?;
    let terminal: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind,
                runner_recovery_runner_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let retained_loss: String = sqlx::query_scalar(
        "SELECT record.state_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let persisted_effect: (Uuid, Uuid, Decimal, Uuid) = sqlx::query_as(
        "SELECT turn_id, runner_id, placement_revision,
                yielded_turn_attempt_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(uuid(0xa120))
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        terminal,
        (
            String::from("terminal"),
            Some(String::from("cancelled")),
            None
        )
    );
    assert_eq!(retained_loss, "runner_lost_before_pin");
    assert!(
        reload.is_some(),
        "the terminalized runner wait must reload before its queued successor starts"
    );
    assert_eq!(
        persisted_effect,
        (
            turn.into_uuid(),
            runner.into_uuid(),
            Decimal::from(placement.revision().get()),
            turn_attempt.into_uuid(),
        )
    );
    assert_eq!(store.load_runner_recovery_wait(session).await?, None);
    drop(pool);
    Ok(())
}

/// stopping a recovery wait with an active tool round uses
/// the round's yielded frontier instead of the ordinary active-batch decoder.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_stop_uses_tool_round_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa150));
    let request = ToolRequestId::from_uuid(uuid(0xa15b));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa151));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    let denied_request = ToolRequestId::from_uuid(uuid(0xa15c));
    append_denied_request_to_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        producing_call,
        denied_request,
        boundary,
    )
    .await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let command = DurableCommandId::from_uuid(uuid(0xa152));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa15a));
    let denied_result = SemanticTranscriptEntryId::from_uuid(uuid(0xa15e));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop tool-round runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa153)),
            Some(TurnId::from_uuid(uuid(0xa154))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa155)),
                ContextFrontierId::from_uuid(uuid(0xa156)),
            ),
            |_| TurnId::from_uuid(uuid(0xa157)),
            |_| {
                (
                    vec![tool_closure, denied_result],
                    ContextFrontierId::from_uuid(uuid(0xa158)),
                )
            },
        )
        .await?;
    let persisted_source: Uuid = sqlx::query_scalar(
        "SELECT source_frontier_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;
    let denied: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(denied_result.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(persisted_source, boundary.into_uuid());
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, request.into_uuid());
    assert_eq!(denied.0, "tool_denied");
    assert_eq!(denied.1, denied_request.into_uuid());
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted_member = sqlx::query(
        "UPDATE context_frontier_delta
            SET semantic_entry_id = $1
          WHERE owning_session_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(uuid(0xa15f))
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    assert_eq!(corrupted_member.rows_affected(), 1);
    let malformed = sqlx::query("SELECT assert_cancelled_turn_final_state($1)")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("runner recovery cancellation authenticates every tool-result suffix member");

    assert_check_violation(malformed);
    drop(pool);
    Ok(())
}

/// stopping a runner wait preserves an interrupted
/// external-effect attempt as reconciliation-required at the round boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_stop_preserves_tool_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), side_effecting_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                    .expect("the exact fixture directory is valid"),
            ),
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
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
            authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::ExternalEffect),
            offer_request(),
        )
        .expect("the external-effect fixture pins the placement");
    store.store_pin(&pin, &registration).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    let request = ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa160));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    let mut recovery = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut recovery,
        session,
        Some("connection"),
        None,
        Some(interrupted_attempt),
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = $4
          WHERE turn_id = $5 AND session_id = $6",
    )
    .bind(producing_call.into_uuid())
    .bind(expected_enrollment.runner().into_uuid())
    .bind(Decimal::from(pin.placement.revision().get()))
    .bind(interrupted_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let command = DurableCommandId::from_uuid(uuid(0xa161));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa162));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa163));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop interrupted runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa164)),
            Some(TurnId::from_uuid(uuid(0xa165))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa166)),
                ContextFrontierId::from_uuid(uuid(0xa167)),
            ),
            |_| TurnId::from_uuid(uuid(0xa168)),
            |_| (vec![tool_closure], terminal_frontier),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa169)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa16a)),
                ContextFrontierId::from_uuid(uuid(0xa16b)),
                TurnAttemptId::from_uuid(uuid(0xa16c)),
            ),
        )
        .await?;
    let persisted: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                lifecycle.terminal_tool_attempt_id,
                effect.source_frontier_id
           FROM turn_lifecycle AS lifecycle
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.turn_id = lifecycle.turn_id
            AND effect.session_id = lifecycle.session_id
          WHERE effect.command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(persisted.0, "reconciliation_required");
    assert_eq!(persisted.1, interrupted_attempt.into_uuid());
    assert_eq!(persisted.2, boundary.into_uuid());
    assert!(
        reload.is_some(),
        "the terminalized ambiguous runner wait must reload before its queued successor starts"
    );
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, request.into_uuid());
    drop(pool);
    Ok(())
}

struct RunnerRecoveryToolRoundFacts {
    session: SessionId,
    turn: TurnId,
    turn_attempt: TurnAttemptId,
    interrupted_attempt: ToolAttemptId,
    boundary: ContextFrontierId,
    request: ToolRequestId,
    lease: RunnerLease,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    producing_call: ModelCallId,
}

async fn prepare_runner_recovery_tool_round(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_effect_kind: &'static str,
) -> Result<RunnerRecoveryToolRoundFacts, Box<dyn Error>> {
    let (session, turn, turn_attempt) = insert_running_turn(pool).await?;
    insert_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    set_fixture_physical_attempt_effect(pool, INITIAL_PHYSICAL_ATTEMPT, fixture_effect_kind)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), fixture_catalog);
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                    .expect("the exact fixture directory is valid"),
            ),
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
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
            authorize(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the retryable fixture pins the placement");
    store.store_pin(&pin, &registration).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    let request = ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa170));
    attach_continuing_tool_round_projection(
        pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    Ok(RunnerRecoveryToolRoundFacts {
        session,
        turn,
        turn_attempt,
        interrupted_attempt,
        boundary,
        request,
        lease: pin.lease,
        runner: expected_enrollment.runner(),
        placement_revision: pin.placement.revision(),
        producing_call,
    })
}

async fn park_runner_recovery_tool_round(
    pool: &PgPool,
    facts: &RunnerRecoveryToolRoundFacts,
) -> Result<(), Box<dyn Error>> {
    let mut recovery = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut recovery,
        facts.session,
        Some("connection"),
        None,
        Some(facts.interrupted_attempt),
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(facts.turn_attempt.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = $4
          WHERE turn_id = $5 AND session_id = $6",
    )
    .bind(facts.producing_call.into_uuid())
    .bind(facts.runner.into_uuid())
    .bind(Decimal::from(facts.placement_revision.get()))
    .bind(facts.interrupted_attempt.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    Ok(())
}

async fn prepare_execution_possible_retryable_runner_recovery(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_effect_kind: &'static str,
) -> Result<RunnerRecoveryToolRoundFacts, Box<dyn Error>> {
    let facts =
        prepare_runner_recovery_tool_round(pool, authorize, fixture_catalog, fixture_effect_kind)
            .await?;
    record_execution_possible_lease_loss(pool, &facts.lease).await?;
    park_runner_recovery_tool_round(pool, &facts).await?;
    Ok(facts)
}

async fn prepare_unclaimed_retryable_runner_recovery(
    pool: &PgPool,
) -> Result<
    (
        SessionId,
        TurnId,
        ToolAttemptId,
        ContextFrontierId,
        ToolRequestId,
        RunnerLeaseCorrelation,
    ),
    Box<dyn Error>,
> {
    let facts = prepare_runner_recovery_tool_round(
        pool,
        external_authorized,
        side_effecting_catalog(),
        "external_effect",
    )
    .await?;
    record_no_execution_lease_loss(pool, &facts.lease).await?;
    park_runner_recovery_tool_round(pool, &facts).await?;
    Ok((
        facts.session,
        facts.turn,
        facts.interrupted_attempt,
        facts.boundary,
        facts.request,
        facts.lease.correlation(),
    ))
}

/// stopping a retryable no-execution runner wait retires
/// its dispatch authority before cancelling and releasing the active slot.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_retires_retryable_runner_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, interrupted_attempt, boundary, request, lease) =
        prepare_unclaimed_retryable_runner_recovery(&pool).await?;
    let command = DurableCommandId::from_uuid(uuid(0xa171));
    let result_entry = SemanticTranscriptEntryId::from_uuid(uuid(0xa172));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa173));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop retryable runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa174)),
            Some(TurnId::from_uuid(uuid(0xa175))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa176)),
                terminal_frontier,
            ),
            |_| TurnId::from_uuid(uuid(0xa177)),
            |_| (vec![result_entry], terminal_frontier),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa178)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa179)),
                ContextFrontierId::from_uuid(uuid(0xa17a)),
                TurnAttemptId::from_uuid(uuid(0xa17b)),
            ),
        )
        .await?;
    let lifecycle: (String, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_attempt_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let stopped_attempt: (String, String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let result: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT entry.payload_kind, entry.tool_result_attempt_id,
                effect.source_frontier_id
           FROM semantic_transcript_entry AS entry
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.session_id = entry.source_session_id
            AND effect.command_id = $1
          WHERE entry.source_session_id = $2
            AND entry.semantic_entry_id = $3",
    )
    .bind(command.into_uuid())
    .bind(session.into_uuid())
    .bind(result_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure_request: Uuid =
        sqlx::query_scalar("SELECT request_id FROM tool_attempt WHERE attempt_id = $1")
            .bind(interrupted_attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    let consumed_loss = RunnerProtocolStore::new(pool.clone(), side_effecting_catalog())
        .load_lease_loss(lease.lease, lease.generation)
        .await?
        .expect("the stopped source lease remains loadable");
    let retry = consumed_loss
        .retry()
        .expect("the stopped no-execution loss retains checked lineage");
    let consumed_retry = retry
        .prepare_unclaimed_attempt(claimed_batch_with_effect(
            INITIAL_PHYSICAL_ATTEMPT,
            ToolEffectClass::ExternalEffect,
        ))
        .expect_err("the stop durably consumes retry preparation authority");

    assert_eq!(lifecycle.0, "cancelled");
    assert_eq!(stopped_attempt.0, "terminal");
    assert_eq!(stopped_attempt.1, "known_failed");
    assert_eq!(stopped_attempt.2, "crash_lost");
    assert_eq!(result.0, "tool_execution_result");
    assert_eq!(result.1, interrupted_attempt.into_uuid());
    assert_eq!(result.2, boundary.into_uuid());
    assert_eq!(closure_request, request.into_uuid());
    assert_eq!(consumed_retry, RunnerDomainError::InvalidState);
    assert!(
        reload.is_some(),
        "the cancelled retryable runner wait must reload before its queued successor starts"
    );
    drop(pool);
    Ok(())
}

/// stopping a pure execution-possible runner wait reloads
/// its named terminal attempt and emits the correlated crash-lost result.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_reloads_pure_attempt_from_terminal_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let facts = prepare_execution_possible_retryable_runner_recovery(
        &pool,
        authorized,
        catalog(),
        "effect_free",
    )
    .await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let resumable_loss = store
        .load_lease_loss(facts.lease.correlation().lease, facts.lease.generation())
        .await?
        .expect("the execution-possible loss remains retryable before stop");
    let reserved =
        authorize_fixture_claimed_retry(&store, &resumable_loss, ToolEffectClass::EffectFree)
            .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa184));
    let result_entry = SemanticTranscriptEntryId::from_uuid(uuid(0xa185));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa186));
    let interrupt = SubmitInput::new(
        command,
        facts.session,
        UserContent::try_text(String::from("stop pure lost runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: facts.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa187)),
            Some(TurnId::from_uuid(uuid(0xa188))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa189)),
                terminal_frontier,
            ),
            |_| TurnId::from_uuid(uuid(0xa18b)),
            |_| (vec![result_entry], terminal_frontier),
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let attempt: (String, String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(facts.interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let result: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT entry.payload_kind, entry.tool_result_attempt_id,
                effect.source_frontier_id
           FROM semantic_transcript_entry AS entry
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.session_id = entry.source_session_id
            AND effect.command_id = $1
          WHERE entry.source_session_id = $2
            AND entry.semantic_entry_id = $3",
    )
    .bind(command.into_uuid())
    .bind(facts.session.into_uuid())
    .bind(result_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    let stopped_reservation = store
        .load_claimed_retry_attempt_reservation(
            facts.lease.correlation().lease,
            facts.lease.generation(),
        )
        .await?;

    assert_eq!(reserved.source(), &facts.lease.correlation());
    assert_eq!(lifecycle, "cancelled");
    assert_eq!(attempt.0, "terminal");
    assert_eq!(attempt.1, "known_failed");
    assert_eq!(attempt.2, "crash_lost");
    assert_eq!(result.0, "tool_execution_result");
    assert_eq!(result.1, facts.interrupted_attempt.into_uuid());
    assert_eq!(result.2, facts.boundary.into_uuid());
    assert_eq!(stopped_reservation, None);
    drop(pool);
    Ok(())
}

/// stopping an idempotent execution-possible runner wait
/// reloads its named ambiguity and retains reconciliation authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_reloads_idempotent_ambiguity_from_terminal_history() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let facts = prepare_execution_possible_retryable_runner_recovery(
        &pool,
        idempotent_authorized,
        idempotent_catalog(),
        "external_effect",
    )
    .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa18c));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa18d));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa18e));
    let interrupt = SubmitInput::new(
        command,
        facts.session,
        UserContent::try_text(String::from("stop idempotent lost runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: facts.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa18f)),
            Some(TurnId::from_uuid(uuid(0xa190))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa191)),
                ContextFrontierId::from_uuid(uuid(0xa192)),
            ),
            |_| TurnId::from_uuid(uuid(0xa193)),
            |_| (vec![tool_closure], terminal_frontier),
        )
        .await?;
    let lifecycle: (String, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_tool_attempt_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let attempt: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(facts.interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(lifecycle.0, "reconciliation_required");
    assert_eq!(lifecycle.1, facts.interrupted_attempt.into_uuid());
    assert_eq!(attempt.0, "terminal");
    assert_eq!(attempt.1, "ambiguous");
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, facts.request.into_uuid());
    drop(pool);
    Ok(())
}

/// a corrupted stop cannot turn a no-execution source into
/// reconciliation-required ambiguity merely by terminalizing its attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_rejects_unclaimed_side_effecting_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, interrupted_attempt, _, _, _) =
        prepare_unclaimed_retryable_runner_recovery(&pool).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let interrupt = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0xa17c)),
        session,
        UserContent::try_text(String::from("invalid ambiguous stop"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let rejected = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa17d)),
            Some(TurnId::from_uuid(uuid(0xa17e))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa17f)),
                ContextFrontierId::from_uuid(uuid(0xa180)),
            ),
            |_| TurnId::from_uuid(uuid(0xa181)),
            |_| {
                (
                    vec![SemanticTranscriptEntryId::from_uuid(uuid(0xa182))],
                    ContextFrontierId::from_uuid(uuid(0xa183)),
                )
            },
        )
        .await
        .expect_err("no-execution loss cannot become reconciliation-required");

    assert!(
        rejected
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the deferred effect correlation must reject the wrong lease ambiguity: {rejected}"
    );
    drop(pool);
    Ok(())
}

/// an interrupt terminalizes a delegated runner-loss wait
/// through its delegation projection and retains the exact loss evidence.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_stop_terminalizes_delegated_runner_recovery_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let older_ordinary_turn = TurnId::from_uuid(uuid(0xa140));
    let older_ordinary_input = AcceptedInputId::from_uuid(uuid(0xa141));
    let older_ordinary_command = DurableCommandId::from_uuid(uuid(0xa142));
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                older_ordinary_command,
                session,
                UserContent::try_text(String::from("older queued work"))
                    .expect("the fixture input is valid"),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::try_from_u64(1)
                            .expect("the fixture defaults version is positive"),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ),
            older_ordinary_input,
            Some(older_ordinary_turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa143)),
                ContextFrontierId::from_uuid(uuid(0xa144)),
            ),
            |_| TurnId::from_uuid(uuid(0xa145)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa146))),
        )
        .await
        .expect("older ordinary work is accepted while the turn is active");
    make_accepted_turn_direct_root(&pool, older_ordinary_command, older_ordinary_input).await?;
    let stored_older_ordinary: Uuid = sqlx::query_scalar(
        "SELECT turn_id
           FROM queued_input_origin
          WHERE session_id = $1 AND accepted_input_id = $2",
    )
    .bind(session.into_uuid())
    .bind(older_ordinary_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    let starting_frontier = convert_running_turn_to_delegated_runner_recovery(
        &pool,
        session,
        turn,
        turn_attempt,
        runner,
        placement.revision(),
    )
    .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa135));
    let interrupt_successor = TurnId::from_uuid(uuid(0xa137));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop delegated runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa136)),
            Some(interrupt_successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa138)),
                ContextFrontierId::from_uuid(uuid(0xa139)),
            ),
            |_| TurnId::from_uuid(uuid(0xa13a)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa13b))),
        )
        .await?;
    let recorded_command = SubmitInputRepository::new(pool.clone())
        .load(command)
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa13c)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa13d)),
                ContextFrontierId::from_uuid(uuid(0xa13e)),
                TurnAttemptId::from_uuid(uuid(0xa13f)),
            ),
        )
        .await?;
    let reloaded_turn = reload
        .as_ref()
        .expect("the delegated interrupt successor must reload")
        .prepared()
        .turn()
        .turn();
    let terminal: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind,
                runner_recovery_runner_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let retained_loss: String = sqlx::query_scalar(
        "SELECT record.state_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let persisted_effect: (Uuid, Uuid, Decimal, Uuid, Uuid) = sqlx::query_as(
        "SELECT turn_id, runner_id, placement_revision,
                yielded_turn_attempt_id, source_frontier_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        terminal,
        (
            String::from("terminal"),
            Some(String::from("cancelled")),
            None
        )
    );
    assert_eq!(retained_loss, "runner_lost_before_pin");
    assert_eq!(
        persisted_effect,
        (
            turn.into_uuid(),
            runner.into_uuid(),
            Decimal::from(placement.revision().get()),
            turn_attempt.into_uuid(),
            starting_frontier.into_uuid(),
        )
    );
    assert_eq!(store.load_runner_recovery_wait(session).await?, None);
    assert_eq!(stored_older_ordinary, older_ordinary_turn.into_uuid());
    assert_eq!(reloaded_turn, interrupt_successor);
    assert!(
        recorded_command.is_some(),
        "the interrupt receipt must reload with its non-accepted predecessor"
    );
    assert!(
        reload.is_some(),
        "the delegated runner-recovery successor must reload after its predecessor stops"
    );
    drop(pool);
    Ok(())
}

/// placement advance and runner-recovery parking rendezvous
/// on the scheduler row, so the stale placement transaction cannot commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_serializes_with_placement_advance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *recovery)
        .await?;
    let replacement_pool = pool.clone();
    let replacement_runner = successor.runner();
    let replacement_commit = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, async {
            let mut replacement = replacement_pool.begin().await?;
            append_pre_pin_replacement_without_advancing_head(
                &mut replacement,
                session,
                replacement_runner,
            )
            .await?;
            sqlx::query(
                "UPDATE runner_current_session_placement
                    SET event_ordinal = event_ordinal + 1
                  WHERE session_id = $1",
            )
            .bind(session.into_uuid())
            .execute(&mut *replacement)
            .await?;
            replacement.commit().await
        })
        .await
    });
    let blocked = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
        .await
        .expect("placement scheduler-lock observation must remain bounded")?;

    assert!(blocked, "placement advance must wait on the scheduler lock");
    recovery.commit().await?;
    let rejected = replacement_commit
        .await
        .expect("the replacement commit task remains joinable")
        .expect("the replacement commit must finish within its task-owned timeout")
        .expect_err("the stale placement advance cannot commit after runner recovery");
    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a queued turn cannot fabricate a runner-recovery slot.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_queued_turn_cannot_enter_runner_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let session = SessionId::from_uuid(uuid(SESSION));
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let frontier = ContextFrontierId::from_uuid(uuid(0xa141));
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session.into_uuid())
    .bind(frontier.into_uuid())
    .execute(&pool)
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active', start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'awaiting_runner_recovery',
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(frontier.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await
    .expect_err("queued work cannot fabricate a runner recovery wait");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a delegated recovery wait may release its runtime slot
/// without mutating the retained physical lifecycle.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_delegated_runner_recovery_releases_runtime_slot_and_wait() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let session = SessionId::from_uuid(uuid(SESSION));
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut released = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE turn_lifecycle
            SET delegation_runtime_terminal = true
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *released)
    .await?;

    assert_eq!(updated.rows_affected(), 1);
    released.commit().await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loaded_wait = store.load_runner_recovery_wait(session).await?;
    append_pre_pin_replacement_projection(
        &pool,
        session,
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    let current_event: String = sqlx::query_scalar(
        "SELECT record.event_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(loaded_wait, None);
    assert_eq!(current_event, "pre_pin_replaced");
    drop(pool);
    Ok(())
}

/// an interrupted physical attempt must be leased to the
/// exact runner and placement revision named by the loss wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_cross_wired_lease_runner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_lease_generation
            SET runner_id = $1
          WHERE attempt_id = $2",
    )
    .bind(uuid(REPLACEMENT_RUNNER))
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot claim another runner's leased attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// the placement-loss fact itself cannot name an unrelated
/// same-session attempt that has no lease on the lost runner and revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_record_rejects_unleased_same_session_attempt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    insert_physical_attempt(&pool, PROFILELESS_PHYSICAL_ATTEMPT).await?;
    let unrelated_attempt = ToolAttemptId::from_uuid(uuid(PROFILELESS_PHYSICAL_ATTEMPT.attempt));
    mark_interrupted_attempt_ambiguous(&pool, unrelated_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(PROFILELESS_PHYSICAL_ATTEMPT.turn)),
            runner: pin.lease.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: unrelated_attempt,
            recovery_interrupted_tool_attempt: Some(unrelated_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                PROFILELESS_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner loss cannot retain an unleased same-session attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// runner recovery may retain only an ambiguous physical
/// attempt; a known terminal result cannot be reclassified as runner loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_non_ambiguous_tool_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'execution_failed'
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain a known terminal tool attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// later tool-attempt mutation cannot invalidate the exact
/// physical attempt retained by an active runner-recovery wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rechecks_changed_tool_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let rejected = sqlx::query(
        "UPDATE tool_attempt
            SET terminal_disposition_kind = 'known_failed',
                error_kind = 'execution_failed'
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("tool-attempt changes must preserve the exact runner recovery wait");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a continuation written after the wait must not leave its
/// predecessor masquerading as the yielded chain-tip recovery boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rechecks_turn_attempt_continuations() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let rejected = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(uuid(0xa16e))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a later continuation must invalidate the stale yielded boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a continuation writer takes the scheduler rendezvous
/// before inserting a successor to the yielded runner-recovery attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_serializes_turn_attempt_continuations() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut continuation = Box::pin(
        sqlx::query(
            "INSERT INTO turn_attempt
                (turn_attempt_id, turn_id, session_id,
                 continued_from_attempt_id, state_kind,
                 end_variant, end_disposition)
             VALUES ($1, $2, $3, $4, 'ended', 'without_stop',
                     'known_failure')",
        )
        .bind(uuid(0xa16e))
        .bind(turn.into_uuid())
        .bind(session.into_uuid())
        .bind(turn_attempt.into_uuid())
        .execute(&pool),
    );
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut continuation)
        .await
        .expect_err("continuation insertion must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut continuation)
        .await
        .expect("continuation admission finishes after the scheduler is released")
        .expect_err("the stale yielded boundary still rejects the continuation");

    assert_check_violation(rejected);
    drop(continuation);
    drop(pool);
    Ok(())
}

/// a lease writer takes the scheduler rendezvous before
/// advancing the lease head retained by an active runner-recovery wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_serializes_lease_head_advances() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    let stale_completion = duplicate_lease(&claimed, registration.registration())
        .complete(pin.lease.correlation())
        .expect("the pre-loss claimed snapshot admits its exact completion");
    let loss = claimed
        .lose()
        .expect("claimed side-effecting work admits execution-possible loss");
    store.store_lease_loss(&loss).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&stale_completion));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("lease admission must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut lease_store)
        .await
        .expect("lease admission finishes after the scheduler is released")
        .expect_err("the stale lease snapshot cannot advance the retained loss");

    assert_store_check_violation(rejected);
    drop(lease_store);
    drop(pool);
    Ok(())
}

/// a lease-head rewrite after wait admission must recheck
/// the exact loss event that authorized runner recovery.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rechecks_changed_lease_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let correlation = pin.lease.correlation();
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         DISABLE TRIGGER runner_current_lease_event_advances",
    )
    .execute(&pool)
    .await?;
    let rejected = sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 1
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&pool)
    .await
    .expect_err("a lease-head rewrite must preserve runner-recovery loss authority");
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         ENABLE TRIGGER runner_current_lease_event_advances",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// mutating the lease event under an unchanged head must
/// also recheck the execution-loss classification retained by the wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rechecks_changed_lease_event() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let correlation = pin.lease.correlation();
    sqlx::query(
        "ALTER TABLE runner_lease_event
         DISABLE TRIGGER runner_lease_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    let rejected = sqlx::query(
        "UPDATE runner_lease_event
            SET state_kind = 'claimed'
          WHERE lease_id = $1 AND generation = $2 AND event_ordinal = 2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&pool)
    .await
    .expect_err("lease-event mutation must preserve runner-recovery loss authority");
    sqlx::query(
        "ALTER TABLE runner_lease_event
         ENABLE TRIGGER runner_lease_event_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a completed lease cannot be reclassified as the
/// physical execution interrupted by a later runner loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_completed_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    let completed = claimed
        .complete(pin.lease.correlation())
        .expect("the exact claimed lease correlation completes");
    store.store_lease(&completed).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease completed");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an offered lease is not evidence that runner loss
/// interrupted execution.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_offered_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease is only offered");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a claimed lease without a durable loss event is not
/// evidence that runner loss interrupted execution.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_claimed_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease is only claimed");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a no-execution loss cannot be reclassified as an
/// execution-possible interrupted attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_no_execution_lease_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_no_execution_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain a proven no-execution lease loss");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an older ambiguous attempt under the same placement
/// revision cannot impersonate the operation interrupted at a later loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_attempt_matches_exact_active_round() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, later_lease) =
        stored_side_effecting_later_lease_fixture(&pool).await?;
    store.store_lease(&later_lease).await?;
    let stale_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, stale_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(LATER_LEASE_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: stale_attempt,
            recovery_interrupted_tool_attempt: Some(stale_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                LATER_LEASE_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner loss cannot retain an older round's ambiguous attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a claimed-retry predecessor is no longer the physical
/// attempt interrupted by loss after its replacement becomes current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_loss_rejects_retired_claimed_retry_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
    let retired_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: retired_attempt,
            recovery_interrupted_tool_attempt: Some(retired_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner loss cannot retain a retired claimed-retry predecessor");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a non-null runner-recovery wait names the only current
/// live or ambiguous physical attempt in its retained tool round.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_rejects_additional_round_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 1, 'inspect', 'json', '{}')",
    )
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.request))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(producing_call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, 'external_effect', 1,
                 'terminal', 'ambiguous')",
    )
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.attempt))
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.request))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(uuid(
        INITIAL_PHYSICAL_ATTEMPT.turn + RELATED_IDENTITY_OFFSET,
    ))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: producing_call,
        },
    )
    .await
    .expect_err("runner recovery cannot retain a second ambiguous round attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner-loss wait reads back only from the exact
/// current lost placement and retains the interrupted physical attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pinned_runner_recovery_wait_round_trips_exact_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let loaded_placement = store
        .load_placement(session)
        .await?
        .expect("the lost placement is present");
    let loaded_wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the exact runner recovery wait is present");

    assert_eq!(
        loaded_placement.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    assert_eq!(loaded_wait.turn(), turn);
    assert_eq!(loaded_wait.runner(), expected_enrollment.runner());
    assert_eq!(loaded_wait.placement_revision(), pin.placement.revision());
    assert_eq!(
        loaded_wait.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    let (_, _, _, _, consumed_interrupted_attempt) = loaded_placement.into_parts();
    assert_eq!(consumed_interrupted_attempt, Some(interrupted_attempt));
    drop(pool);
    Ok(())
}

/// the immutable runner-recovery interrupt effect rejects
/// statement-level truncation as well as row-level mutation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_interrupt_effect_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let rejected = sqlx::query("TRUNCATE turn_runner_recovery_interrupt_effect")
        .execute(&pool)
        .await
        .expect_err("immutable runner recovery effects cannot be truncated");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// execution-possible loss of retryable pure work parks the
/// turn with its exact in-flight source attempt for successor reissuance.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_retryable_pure_loss_wait_retains_in_flight_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let loaded_wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the retryable pure loss retains its runner recovery wait");

    assert_eq!(loaded_wait.turn(), turn);
    assert_eq!(loaded_wait.runner(), expected_enrollment.runner());
    assert_eq!(loaded_wait.placement_revision(), pin.placement.revision());
    assert_eq!(
        loaded_wait.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    drop(pool);
    Ok(())
}

/// durable no-execution proof keeps even side-effecting
/// work retryable and parks the turn with its exact in-flight source attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_unclaimed_loss_wait_retains_in_flight_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_no_execution_lease_loss(&pool, &pin.lease).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let loaded_wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the unclaimed loss retains its runner recovery wait");

    assert_eq!(loaded_wait.turn(), turn);
    assert_eq!(loaded_wait.runner(), expected_enrollment.runner());
    assert_eq!(loaded_wait.placement_revision(), pin.placement.revision());
    assert_eq!(
        loaded_wait.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    drop(pool);
    Ok(())
}

/// a pre-pin loss may park the turn without fabricating a
/// physical attempt, and that nullable arm reads back distinctly.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pre_pin_runner_recovery_wait_round_trips_without_attempt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the pre-pin runner recovery wait is present");

    assert_eq!(loaded.turn(), turn);
    assert_eq!(loaded.runner(), runner);
    assert_eq!(loaded.placement_revision(), placement.revision());
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// the discriminator alone cannot authenticate a runner
/// recovery wait against another runner's loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_wait_rejects_cross_wired_runner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let lost_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(lost_runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let rejected = insert_runner_recovery_turn(
        &pool,
        session,
        TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
        placement.revision(),
        None,
        None,
    )
    .await
    .expect_err("runner recovery must name the current placement's exact lost runner");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner wait cannot name a placement revision other
/// than the exact current loss revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_wait_rejects_cross_wired_revision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let unrelated_revision =
        RunnerGeneration::try_from_u64(2).expect("the unrelated placement revision is positive");
    let rejected = insert_runner_recovery_turn(
        &pool,
        session,
        TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
        runner,
        unrelated_revision,
        None,
        None,
    )
    .await
    .expect_err("runner recovery must name the current placement's exact revision");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner wait cannot omit the physical attempt retained
/// by the exact placement-loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_wait_requires_loss_recorded_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: None,
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery must retain the loss-recorded tool attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// generic active-phase mutation cannot reopen a runner-recovery
/// wait without the future checked replacement transaction.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_recovery_wait_rejects_generic_active_reopen() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET runner_recovery_runner_id = runner_recovery_runner_id
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("generic mutation cannot reopen a runner recovery wait");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_generic_store_rejects_pre_pin_replacement_without_command_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the current registration prepares a successor request");
    let rejected = store
        .store_placement(&replacement.placement, Some(&registration), None)
        .await
        .expect_err("the generic writer cannot invent replacement-command authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

/// initial pinning is a multi-aggregate transaction; the
/// generic placement writer cannot bypass its connection and lease authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_generic_store_rejects_initial_pin_without_lease_authority()
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
        exact_runner_request(expected_enrollment.runner()),
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
        .expect("the current registration prepares an initial pin");
    let rejected = store
        .store_placement(&pin.placement, Some(&registration), None)
        .await
        .expect_err("the generic writer cannot invent initial-pin lease authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_abandoned_pre_pin_placement_round_trips_terminal_state() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost pre-pin placement may be abandoned");
    append_abandoned_projection(&pool, abandoned.session(), None).await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the abandoned pre-pin placement is present");

    assert_eq!(loaded.placement(), &abandoned);
    assert_eq!(loaded.registration(), None);
    assert_eq!(loaded.grant(), None);
    drop(pool);
    Ok(())
}

/// pre-pin abandonment reconstitution requires its exact
/// immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pre_pin_abandonment_without_loss_predecessor()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'created', state_kind = 'unpinned',
                lost_runner_id = NULL
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin abandonment requires its exact loss predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pre-pin abandonment retains the complete authenticated
/// lineage beneath its immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pre_pin_abandonment_requires_complete_loss_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'created'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin abandonment cannot hide a missing placement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_abandoned_pinned_placement_round_trips_retained_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost pinned placement may be abandoned");
    append_abandoned_projection(&pool, abandoned.session(), None).await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the abandoned pinned placement is present");

    assert_eq!(loaded.placement(), &abandoned);
    assert_eq!(loaded.registration(), Some(&registration));
    assert_eq!(loaded.grant(), pin.grant.as_ref());
    drop(pool);
    Ok(())
}

/// the current placement pointer cannot rewind from
/// terminal abandonment to its authenticated replaceable loss predecessor.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_rewound_current_placement_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost placement may be abandoned");
    append_abandoned_projection(&pool, abandoned.session(), None).await?;
    sqlx::query("ALTER TABLE runner_current_session_placement DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement AS current_placement
            SET event_ordinal = loss.event_ordinal
           FROM runner_session_placement_record AS loss
          WHERE current_placement.session_id = $1
            AND loss.session_id = current_placement.session_id
            AND loss.event_kind = 'runner_lost'",
    )
    .bind(abandoned.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_session_placement ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(abandoned.session())
        .await
        .expect_err("the current pointer cannot hide the terminal abandonment event");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment reconstitution requires its exact
/// immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pinned_abandonment_without_loss_predecessor() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'pinned', state_kind = 'pinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment requires its exact loss predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment reconstitution authenticates the
/// retained registration against the exact loss predecessor.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pinned_abandonment_with_cross_wired_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET registration_enrollment_id = $2
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(uuid(REPLACEMENT_ENROLLMENT))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment requires its loss registration snapshot");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment retains the complete authenticated
/// lineage beneath its immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pinned_abandonment_requires_complete_loss_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'created'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment cannot hide a missing placement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// pre-pin loss reconstitution requires its exact
/// immediately preceding unpinned record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pre_pin_loss_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost_before_pin',
                state_kind = 'runner_lost_before_pin'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin loss cannot be fabricated from terminal abandonment");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned loss reconstitution requires its exact
/// immediately preceding pinned record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pinned_loss_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost', state_kind = 'runner_lost'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned loss cannot be fabricated from terminal abandonment");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// active pinned reconstitution requires the exact
/// predecessor for its admitted pinned event kind.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_pinned_state_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'pinned', state_kind = 'pinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("terminal abandonment cannot be resurrected as an active pin");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// every historical pre-pin loss authenticates its own
/// immediately preceding unpinned origin.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_abandonment_relabelled_as_historical_loss() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost_before_pin',
                state_kind = 'runner_lost_before_pin'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    append_pre_pin_replacement_projection(
        &pool,
        placement.session(),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("replacement history cannot resurrect an abandoned placement");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinning a pre-pin successor preserves authentication of
/// the complete append-only replacement history.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pinned_pre_pin_successor_requires_complete_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    let pin = replacement
        .placement
        .pin_and_offer_lease(
            &successor,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the pre-pin successor may pin the placement");
    store.store_pin(&pin, &registration).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a pin cannot hide a missing pre-pin loss boundary");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// an initial pin after pre-pin replacement authenticates
/// a freshly provisioned workspace at that successor placement revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pre_pin_successor_rejects_stale_workspace_generation() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let initial_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(initial_runner),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    };
    let placement =
        SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), initial_request.clone());
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let successor_request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(successor.runner()),
        ..initial_request
    };
    let replacement = lost
        .replace_lost_runner_before_pin(successor_request, registration.registration())
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    let successor_revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
    let successor_directory = RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
        .expect("the successor working directory is valid");
    let pin = replacement
        .placement
        .pin_and_offer_lease(
            &successor,
            registration.registration(),
            successor_directory.clone(),
            Some(ProvisionedWorkspace {
                session: SessionId::from_uuid(uuid(SESSION)),
                placement_revision: successor_revision,
                runner: successor.runner(),
                repository: None,
                canonical_clone_url_digest: None,
                credential_profile: None,
                sandbox: RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: successor_directory,
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/2/work",
                    uuid(SESSION)
                ))
                .expect("the successor private-root path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x82)),
                recovery: None,
            }),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the pre-pin successor provisions a fresh private root");
    store.store_pin(&pin, &registration).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET workspace_placement_revision = 1,
                workspace_relative_path = $2
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(format!("sessions/{}/1/work", uuid(SESSION)))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("an initial successor pin cannot retain an older workspace generation");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// loss reconstitution authenticates the complete history
/// of the pin consumed at the loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_lost_pre_pin_successor_requires_complete_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost_before_pin = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost_before_pin.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost_before_pin
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    let pin = replacement
        .placement
        .pin_and_offer_lease(
            &successor,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the pre-pin successor may pin the placement");
    store.store_pin(&pin, &registration).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned successor may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(lost.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost.session())
        .await
        .expect_err("a loss cannot hide a missing pre-pin loss boundary");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// every historical pin reconstitutes against its own
/// canonical validated registration rather than only the current successor's.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_replacement_authenticates_historical_pin_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(first_enrollment.runner()),
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &first_enrollment,
            first_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the first registration pins the exact placement");
    store.store_pin(&pin, &first_registration).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the first pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let replacement = lost
        .replace_lost_runner(
            exact_runner_request(successor_enrollment.runner()),
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact successor directory is valid"),
            None,
            None,
        )
        .expect("the live successor replaces the lost exact placement");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            None,
        )
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
    .bind(first_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(first_registration.revision().get()))
    .bind(tool("inspect").as_str())
    .bind(r#"{"different":0}"#)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_registration_tool ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a successor cannot hide a noncanonical historical registration");

    assert_store_domain_error(corrupted, RunnerDomainError::CorruptStoredFacts);
    drop(pool);
    Ok(())
}

/// every historical runner-replacement row retains the
/// closed pinned shape even when a later successor becomes current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_historical_runner_replacement_rejects_loss_metadata() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the credential-bearing pin has its grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let revoked = store
        .revoke_grant(
            lost.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?
        .expect("the active predecessor grant revokes exactly once");
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the successor directory is valid"),
            None,
            Some(revoked),
        )
        .expect("the live successor replaces the lost runner");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await?;
    let historical_replacement_revision = replacement.placement.revision();
    let replacement_grant = replacement
        .grant
        .as_ref()
        .expect("the successor carries the advanced grant");
    let profile_replacement = duplicate_placement(
        &replacement.placement,
        Some(successor_registration.registration()),
    )
    .replace_credential_profile(
        duplicate_grant(replacement_grant, successor_registration.registration()),
        successor_registration.registration(),
        replacement_profile(),
        [tool("inspect")],
    )
    .expect("the successor may replace its credential profile");
    store
        .store_placement(
            &profile_replacement.placement,
            Some(&successor_registration),
            Some(&profile_replacement.grant.grant),
        )
        .await?;
    let later_loss = profile_replacement
        .placement
        .mark_runner_lost()
        .expect("the profile-replaced successor may be marked lost");
    append_runner_lost_projection(&pool, later_loss.session()).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = pinned_runner_id,
                loss_source_kind = 'registration'
          WHERE session_id = $1
            AND event_kind = 'runner_replaced'
            AND placement_revision = $2",
    )
    .bind(later_loss.session().into_uuid())
    .bind(Decimal::from(historical_replacement_revision.get()))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(later_loss.session())
        .await
        .expect_err("a current successor cannot hide loss metadata on historical pinned state");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_generic_store_rejects_abandonment_without_scheduler_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost placement can prepare terminal abandonment");
    let rejected = store
        .store_placement(&abandoned, Some(&registration), pin.grant.as_ref())
        .await
        .expect_err("the generic writer cannot invent an empty active-turn proof");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_pin_grant_requires_complete_registration_inventory() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, expanded_advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s32_loaded_placement_retains_reconciliation_registration() -> Result<(), Box<dyn Error>> {
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
    append_runner_registration_loss_projection(&pool, lost.session()).await?;
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
async fn s32_direct_lease_admission_serializes_runner_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin, lease) = stored_later_lease_fixture(&pool).await?;
    let mut runner_loss = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut runner_loss,
        pin.placement.session(),
        Some("connection"),
        None,
        None,
    )
    .await?;
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
async fn s32_current_placement_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
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
async fn s32_current_placement_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s32_appended_placement_must_advance_current_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut malformed,
        pin.placement.session(),
        Some("connection"),
        None,
        None,
    )
    .await?;
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
async fn s31_initial_lease_rejects_cross_wired_dispatch_fence() -> Result<(), Box<dyn Error>> {
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
async fn s31_later_lease_event_rejects_cross_wired_dispatch_fence() -> Result<(), Box<dyn Error>> {
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
async fn s31_current_lease_event_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
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
async fn s31_current_lease_event_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s31_lease_event_history_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s31_appended_lease_event_must_advance_current_head() -> Result<(), Box<dyn Error>> {
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
async fn s31_every_generation_requires_offered_event_head() -> Result<(), Box<dyn Error>> {
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
async fn s30_explicit_automatic_grant_approval_cannot_be_downgraded() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
    let revoked = store
        .revoke_grant(
            lost.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?
        .expect("the active grant revokes exactly once");
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
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
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            Some(grant),
        )
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
async fn s32_grant_audit_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn s32_profile_replacement_preserves_workspace_origin_revision() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let working_directory = RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
        .expect("the fixture working directory is valid");
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let workspace_placement_revision = RunnerGeneration::one();
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
            &expected_enrollment,
            registration.registration(),
            working_directory.clone(),
            Some(ProvisionedWorkspace {
                session: SessionId::from_uuid(uuid(SESSION)),
                placement_revision: workspace_placement_revision,
                runner: expected_enrollment.runner(),
                repository: None,
                canonical_clone_url_digest: None,
                credential_profile: None,
                sandbox: RunnerSandboxProfile::WorkspaceRestricted,
                working_directory,
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/1/work",
                    uuid(SESSION)
                ))
                .expect("the private-root path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x81)),
                recovery: None,
            }),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the restricted placement provisions its private root");
    store.store_pin(&pin, &registration).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the profiled placement carries a grant");
    let replacement = duplicate_placement(&pin.placement, Some(registration.registration()))
        .replace_credential_profile(
            duplicate_grant(original_grant, registration.registration()),
            registration.registration(),
            replacement_profile(),
            [tool("inspect")],
        )
        .expect("profile replacement retains the provisioned private root");
    store
        .store_placement(
            &replacement.placement,
            Some(&registration),
            Some(&replacement.grant.grant),
        )
        .await?;
    let loaded = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the retained workspace origin revision is loadable");
    let revisions: (Decimal, Decimal) = sqlx::query_as(
        "SELECT placement_revision, workspace_placement_revision
           FROM runner_session_placement_record
          WHERE session_id = $1
          ORDER BY event_ordinal DESC
          LIMIT 1",
    )
    .bind(uuid(SESSION))
    .fetch_one(&pool)
    .await?;

    assert_eq!(loaded.placement(), &replacement.placement);
    assert_eq!(
        revisions,
        (
            Decimal::from(replacement.placement.revision().get()),
            Decimal::from(workspace_placement_revision.get()),
        ),
    );
    drop(pool);
    Ok(())
}

/// a profile replacement grant names the exact grant
/// projected by its immediately preceding placement record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_replacement_authenticates_durable_grant_predecessor()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_credential_grant
            SET prior_runner_id = $3
          WHERE session_id = $1 AND grant_revision = $2",
    )
    .bind(replacement.grant.grant.session().into_uuid())
    .bind(Decimal::from(replacement.grant.grant.revision().get()))
    .bind(uuid(REPLACEMENT_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a profile replacement cannot cross-wire its durable grant predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// a profile replacement grant belongs to the exact
/// placement event that installs it.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_replacement_authenticates_grant_placement_event() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_credential_grant AS credential_grant
            SET placement_event_ordinal = placement.event_ordinal
           FROM runner_session_placement_record AS placement
          WHERE credential_grant.session_id = $1
            AND credential_grant.grant_revision = $2
            AND placement.session_id = credential_grant.session_id
            AND placement.event_kind = 'pinned'",
    )
    .bind(replacement.grant.grant.session().into_uuid())
    .bind(Decimal::from(replacement.grant.grant.revision().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a profile replacement grant cannot name another placement event");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::MissingCanonicalGrant);
    drop(pool);
    Ok(())
}

/// a base grant cannot borrow policy from a later
/// profiled placement that installs a different grant revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_base_grant_authenticates_policy_placement_identity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant
         DROP CONSTRAINT runner_credential_grant_revision_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_credential_grant AS credential_grant
            SET placement_event_ordinal = placement.event_ordinal
           FROM runner_session_placement_record AS placement
          WHERE credential_grant.session_id = $1
            AND credential_grant.grant_revision = $2
            AND placement.session_id = credential_grant.session_id
            AND placement.event_kind = 'profile_replaced'",
    )
    .bind(original_grant.session().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a base grant cannot borrow another grant's policy placement");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::MissingCanonicalGrant);
    drop(pool);
    Ok(())
}

/// a profile replacement cannot derive fresh credential
/// authority from a predecessor grant that durable audit already revoked.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_replacement_rejects_revoked_predecessor_grant() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    sqlx::query("ALTER TABLE runner_credential_grant_audit DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         SELECT session_id, lineage_origin_event_ordinal,
                runner_id, grant_revision, 2,
                'revoked', credential_profile_name
           FROM runner_credential_grant
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3",
    )
    .bind(original_grant.session().into_uuid())
    .bind(original_grant.runner().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant_audit ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a revoked predecessor grant cannot source a profile replacement");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// a profile replacement cannot derive credential
/// authority from a predecessor grant whose canonical issuance is absent.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_replacement_requires_predecessor_issuance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the fixture pin carries its issued credential grant");
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
    sqlx::query("ALTER TABLE runner_credential_grant_audit DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM runner_credential_grant_audit
          WHERE session_id = $1
            AND runner_id = $2
            AND grant_revision = $3
            AND audit_ordinal = 1",
    )
    .bind(original_grant.session().into_uuid())
    .bind(original_grant.runner().into_uuid())
    .bind(Decimal::from(original_grant.revision().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant_audit ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a predecessor grant without issuance cannot source a replacement");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::MissingCanonicalGrant);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_new_revoked_grant_round_trips_terminal_audit() -> Result<(), Box<dyn Error>> {
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
async fn s32_grant_audit_kind_is_revision_bound() -> Result<(), Box<dyn Error>> {
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
async fn s32_relational_placement_binds_selected_grant() -> Result<(), Box<dyn Error>> {
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
             workspace_manifest_id, workspace_placement_revision,
             workspace_clone_url_digest,
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
                workspace_manifest_id, workspace_placement_revision,
                workspace_clone_url_digest,
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
async fn s32_cross_runner_grant_predecessor_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
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
    append_runner_lost_projection(&pool, lost.session()).await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    store
        .open_connection(second_enrollment.enrollment())
        .await?;
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
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &second_registration,
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

/// runner replacement provisions any runner-owned
/// workspace at the successor placement revision rather than retaining an
/// older workspace generation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_replacement_rejects_stale_workspace_generation() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
    let request = SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(class()),
        working_directory: WorkingDirectorySelection::RunnerDefault,
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: no_permission_overrides(),
    };
    let placement =
        SessionRunnerPlacement::new(SessionId::from_uuid(uuid(SESSION)), request.clone());
    store.store_placement(&placement, None, None).await?;
    let first_directory = RunnerWorkingDirectory::try_new("/workspace/first".to_owned())
        .expect("the first working directory is valid");
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
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/1/work",
                    uuid(SESSION)
                ))
                .expect("the first private-root path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x80)),
                recovery: None,
            }),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the first restricted placement provisions its private root");
    store.store_pin(&pin, &first_registration).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let successor_revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
    let successor_directory = RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
        .expect("the successor working directory is valid");
    let replacement = lost
        .replace_lost_runner(
            request,
            successor_registration.registration(),
            successor_directory.clone(),
            Some(ProvisionedWorkspace {
                session: SessionId::from_uuid(uuid(SESSION)),
                placement_revision: successor_revision,
                runner: successor_enrollment.runner(),
                repository: None,
                canonical_clone_url_digest: None,
                credential_profile: None,
                sandbox: RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: successor_directory,
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/2/work",
                    uuid(SESSION)
                ))
                .expect("the successor private-root path is relative"),
                manifest_id: WorkspaceManifestId::from_uuid(uuid(SESSION + 0x81)),
                recovery: None,
            }),
            None,
        )
        .expect("the distinct successor provisions a fresh private root");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            None,
        )
        .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET workspace_placement_revision = 1,
                workspace_relative_path = $2
          WHERE session_id = $1
            AND event_kind = 'runner_replaced'",
    )
    .bind(replacement.placement.session().into_uuid())
    .bind(format!("sessions/{}/1/work", uuid(SESSION)))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("a replacement cannot retain an older workspace generation");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// a returning runner's replacement grant must be the exact
/// successor of the immediately preceding cross-runner grant.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_load_rejects_stale_returning_runner_grant() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
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
    let first_lost = pin
        .placement
        .mark_runner_lost()
        .expect("the first runner may be marked lost");
    append_runner_lost_projection(&pool, first_lost.session()).await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    store
        .open_connection(second_enrollment.enrollment())
        .await?;
    let second = first_lost
        .replace_lost_runner(
            request.clone(),
            second_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second".to_owned())
                .expect("the second runner directory is valid"),
            None,
            pin.grant
                .as_ref()
                .map(|grant| duplicate_grant(grant, first_registration.registration())),
        )
        .expect("the second runner advances the grant lineage");
    store
        .store_runner_replacement_projection_for_test(
            &second.placement,
            &second_registration,
            second.grant.as_ref(),
        )
        .await?;
    let second_lost = second
        .placement
        .mark_runner_lost()
        .expect("the second runner may be marked lost");
    append_runner_lost_projection(&pool, second_lost.session()).await?;
    let returning = second_lost
        .replace_lost_runner(
            request,
            first_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/returning".to_owned())
                .expect("the returning runner directory is valid"),
            None,
            second
                .grant
                .as_ref()
                .map(|grant| duplicate_grant(grant, second_registration.registration())),
        )
        .expect("the original runner may return with the next grant revision");
    store
        .store_runner_replacement_projection_for_test(
            &returning.placement,
            &first_registration,
            returning.grant.as_ref(),
        )
        .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET credential_grant_revision = 1
          WHERE session_id = $1 AND event_kind = 'runner_replaced'
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(returning.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(returning.placement.session())
        .await
        .expect_err("a returning runner cannot reuse its stale grant revision");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// S32: a profile-free tombstone retains the predecessor placement's
/// approval policy even when the successor placement selects a different one.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_free_tombstone_uses_predecessor_approval_policy() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
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
                relative_path: WorkspaceRelativePath::try_new(format!(
                    "sessions/{}/1/work",
                    uuid(SESSION)
                ))
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
    append_runner_lost_projection(&pool, lost.session()).await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    store
        .open_connection(second_enrollment.enrollment())
        .await?;
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
        .store_runner_replacement_projection_for_test(
            &profile_free.placement,
            &second_registration,
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
    let profile_free_lost = profile_free
        .placement
        .mark_runner_lost()
        .expect("the profile-free runner may be marked lost");
    append_runner_lost_projection(&pool, profile_free_lost.session()).await?;
    let reloaded_lost = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the carried tombstone keeps its originating approval policy");

    assert_eq!(reloaded_lost.placement(), &profile_free_lost);
    assert_eq!(reloaded_lost.grant(), Some(&tombstone));
    let later_enrollment = RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(LATER_ENROLLMENT)),
        RunnerId::from_uuid(uuid(LATER_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(LATER_AUTHENTICATION)),
        [class()],
    );
    store.insert_enrollment(&later_enrollment).await?;
    let later_registration = store.register(&later_enrollment, advertisement()).await?;
    store.open_connection(later_enrollment.enrollment()).await?;
    let second_profile_free = profile_free_lost
        .replace_lost_runner(
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::CapabilityClass(class()),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: RunnerSandboxProfile::Ambient,
                permission_overrides: no_permission_overrides(),
            },
            later_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/later".to_owned())
                .expect("the later runner directory is valid"),
            None,
            Some(tombstone),
        )
        .expect("a second profile-free replacement carries the original approvals");
    let successor_tombstone = second_profile_free
        .grant
        .as_ref()
        .expect("the second profile-free replacement advances the tombstone");
    store
        .store_runner_replacement_projection_for_test(
            &second_profile_free.placement,
            &later_registration,
            Some(successor_tombstone),
        )
        .await?;
    let reloaded_successor = store
        .load_placement(SessionId::from_uuid(uuid(SESSION)))
        .await?
        .expect("the successor tombstone remains loadable from active policy");

    assert_eq!(
        reloaded_successor.placement(),
        &second_profile_free.placement
    );
    assert_eq!(reloaded_successor.grant(), Some(successor_tombstone));
    drop(pool);
    Ok(())
}

/// grant policy resolution follows the exact durable
/// predecessor chain and ignores a later sibling sharing its lineage origin.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_grant_policy_resolution_excludes_sibling_lineage() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let replacement = lost
        .replace_lost_runner(
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::CapabilityClass(class()),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: RunnerSandboxProfile::Ambient,
                permission_overrides: no_permission_overrides(),
            },
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/successor".to_owned())
                .expect("the successor working directory is valid"),
            None,
            pin.grant,
        )
        .expect("the profile-free successor carries a grant tombstone");
    let tombstone = replacement
        .grant
        .as_ref()
        .expect("the replacement retains its terminal grant");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            Some(tombstone),
        )
        .await?;
    let expected_policy_event: Decimal = sqlx::query_scalar(
        "SELECT event_ordinal
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(replacement.placement.session().into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_credential_grant
            (session_id, lineage_origin_event_ordinal, runner_id,
             grant_revision, credential_profile_name,
             registration_enrollment_id, registration_revision,
             placement_event_ordinal, prior_runner_id,
             prior_grant_revision, tool_count)
         SELECT grant_record.session_id,
                grant_record.lineage_origin_event_ordinal,
                $2, grant_record.grant_revision + 1,
                grant_record.credential_profile_name,
                grant_record.registration_enrollment_id,
                grant_record.registration_revision,
                loss.event_ordinal, grant_record.runner_id,
                grant_record.grant_revision, 0
           FROM runner_credential_grant AS grant_record
           JOIN runner_session_placement_record AS loss
             ON loss.session_id = grant_record.session_id
            AND loss.event_kind = 'runner_lost'
          WHERE grant_record.session_id = $1
            AND grant_record.grant_revision = 1",
    )
    .bind(replacement.placement.session().into_uuid())
    .bind(uuid(LATER_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let actual_policy_event = store
        .load_current_grant_policy_event_for_test(replacement.placement.session())
        .await?
        .expect("the retained grant has an authenticated policy event");
    let loaded = store
        .load_placement(replacement.placement.session())
        .await?
        .expect("the sibling grant does not corrupt the authenticated chain");

    assert_eq!(actual_policy_event, expected_policy_event);
    assert_eq!(loaded.grant(), Some(tombstone));
    drop(pool);
    Ok(())
}

/// the grant policy loader fails closed when a corrupted
/// revision-one grant names itself as its predecessor.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_grant_policy_rejects_cyclic_base_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_credential_grant
             DROP CONSTRAINT runner_credential_grant_revision_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_credential_grant
            SET prior_runner_id = runner_id,
                prior_grant_revision = grant_revision
          WHERE session_id = $1 AND grant_revision = 1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a base grant cannot name itself as its predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// the grant policy loader fails closed when a corrupted
/// successor grant names a revision-one predecessor that does not exist.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_grant_policy_rejects_missing_base_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query("ALTER TABLE runner_credential_grant DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_credential_grant_audit DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_credential_grant_audit DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_credential_grant
            SET grant_revision = 2,
                prior_runner_id = runner_id,
                prior_grant_revision = 1,
                tool_count = 0
          WHERE session_id = $1 AND grant_revision = 1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_credential_grant_audit
            SET grant_revision = 2,
                event_kind = 'replaced'
          WHERE session_id = $1 AND grant_revision = 1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_current_credential_grant_audit
            SET grant_revision = 2,
                event_kind = 'replaced'
          WHERE session_id = $1 AND grant_revision = 1",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET credential_grant_revision = 2
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_credential_grant_audit ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_credential_grant_audit ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_credential_grant ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a successor grant must reach its canonical base grant");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_profile_free_replacement_preserves_grant_lineage() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let first_enrollment = enrollment();
    store.insert_enrollment(&first_enrollment).await?;
    let first_registration = store.register(&first_enrollment, advertisement()).await?;
    store.open_connection(first_enrollment.enrollment()).await?;
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
    append_runner_lost_projection(&pool, first_lost.session()).await?;
    let second_enrollment = replacement_enrollment();
    store.insert_enrollment(&second_enrollment).await?;
    let second_registration = store.register(&second_enrollment, advertisement()).await?;
    store
        .open_connection(second_enrollment.enrollment())
        .await?;
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
        .store_runner_replacement_projection_for_test(
            &profile_free.placement,
            &second_registration,
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
    append_runner_lost_projection(&pool, second_lost.session()).await?;
    let later_enrollment = RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(LATER_ENROLLMENT)),
        RunnerId::from_uuid(uuid(LATER_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(LATER_AUTHENTICATION)),
        [class()],
    );
    store.insert_enrollment(&later_enrollment).await?;
    let later_registration = store.register(&later_enrollment, advertisement()).await?;
    store.open_connection(later_enrollment.enrollment()).await?;
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
        .store_runner_replacement_projection_for_test(
            &later.placement,
            &later_registration,
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
async fn s32_worktree_pin_requires_provisioned_facts() -> Result<(), Box<dyn Error>> {
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
async fn s31_claimed_retry_reservation_rejects_terminal_source() -> Result<(), Box<dyn Error>> {
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
    let replacement = loss
        .retry()
        .expect("the durable loss carries checked retry authority")
        .prepare_claimed_attempt(
            claimed_batch_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::EffectFree),
            ToolAttemptId::from_uuid(uuid(RETRY_PHYSICAL_ATTEMPT.attempt)),
        )
        .expect("the owning batch produces the exact replacement attempt");
    terminalize_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let rejected = store
        .store_claimed_retry_attempt_authority(&loss, &replacement)
        .await
        .expect_err("a stopped source attempt cannot reserve retry authority");
    let reservation_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM runner_claimed_retry_attempt_authority
          WHERE source_lease_id = $1 AND source_generation = $2",
    )
    .bind(pin.lease.correlation().lease.into_uuid())
    .bind(Decimal::from(pin.lease.generation().get()))
    .fetch_one(&pool)
    .await?;

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert_eq!(reservation_count, 0);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_replacement_attempt_commits_only_with_successor_lease() -> Result<(), Box<dyn Error>> {
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
async fn s31_idempotent_claimed_loss_retires_physical_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s31_claimed_retry_state_survives_reconstitution() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s31_adapter_rejects_caller_reconstituted_no_execution_proof() -> Result<(), Box<dyn Error>>
{
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
async fn s31_unclaimed_retry_authority_survives_reconstitution() -> Result<(), Box<dyn Error>> {
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
async fn s31_unclaimed_loss_requires_live_source_attempt() -> Result<(), Box<dyn Error>> {
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
async fn s31_retryable_loss_serializes_with_attempt_termination() -> Result<(), Box<dyn Error>> {
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
async fn s31_first_generation_requires_null_predecessor() -> Result<(), Box<dyn Error>> {
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
async fn s31_relational_retry_rejects_claimed_attempt_reuse() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
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
async fn s30_reconstitution_rejects_cross_wired_registration() -> Result<(), Box<dyn Error>> {
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
async fn s30_reconstitution_requires_trusted_catalog_declarations() -> Result<(), Box<dyn Error>> {
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
async fn s30_reconstitution_rejects_noncanonical_tool_schema() -> Result<(), Box<dyn Error>> {
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
async fn s30_idempotent_registration_tool_requires_runner_only_locus() -> Result<(), Box<dyn Error>>
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
async fn s30_registration_tool_requires_selector_discriminator() -> Result<(), Box<dyn Error>> {
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
async fn s30_registration_profile_approval_requires_tool_name_shape() -> Result<(), Box<dyn Error>>
{
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
async fn s30_reconstitution_rejects_cross_wired_enrollment() -> Result<(), Box<dyn Error>> {
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

/// each terminal physical connection advances one enrollment-owned
/// append-only loss epoch with its exact connection source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_terminal_connections_advance_exact_loss_epochs() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let first_terminal = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the first terminal connection remains durable");
    let first_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the first terminal connection advances a loss epoch");
    let second_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            second_connection.epoch(),
            RunnerConnectionTransition::HeartbeatTimeout,
        )
        .await?;
    let second_terminal = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the successor terminal connection remains durable");
    let second_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the successor terminal connection advances the loss epoch");

    assert_eq!(first_loss.loss_epoch().get(), 1);
    assert_eq!(first_loss.connection_epoch(), first_terminal.epoch());
    assert_eq!(
        first_loss.connection_event_ordinal(),
        first_terminal.event_ordinal()
    );
    assert_eq!(second_loss.loss_epoch().get(), 2);
    assert_eq!(second_loss.connection_epoch(), second_terminal.epoch());
    assert_eq!(
        second_loss.connection_event_ordinal(),
        second_terminal.event_ordinal()
    );
    drop(pool);
    Ok(())
}

/// failure to advance the durable loss epoch rolls the terminal
/// connection event back at the same commit boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_loss_epoch_failure_rolls_back_terminal_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query(
        "CREATE FUNCTION reject_runner_loss_epoch_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'injected runner loss epoch refusal'
                 USING ERRCODE = '23514';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_runner_loss_epoch_for_test
         BEFORE INSERT ON runner_connection_loss_epoch
         FOR EACH ROW EXECUTE FUNCTION reject_runner_loss_epoch_for_test()",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await
        .expect_err("terminal connection and loss epoch share one commit boundary");
    sqlx::query(
        "DROP TRIGGER reject_runner_loss_epoch_for_test
         ON runner_connection_loss_epoch",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION reject_runner_loss_epoch_for_test()")
        .execute(&pool)
        .await?;
    let retained = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the established connection remains current");
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?;

    assert_store_check_violation(rejected);
    assert_eq!(retained, connection);
    assert_eq!(loss, None);
    drop(pool);
    Ok(())
}

/// a loss epoch may name only its exact terminal connection source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_loss_epoch_rejects_connected_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let rejected = sqlx::query(
        "INSERT INTO runner_connection_loss_epoch
            (enrollment_id, loss_epoch, connection_epoch,
             connection_event_ordinal)
         VALUES ($1, 1, $2, $3)",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(connection.epoch().get()))
    .bind(Decimal::from(connection.event_ordinal()))
    .execute(&pool)
    .await
    .expect_err("a live connection cannot mint a terminal loss fence");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a terminal connection fences the placement's later lease
/// offers even after the enrollment opens a successor physical connection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_loss_fences_placement_across_successor_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let rejected = store
        .store_lease(&lease)
        .await
        .expect_err("a terminal connection cannot authorize a later lease offer");
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let reconnect_rejected = store
        .store_lease(&lease)
        .await
        .expect_err("reconnect cannot erase the placement's observed loss fence");
    let loaded = store
        .load_lease(lease.correlation().lease, lease.correlation().generation)
        .await?;

    assert_store_check_violation(rejected);
    assert_store_check_violation(reconnect_rejected);
    assert_eq!(loaded, None);
    drop(pool);
    Ok(())
}

/// an exact runner selected before connection loss cannot
/// be pinned after reconnect without an explicit placement replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_exact_selection_loss_rejects_post_reconnect_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let expected_unpinned_state = placement.state().clone();
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the domain pin retains the pre-loss exact selection");
    let rejected = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("the adapter rejects a pin whose exact selection predates loss");
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the unpinned selection remains current");

    assert_store_check_violation(rejected);
    assert_eq!(loaded.placement().request(), pin.placement.request());
    assert_eq!(loaded.placement().revision(), pin.placement.revision());
    assert_eq!(loaded.placement().state(), &expected_unpinned_state);
    drop(pool);
    Ok(())
}

/// an exact identity selected before its enrollment exists
/// derives its first loss baseline at pin when no intervening loss exists.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_pre_enrollment_exact_selection_pins_without_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the newly enrolled exact selection prepares its initial pin");
    let expected_state = pin.placement.state().clone();
    store.store_pin(&pin, &registration).await?;
    let baseline: (Uuid, Option<Decimal>) = sqlx::query_as(
        "SELECT loss_fence_enrollment_id, observed_runner_loss_epoch
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the first-baseline pin remains current");

    assert_eq!(baseline.0, expected_enrollment.enrollment().into_uuid());
    assert_eq!(baseline.1, None);
    assert_eq!(loaded.placement().state(), &expected_state);
    drop(pool);
    Ok(())
}

/// an exact selection created after reconnect observes the
/// prior loss epoch and may pin on the live successor connection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_post_reconnect_selection_pins_with_fresh_loss_baseline() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its durable loss epoch");
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the post-reconnect exact selection can be pinned");
    store.store_pin(&pin, &registration).await?;
    let baseline: (Uuid, Decimal) = sqlx::query_as(
        "SELECT loss_fence_enrollment_id, observed_runner_loss_epoch
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let loaded = store
        .load_lease(
            pin.lease.correlation().lease,
            pin.lease.correlation().generation,
        )
        .await?
        .expect("the fresh-baseline lease is durable");

    assert_eq!(baseline.0, expected_enrollment.enrollment().into_uuid());
    assert_eq!(baseline.1, Decimal::from(loss.loss_epoch().get()));
    assert_eq!(loaded, pin.lease);
    drop(pool);
    Ok(())
}

/// placement callers cannot forge the adapter-derived loss
/// baseline, even when the supplied enrollment and epoch exist durably.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_placement_loss_baseline_rejects_caller_input() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the supplied fixture epoch exists durably");
    let rejected = sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count,
             loss_fence_enrollment_id, observed_runner_loss_epoch)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'ambient', 0, 'unpinned', 0, $3, $4)",
    )
    .bind(uuid(SESSION))
    .bind(expected_enrollment.runner().into_uuid())
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await
    .expect_err("placement input cannot supply its own observed loss baseline");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// placement pin takes the scheduler before
/// runner authority, so a loss that commits while pin waits is rechecked and
/// rejects the stale exact selection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_connection_loss_serializes_exact_selection_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let expected_unpinned_state = placement.state().clone();
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the exact selection prepares its initial pin");
    let mut scheduler = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *scheduler)
    .await?;
    let mut pin_store = Box::pin(store.store_pin(&pin, &registration));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut pin_store)
        .await
        .expect_err("pin waits at the scheduler before runner authority");
    tokio::time::timeout(
        LOCK_COMPLETION_TIMEOUT,
        store.transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        ),
    )
    .await
    .expect("connection loss does not wait behind the session scheduler")?;
    scheduler.commit().await?;
    let rejected = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, pin_store)
        .await
        .expect("pin resumes after the scheduler lock is released")
        .expect_err("the resumed pin observes the committed loss baseline");
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the exact selection remains unpinned after loss wins");
    let lost_connection = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection remains the durable head");

    assert_eq!(lost_connection.state(), RunnerConnectionState::Lost);
    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert_eq!(loaded.placement().state(), &expected_unpinned_state);
    drop(pool);
    Ok(())
}

/// clean shutdown is terminal for its exact connection
/// epoch and cannot strand a newly offered lease behind unusable authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_shutdown_connection_rejects_later_lease_offer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::DaemonShutdown,
        )
        .await?;
    let rejected = store
        .store_lease(&lease)
        .await
        .expect_err("a cleanly shut down connection cannot authorize a lease offer");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// once a terminal transition owns enrollment authority,
/// a concurrent lease offer observes the committed loss fence and is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_connection_loss_wins_concurrent_lease_offer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let mut loss_store = Box::pin(store.transition_connection(
        enrollment,
        connection.epoch(),
        RunnerConnectionTransition::TransportClosed,
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut loss_store)
        .await
        .expect_err("the terminal transition waits on connection authority");
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease offer waits behind terminal enrollment authority");
    authority.commit().await?;
    loss_store.await?;
    let rejected = lease_store
        .await
        .expect_err("the later lease offer observes the committed loss fence");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a lease offer that already owns enrollment authority
/// commits before a racing terminal transition installs the loss fence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_lease_offer_wins_concurrent_connection_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease offer waits on connection authority");
    let mut loss_store = Box::pin(store.transition_connection(
        enrollment,
        connection.epoch(),
        RunnerConnectionTransition::TransportClosed,
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut loss_store)
        .await
        .expect_err("the terminal transition waits behind enrollment authority");
    authority.commit().await?;
    lease_store.await?;
    loss_store.await?;
    let loaded = store
        .load_lease(lease.correlation().lease, lease.correlation().generation)
        .await?
        .expect("the earlier lease offer remains durable");
    let loss = store
        .load_current_connection_loss(enrollment)
        .await?
        .expect("the later terminal transition advances the loss fence");

    assert_eq!(loaded, lease);
    assert_eq!(loss.connection_epoch(), connection.epoch());
    drop(pool);
    Ok(())
}

/// a claim retains the exact connection/loss baseline that
/// authorized its offer, so neither terminal loss nor a successor connection
/// can revive the stale execution capability.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s31_loss_fences_offered_lease_claim_across_reconnect() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    store
        .transition_connection(
            enrollment,
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let lost_rejection = store
        .store_lease(&claimed)
        .await
        .expect_err("terminal loss fences the outstanding lease claim");
    store.open_connection(enrollment).await?;
    let successor_rejection = store
        .store_lease(&claimed)
        .await
        .expect_err("a successor connection cannot revive the prior offer");

    assert_store_check_violation(lost_rejection);
    assert_store_check_violation(successor_rejection);
    drop(pool);
    Ok(())
}

/// terminal loss that reaches connection authority first
/// fences a concurrently queued claim before execution capability is issued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_connection_loss_wins_concurrent_lease_claim() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let loss_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loss_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            loss_store.transition_connection(
                enrollment,
                connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            ),
        )
        .await
    });
    let loss_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let claim_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let claim_task = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, claim_store.store_lease(&claimed)).await
    });
    let claim_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let authority_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, authority.commit()).await;
    let loss_result = loss_task.await;
    let claim_result = claim_task.await;
    let loss_blocked = loss_observation.expect("loss lock observation must remain bounded")?;
    let claim_blocked = claim_observation.expect("claim lock observation must remain bounded")?;
    authority_commit.expect("connection-authority blocker commit must remain bounded")?;
    loss_result
        .expect("loss task must remain joinable")
        .expect("loss must finish within its task-owned timeout")?;
    let rejected = claim_result
        .expect("claim task must remain joinable")
        .expect("claim must finish within its task-owned timeout")
        .expect_err("the claim observes the loss that won authority");

    assert!(loss_blocked, "loss must reach connection authority");
    assert!(claim_blocked, "claim must queue behind terminal loss");
    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a claim that reaches connection authority first commits
/// before a racing loss and remains the durable execution-capability boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s31_lease_claim_wins_concurrent_connection_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    let expected_state = claimed.state();
    let lease_id = claimed.correlation().lease;
    let generation = claimed.correlation().generation;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let claim_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let claim_task = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, claim_store.store_lease(&claimed)).await
    });
    let claim_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let loss_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loss_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            loss_store.transition_connection(
                enrollment,
                connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            ),
        )
        .await
    });
    let loss_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let authority_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, authority.commit()).await;
    let claim_result = claim_task.await;
    let loss_result = loss_task.await;
    let claim_blocked = claim_observation.expect("claim lock observation must remain bounded")?;
    let loss_blocked = loss_observation.expect("loss lock observation must remain bounded")?;
    authority_commit.expect("connection-authority blocker commit must remain bounded")?;
    claim_result
        .expect("claim task must remain joinable")
        .expect("claim must finish within its task-owned timeout")?;
    loss_result
        .expect("loss task must remain joinable")
        .expect("loss must finish within its task-owned timeout")?;
    let retained = store
        .load_lease(lease_id, generation)
        .await?
        .expect("the winning claim remains current after later loss");

    assert!(claim_blocked, "claim must reach connection authority");
    assert!(loss_blocked, "loss must queue behind the claim");
    assert_eq!(retained.state(), expected_state);
    drop(pool);
    Ok(())
}

/// initial pin dispatches from its immutable placement
/// record with the complete follower-visible runner facts.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_pinned_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Pinned,
        }
    );
    drop(pool);
    Ok(())
}

/// first-heartbeat suspicion dispatches only from its
/// exact connection event and retained pinned placement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_suspect_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let outbox_event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;
    let runner_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_state_transition_outbox_event")
            .fetch_one(&pool)
            .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(outbox_event_count, 1);
    assert_eq!(runner_event_count, 1);
    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Suspect,
        }
    );
    drop(pool);
    Ok(())
}

/// one connection-health transition publishes one event
/// for every session pinned to the affected enrollment.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_suspect_outbox_covers_every_pinned_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, first_pin) =
        stored_credentialless_pin_fixture(&pool).await?;
    let second_pin = store_additional_credentialless_pin_fixture(
        &pool,
        &store,
        &expected_enrollment,
        &registration,
    )
    .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let event_sessions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT session_id
           FROM runner_state_transition_outbox_event
          WHERE state_kind = 'suspect'
          ORDER BY session_id",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(event_sessions.len(), 2);
    assert_eq!(event_sessions[0], first_pin.placement.session().into_uuid());
    assert_eq!(
        event_sessions[1],
        second_pin.placement.session().into_uuid()
    );
    drop(pool);
    Ok(())
}

/// initial pin rechecks connection health under enrollment authority
/// and cannot commit after a concurrent first-heartbeat suspicion.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_initial_pin_rejects_suspect_connection() -> Result<(), Box<dyn Error>> {
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
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let expected_state = placement.state().clone();
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
        .expect("the validated registration prepares the initial pin");
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let rejected = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("a suspect connection cannot authorize initial pin");
    let retained = store
        .load_placement(pin.placement.session())
        .await?
        .expect("the rejected pin retains the unpinned placement");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert_eq!(retained.placement().state(), &expected_state);
    drop(pool);
    Ok(())
}

/// pin and heartbeat publication serialize on enrollment
/// authority, so neither can observe a split connection/placement state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn s32_initial_pin_serializes_with_heartbeat_suspicion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let setup_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    setup_store.insert_enrollment(&expected_enrollment).await?;
    let registration = setup_store
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
    setup_store.store_placement(&placement, None, None).await?;
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
        .expect("the validated registration prepares the initial pin");
    let expected_session = pin.placement.session();
    let expected_state = pin.placement.state().clone();
    let connection = setup_store
        .open_connection(expected_enrollment.enrollment())
        .await?;
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
    let pin_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let pin_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            pin_store.store_pin(&pin, &registration),
        )
        .await
    });
    let pin_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let heartbeat_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let enrollment_id = expected_enrollment.enrollment();
    let heartbeat_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            heartbeat_store.transition_connection(
                enrollment_id,
                connection.epoch(),
                RunnerConnectionTransition::HeartbeatMissed,
            ),
        )
        .await
    });
    let heartbeat_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let blocker_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocker.commit()).await;
    let pin_result = pin_task.await;
    let heartbeat_result = heartbeat_task.await;
    let pin_blocked = pin_observation.expect("pin lock observation must remain bounded")?;
    let heartbeat_blocked =
        heartbeat_observation.expect("heartbeat lock observation must remain bounded")?;
    blocker_commit.expect("enrollment blocker commit must remain bounded")?;
    pin_result
        .expect("pin task must remain joinable")
        .expect("pin must finish within its task-owned timeout")?;
    heartbeat_result
        .expect("heartbeat task must remain joinable")
        .expect("heartbeat must finish within its task-owned timeout")?;
    let retained = setup_store
        .load_placement(expected_session)
        .await?
        .expect("the serialized pin remains current");
    let runner_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_state_transition_outbox_event")
            .fetch_one(&pool)
            .await?;

    assert!(pin_blocked, "pin must reach enrollment authority");
    assert!(heartbeat_blocked, "heartbeat must queue behind the pin");
    assert_eq!(retained.placement().state(), &expected_state);
    assert_eq!(runner_event_count, 1);
    drop(pool);
    Ok(())
}

/// a follower-event refusal rolls the exact connection
/// transition back rather than leaving durable health without its update.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_suspect_outbox_failure_rolls_back_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query(
        "CREATE FUNCTION reject_runner_health_outbox_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'injected runner outbox refusal'
                 USING ERRCODE = '23514';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_runner_health_outbox_for_test
         BEFORE INSERT ON runner_state_transition_outbox_event
         FOR EACH ROW EXECUTE FUNCTION reject_runner_health_outbox_for_test()",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await
        .expect_err("connection health and its follower event share one commit boundary");
    sqlx::query(
        "DROP TRIGGER reject_runner_health_outbox_for_test
         ON runner_state_transition_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION reject_runner_health_outbox_for_test()")
        .execute(&pool)
        .await?;
    let retained = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the established connection remains current");
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;

    assert_store_check_violation(rejected);
    assert_eq!(retained.state(), connection.state());
    assert_eq!(retained.event_ordinal(), connection.event_ordinal());
    assert_eq!(event_count, 0);
    drop(pool);
    Ok(())
}

/// dispatch rejects a non-connection state that retains
/// connection provenance after post-admission corruption.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_dispatch_rejects_pinned_connection_provenance()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    sqlx::query(
        "ALTER TABLE runner_state_transition_outbox_event
            DROP CONSTRAINT runner_state_transition_outbox_source_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET state_kind = 'pinned'
          WHERE event_sequence = 1",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a pinned event cannot retain connection provenance");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// heartbeat recovery dispatches from the exact recovered
/// connection event rather than the mutable current connection head.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_connected_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let suspect = dispatch_next_outbox_event(&pool).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
    let event = dispatch_next_outbox_event_at(&pool, 2).await?;

    assert_eq!(suspect.sequence(), 1);
    assert_eq!(event.sequence(), 2);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Connected,
        }
    );
    drop(pool);
    Ok(())
}

/// a new connected epoch that supersedes durable suspicion
/// publishes recovery from the new epoch for every affected pinned session.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_reconnect_after_suspicion_publishes_connected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let suspect = dispatch_next_outbox_event(&pool).await?;
    let replacement_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let connected = dispatch_next_outbox_event_at(&pool, 2).await?;

    assert_eq!(suspect.sequence(), 1);
    assert_eq!(connected.sequence(), 2);
    assert_eq!(connected.session(), Some(session));
    assert_eq!(replacement_connection.event_ordinal(), 1);
    assert_eq!(
        connected.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Connected,
        }
    );
    drop(pool);
    Ok(())
}

/// an established epoch publishes recovery only when its
/// immediate durable predecessor is the suspicion that it supersedes.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_initial_connection_cannot_publish_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "established",
    )
    .await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Connected,
            source,
        ),
    )
    .await
    .expect_err("an initial established connection is not a recovery boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an established recovery source starts a successor epoch;
/// a cause-valid established event in the suspect epoch is not a reconnect.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_same_epoch_established_event_cannot_publish_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let mut same_epoch = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_connection_event
            (enrollment_id, connection_epoch, event_ordinal,
             state_kind, cause_kind)
         VALUES ($1, $2, 3, 'connected', 'established')",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(connection.epoch().get()))
    .execute(&mut *same_epoch)
    .await?;
    sqlx::query(
        "UPDATE runner_connection_authority_head
            SET connection_event_ordinal = 3
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&mut *same_epoch)
    .await?;
    same_epoch.commit().await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Connected,
            RunnerStateTransitionOutboxTestSource::connection(
                placement_event_ordinal,
                expected_enrollment.enrollment(),
                connection.epoch(),
                NonZeroU64::new(3).expect("the fixture event ordinal is positive"),
            ),
        ),
    )
    .await
    .expect_err("same-epoch established evidence is not a reconnect recovery");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// dispatch rechecks the immutable predecessor chain so
/// post-admission corruption cannot turn an established epoch into recovery.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_reconnect_dispatch_rejects_corrupted_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    dispatch_next_outbox_event(&pool).await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query("ALTER TABLE runner_connection_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_connection_event
            SET state_kind = 'connected',
                cause_kind = 'heartbeat_recovered'
          WHERE enrollment_id = $1
            AND connection_epoch = $2
            AND event_ordinal = 2",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(first_connection.epoch().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_connection_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("corrupted predecessor state cannot publish reconnect recovery");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// connection-state publication must name the enrollment's
/// latest durable connection event at insertion time.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_rejects_superseded_connection_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "heartbeat_missed",
    )
    .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Suspect,
            source,
        ),
    )
    .await
    .expect_err("a superseded connection event cannot publish current suspicion");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// connection-state publication is bound to the session's
/// current placement rather than a historical placement for the same runner.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_rejects_historical_connection_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "heartbeat_missed",
    )
    .await?;
    append_runner_registration_loss_projection(&pool, session).await?;
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(&mut replacement, session, None).await?;
    replacement.commit().await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Suspect,
            source,
        ),
    )
    .await
    .expect_err("a historical placement cannot publish current connection state");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// exact-identity loss before pin dispatches the retained
/// requested sandbox and user-selected directory.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_lost_before_pin_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, placement.session(), "runner_lost_before_pin").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            placement.session(),
            runner,
            placement_revision,
            placement.request().sandbox,
            Some(expected_directory.clone()),
            DispatchedRunnerState::RunnerLostBeforePin,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(placement.session()));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner,
            placement_revision,
            sandbox: placement.request().sandbox,
            working_directory: Some(expected_directory),
            state: DispatchedRunnerState::RunnerLostBeforePin,
        }
    );
    drop(pool);
    Ok(())
}

/// pinned loss dispatches against the historical loss
/// record even after the placement head can later advance.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_lost_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_lost_projection(&pool, session).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_lost").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::RunnerLost,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    append_abandoned_projection(&pool, session, None).await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::RunnerLost,
        }
    );
    drop(pool);
    Ok(())
}

/// pre-pin user replacement dispatches the successor
/// identity and successor placement revision without fabricating pinned facts.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_pre_pin_replaced_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let successor = RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_pre_pin_replacement_projection(&pool, placement.session(), successor).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, placement.session(), "pre_pin_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            placement.session(),
            successor,
            placement_revision,
            placement.request().sandbox,
            Some(expected_directory.clone()),
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(placement.session()));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: successor,
            placement_revision,
            sandbox: placement.request().sandbox,
            working_directory: Some(expected_directory),
            state: DispatchedRunnerState::Replaced,
        }
    );
    drop(pool);
    Ok(())
}

/// checked pinned replacement dispatches from the exact
/// successor placement record without requiring a directory relocation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_pinned_replaced_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(&mut replacement, session, None).await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Replaced,
        }
    );
    drop(pool);
    Ok(())
}

/// checked same-runner recovery with a new user-selected
/// directory dispatches the relocation state and exact requested directory.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_working_directory_changed_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory.clone()),
            DispatchedRunnerState::WorkingDirectoryChanged,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: Some(replacement_directory),
            state: DispatchedRunnerState::WorkingDirectoryChanged,
        }
    );
    drop(pool);
    Ok(())
}

/// a same-runner directory relocation has exactly one
/// follower state and cannot also masquerade as an ordinary replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_directory_relocation_rejects_replaced_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory),
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await
    .expect_err("a directory relocation cannot publish an ordinary replacement state");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// dispatch repeats relocation exclusivity checks so
/// post-admission state corruption cannot publish an ordinary replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_dispatch_rejects_corrupted_relocation_state()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory),
            DispatchedRunnerState::WorkingDirectoryChanged,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET state_kind = 'replaced'
          WHERE event_sequence = 1",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a corrupted relocation state cannot be offered");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// abandonment dispatches from its exact terminal
/// placement record while retaining the lost runner identity.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_abandoned_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_lost_projection(&pool, session).await?;
    append_abandoned_projection(&pool, session, None).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "abandoned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Abandoned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Abandoned,
        }
    );
    drop(pool);
    Ok(())
}

/// dispatch revalidates the immutable placement source and
/// rejects a runner event whose stored runner was corrupted after admission.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_dispatch_rejects_cross_wired_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET runner_id = $1
          WHERE event_sequence = 1",
    )
    .bind(uuid(FOREIGN_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a cross-wired runner event cannot be offered");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// the relational outbox guard refuses a transition whose
/// runner identity disagrees with its immutable placement source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn s32_runner_outbox_insert_rejects_cross_wired_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            RunnerId::from_uuid(uuid(FOREIGN_RUNNER)),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await
    .expect_err("a cross-wired runner event cannot commit");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}
