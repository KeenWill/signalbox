//! Shared test fixtures.

use super::*;
use signalbox_persistence::test_support::postgres::TestDatabase;

pub(crate) const ENROLLMENT: u128 = 0x9100;
pub(crate) const RUNNER: u128 = 0x9200;
pub(crate) const AUTHENTICATION: u128 = 0x9300;
pub(crate) const REPLACEMENT_RUNNER: u128 = 0x9201;
pub(crate) const REPLACEMENT_AUTHENTICATION: u128 = 0x9301;
pub(crate) const LATER_ENROLLMENT: u128 = 0x9102;
pub(crate) const LATER_RUNNER: u128 = 0x9202;
pub(crate) const LATER_AUTHENTICATION: u128 = 0x9302;
pub(crate) const SESSION: u128 = 0x9400;
pub(crate) const FOREIGN_SESSION: u128 = 0x9401;
pub(crate) const SECOND_SESSION: u128 = 0x9402;
pub(crate) const LEASE: u128 = 0x9500;
pub(crate) const ATTEMPT: u128 = 0x9600;
pub(crate) const FOREIGN_RUNNER: u128 = 0x9202;
pub(crate) const RELATED_IDENTITY_OFFSET: u128 = 0x100;
#[derive(Clone, Copy)]
pub(crate) struct PhysicalAttemptFacts {
    pub(crate) attempt: u128,
    pub(crate) request: u128,
    pub(crate) turn: u128,
}

pub(crate) const INITIAL_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: ATTEMPT,
    request: 0x9700,
    turn: 0x9800,
};
pub(crate) const RETRY_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: RETRY_ATTEMPT,
    request: INITIAL_PHYSICAL_ATTEMPT.request,
    turn: INITIAL_PHYSICAL_ATTEMPT.turn,
};
pub(crate) const PROFILELESS_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9602,
    request: 0x9701,
    turn: 0x9801,
};
pub(crate) const LATER_LEASE_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9604,
    request: 0x9702,
    turn: 0x9802,
};
pub(crate) const SECOND_SESSION_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9606,
    request: 0x9704,
    turn: 0x9804,
};

pub(crate) async fn migrated_postgres() -> Result<(TestDatabase, PgPool), Box<dyn Error>> {
    let (database, pool, url) =
        signalbox_persistence::test_support::postgres::migrated_postgres(8).await?;
    pool.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&url)?.statement_cache_capacity(0))
        .await?;
    Ok((database, pool))
}

pub(crate) fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

pub(crate) async fn insert_empty_instruction_manifest(
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

pub(crate) fn class() -> RunnerCapabilityClass {
    RunnerCapabilityClass::try_new("linux.workspace".to_owned())
        .expect("the fixture capability class is valid")
}

pub(crate) fn tool(name: &str) -> ToolName {
    ToolName::try_new(name.to_owned()).expect("the fixture tool name is valid")
}

pub(crate) fn profile() -> CredentialProfileName {
    CredentialProfileName::try_new("readonly".to_owned())
        .expect("the fixture profile name is valid")
}

pub(crate) fn replacement_profile() -> CredentialProfileName {
    CredentialProfileName::try_new("operator".to_owned())
        .expect("the replacement profile name is valid")
}

pub(crate) fn sandbox_profiles() -> [RunnerSandboxProfile; 2] {
    [
        RunnerSandboxProfile::Ambient,
        RunnerSandboxProfile::WorkspaceRestricted,
    ]
}

pub(crate) fn no_permission_overrides() -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([])
        .expect("the empty permission override fixture is valid")
}

pub(crate) fn permission_overrides(
    permission: RunnerToolPermissionOverride,
) -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([(tool("inspect"), permission)])
        .expect("the exact permission override fixture is valid")
}

pub(crate) fn repository_entry() -> RunnerRepositoryEntry {
    RunnerRepositoryEntry::new(repository_key(), None)
}

pub(crate) fn model_definition() -> RunnerToolModelDefinition {
    RunnerToolModelDefinition::try_new(
        "Inspect the fixture workspace".to_owned(),
        format!(r#"{{"{}":0}}"#, "x".repeat(4096)),
    )
    .expect("the fixture model definition is valid")
}

pub(crate) fn approved_request(facts: PhysicalAttemptFacts) -> ApprovedToolRequest {
    approved_request_for_session(SessionId::from_uuid(uuid(SESSION)), facts)
}

pub(crate) fn approved_request_for_session(
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

pub(crate) fn authorization_from_approved(
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

pub(crate) fn authorization_from_approved_for_session(
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

pub(crate) fn authorized_with_effect(
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved(approved_request(facts), facts, effect)
}

pub(crate) fn claimed_batch_with_effect(
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> ToolBatch {
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

pub(crate) fn authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::EffectFree)
}

pub(crate) fn authorized_for_session(
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

pub(crate) fn external_authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::ExternalEffect)
}

pub(crate) fn idempotent_authorized(facts: PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization {
    authorized_with_effect(facts, ToolEffectClass::ExternalEffect)
}

pub(crate) fn offer_request() -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(LEASE)),
        tool: tool("inspect"),
    }
}

pub(crate) fn offer_request_for(lease: u128) -> RunnerLeaseOfferRequest {
    RunnerLeaseOfferRequest {
        lease: RunnerLeaseId::from_uuid(uuid(lease)),
        tool: tool("inspect"),
    }
}

pub(crate) fn duplicate_lease(
    lease: &RunnerLease,
    registration: &ValidatedRunnerRegistration,
) -> RunnerLease {
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

pub(crate) fn duplicate_placement(
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

pub(crate) fn duplicate_grant(
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

pub(crate) fn enrollment() -> RunnerEnrollment {
    RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(ENROLLMENT)),
        RunnerId::from_uuid(uuid(RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(AUTHENTICATION)),
        [class()],
    )
}

pub(crate) fn replacement_enrollment() -> RunnerEnrollment {
    RunnerEnrollment::new(
        RunnerEnrollmentId::from_uuid(uuid(REPLACEMENT_ENROLLMENT)),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
        RunnerAuthenticationId::from_uuid(uuid(REPLACEMENT_AUTHENTICATION)),
        [class()],
    )
}

pub(crate) fn exact_runner_directory() -> RunnerWorkingDirectory {
    RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
        .expect("the exact fixture directory is valid")
}

pub(crate) fn exact_runner_request(runner: RunnerId) -> SessionRunnerPlacementRequest {
    exact_runner_request_with_directory(runner, exact_runner_directory())
}

pub(crate) fn exact_runner_request_with_directory(
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

pub(crate) fn catalog() -> RunnerCatalog {
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

pub(crate) fn idempotent_catalog() -> RunnerCatalog {
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

pub(crate) fn side_effecting_catalog() -> RunnerCatalog {
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

pub(crate) fn advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
    .with_default_working_directory(Some(
        RunnerWorkingDirectory::try_new("/workspace/successor-default".to_owned())
            .expect("absolute fixture default"),
    ))
}

pub(crate) fn narrowed_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

pub(crate) fn expanded_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect"), tool("catalog_only")],
        [profile(), replacement_profile()],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

pub(crate) async fn stored_pin_fixture(
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

pub(crate) enum ActivePinEffectCase {
    EffectFree,
    IdempotentExternalEffect,
    SideEffectingExternalEffect,
}

pub(crate) async fn stored_active_pin_fixture_with_authorization(
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

pub(crate) async fn stored_pin_fixture_with_authorization(
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

pub(crate) async fn prepared_pin_fixture_with_authorization(
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
    prepared_pin_fixture_for_stored_session(
        pool,
        authorize,
        fixture_catalog,
        fixture_overrides,
        fixture_effect_kind,
    )
    .await
}

pub(crate) async fn prepared_pin_fixture_for_stored_session(
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

pub(crate) async fn stored_credentialless_pin_fixture(
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

pub(crate) async fn stored_later_lease_fixture(
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

pub(crate) async fn stored_later_lease_fixture_with_authorization(
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

/// The `creation_cause` a fixture writes.
///
/// `202608110001_user_role_storage_vocabulary` renamed the stored value, so a
/// fixture seeding a pool held at an earlier migration by `MIGRATOR.run_to`
/// must write the retired spelling: the `CHECK` in force there admits nothing
/// else, and the insert fails with `23514` before the migration under test
/// runs. Fully migrated pools take the current spelling.
pub(crate) const CURRENT_CREATION_CAUSE: &str = "interactive";
pub(crate) async fn insert_session_for(pool: &PgPool, session: Uuid) -> Result<(), sqlx::Error> {
    insert_session_for_with_creation_cause(pool, session, CURRENT_CREATION_CAUSE).await
}

pub(crate) async fn insert_session_for_with_creation_cause(
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

pub(crate) async fn insert_session(pool: &PgPool) -> Result<(), sqlx::Error> {
    insert_session_for(pool, uuid(SESSION)).await
}

pub(crate) async fn dispatch_next_outbox_event(
    pool: &PgPool,
) -> Result<DispatchedOutboxEvent, Box<dyn Error>> {
    dispatch_next_outbox_event_at(pool, 1).await
}

pub(crate) async fn dispatch_next_outbox_event_at(
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

pub(crate) async fn placement_outbox_facts(
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

pub(crate) async fn connection_outbox_source(
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

pub(crate) async fn insert_physical_attempt(
    pool: &PgPool,
    facts: PhysicalAttemptFacts,
) -> Result<(), sqlx::Error> {
    insert_physical_attempt_for(pool, SessionId::from_uuid(uuid(SESSION)), facts).await
}

pub(crate) async fn insert_physical_attempt_for(
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

pub(crate) async fn set_fixture_physical_attempt_effect(
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

pub(crate) async fn insert_external_physical_attempt(
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

pub(crate) async fn terminalize_physical_attempt(
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
pub(crate) async fn store_fixture_retryable_loss(
    store: &RunnerProtocolStore,
    _pool: &PgPool,
    loss: &signalbox_domain::RunnerLeaseLoss,
) -> Result<(), Box<dyn Error>> {
    store.store_lease_loss(loss).await?;
    Ok(())
}

pub(crate) async fn authorize_fixture_claimed_retry(
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
pub(crate) async fn store_fixture_claimed_retry_replacement(
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

pub(crate) async fn append_runner_lost_without_advancing_head(
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

pub(crate) async fn append_runner_lost_projection(
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

pub(crate) async fn mark_interrupted_attempt_ambiguous(
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

pub(crate) async fn record_execution_possible_lease_loss(
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

pub(crate) struct InterruptedLossRecoveryFacts {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) runner: RunnerId,
    pub(crate) placement_revision: RunnerGeneration,
    pub(crate) placement_interrupted_tool_attempt: ToolAttemptId,
    pub(crate) recovery_interrupted_tool_attempt: Option<ToolAttemptId>,
    pub(crate) active_tool_round_call: ModelCallId,
}

pub(crate) async fn insert_runner_recovery_turn_with_interrupted_loss(
    pool: &PgPool,
    facts: InterruptedLossRecoveryFacts,
) -> Result<(), sqlx::Error> {
    insert_runner_recovery_turn_with_interrupted_loss_boundary(pool, facts, "continuing").await
}

pub(crate) async fn insert_runner_recovery_turn_with_interrupted_loss_boundary(
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
             resolved_provider_model_identity_id, effective_provider_model_identity_id,
             context_frontier_id,
             credential_reference, state_kind, terminal_disposition_kind,
             turn_instruction_manifest_id)
         VALUES ($1, $2, $3, $4, 'direct', $5, $6, $6, $7,
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

pub(crate) async fn insert_running_turn(
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

pub(crate) async fn attach_continuing_tool_round_projection(
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
             resolved_provider_model_identity_id, effective_provider_model_identity_id,
             context_frontier_id,
             credential_reference, state_kind, terminal_disposition_kind,
             turn_instruction_manifest_id)
         VALUES ($1, $2, $3, $4, 'direct', $5, $6, $6, $7,
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

pub(crate) async fn append_runner_registration_loss_projection(
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

pub(crate) async fn append_same_runner_replacement_projection(
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

pub(crate) async fn append_runner_lost_before_pin_projection(
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

pub(crate) async fn append_pre_pin_replacement_projection(
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

pub(crate) async fn append_abandoned_projection(
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
pub(crate) fn assert_check_violation(error: sqlx::Error) {
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
pub(crate) fn assert_store_check_violation(error: RunnerProtocolStoreError) {
    let RunnerProtocolStoreError::Database(error) = error else {
        panic!("PostgreSQL must reject the invalid durable evidence")
    };
    assert_check_violation(error);
}

#[track_caller]
pub(crate) fn assert_store_domain_error(
    error: RunnerProtocolStoreError,
    expected: RunnerDomainError,
) {
    let RunnerProtocolStoreError::Domain(actual) = error else {
        panic!("the adapter must reject invalid domain evidence before writing")
    };
    assert_eq!(actual, expected);
}

#[track_caller]
pub(crate) fn assert_store_corruption(
    error: RunnerProtocolStoreError,
    expected: RunnerProtocolCorruption,
) {
    let RunnerProtocolStoreError::Corruption(actual) = error else {
        panic!("the adapter must return typed corruption for malformed durable evidence")
    };
    assert_eq!(actual, expected);
}

#[track_caller]
pub(crate) fn assert_one_store_succeeds_and_one_conflicts(
    first: Result<(), RunnerProtocolStoreError>,
    second: Result<(), RunnerProtocolStoreError>,
) {
    match (first, second) {
        (Ok(()), Err(error)) | (Err(error), Ok(())) => assert_store_check_violation(error),
        outcomes => panic!("one attempt binding must win exactly once: {outcomes:?}"),
    }
}
