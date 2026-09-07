//! Grants coverage.

use super::*;

pub(crate) fn daemon_fallback_permission_overrides() -> RunnerToolPermissionOverrides {
    RunnerToolPermissionOverrides::try_new([(
        tool("daemon_fallback"),
        RunnerToolPermissionOverride::Confirm,
    )])
    .expect("the omitted combined-tool override fixture is valid")
}

pub(crate) fn confirmed_approved_request(facts: PhysicalAttemptFacts) -> ApprovedToolRequest {
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

pub(crate) fn confirmed_authorized_with_effect(
    facts: PhysicalAttemptFacts,
    effect: ToolEffectClass,
) -> RunnerToolAttemptAuthorization {
    authorization_from_approved(confirmed_approved_request(facts), facts, effect)
}

pub(crate) fn confirm_catalog() -> RunnerCatalog {
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

pub(crate) fn profileless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [],
        [WorkspaceCapability::WorktreePerSession],
        sandbox_profiles(),
        [repository_entry()],
    )
}

pub(crate) async fn replace_approval_with_user_command(
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

pub(crate) async fn insert_user_override_approval(
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

#[track_caller]
pub(crate) fn assert_foreign_key_violation(error: sqlx::Error) {
    assert_eq!(
        error
            .as_database_error()
            .expect("PostgreSQL reports a database error")
            .code()
            .as_deref(),
        Some("23503")
    );
}

/// profile replacement does not hide the live
/// lease offered against its pinned predecessor from later loss propagation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_finds_lease_before_profile_replacement() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn current_registration_preserves_profile() -> Result<(), Box<dyn Error>> {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn concurrent_grant_revocation_blocks_a_later_lease() -> Result<(), Box<dyn Error>> {
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
async fn grant_revocation_serializes_profile_replacement() -> Result<(), Box<dyn Error>> {
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

/// profile replacement stays durable after an availability-equivalent re-registration. The domain
/// validates the replacement against the enrollment-owned current revision while the placement
/// record carries the pinned registration snapshot forward.
#[tokio::test]
#[ignore = "requires Docker"]
async fn profile_replacement_survives_equivalent_reregistration() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn combined_tool_override_survives_omitted_runner_availability() -> Result<(), Box<dyn Error>>
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

/// a session-policy tool/profile pair admits a lease only with confirmed approval provenance;
/// policy-auto provenance is rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn session_policy_lease_requires_confirmed_provenance() -> Result<(), Box<dyn Error>> {
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

/// a one-shot user override is the user confirming one exact command in advance, so its provenance
/// admits a session-policy lease exactly as an applied user command does.
#[tokio::test]
#[ignore = "requires Docker"]
async fn session_policy_lease_admits_user_override_provenance() -> Result<(), Box<dyn Error>> {
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

/// a profileless lease on a Confirm-permission tool admits only confirmed approval provenance;
/// policy-auto provenance is rejected even for a direct lease-row insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn profileless_confirm_lease_requires_confirmed_provenance() -> Result<(), Box<dyn Error>> {
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
async fn replaced_grant_is_not_a_current_revocation_target() -> Result<(), Box<dyn Error>> {
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
async fn profile_replacement_requires_current_registration() -> Result<(), Box<dyn Error>> {
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
async fn credential_relations_admit_names_and_audit_only() -> Result<(), Box<dyn Error>> {
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
async fn grant_lineage_origin_is_part_of_every_durable_identity() -> Result<(), Box<dyn Error>> {
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
async fn pinned_affinity_and_grant_round_trip() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn pin_grant_requires_complete_registration_inventory() -> Result<(), Box<dyn Error>> {
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
async fn explicit_automatic_grant_approval_cannot_be_downgraded() -> Result<(), Box<dyn Error>> {
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
async fn grant_audit_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn profile_replacement_preserves_workspace_origin_revision() -> Result<(), Box<dyn Error>> {
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
async fn profile_replacement_authenticates_durable_grant_predecessor() -> Result<(), Box<dyn Error>>
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
async fn profile_replacement_authenticates_grant_placement_event() -> Result<(), Box<dyn Error>> {
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
async fn base_grant_authenticates_policy_placement_identity() -> Result<(), Box<dyn Error>> {
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
async fn profile_replacement_rejects_revoked_predecessor_grant() -> Result<(), Box<dyn Error>> {
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
async fn profile_replacement_requires_predecessor_issuance() -> Result<(), Box<dyn Error>> {
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
async fn new_revoked_grant_round_trips_terminal_audit() -> Result<(), Box<dyn Error>> {
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
async fn grant_audit_kind_is_revision_bound() -> Result<(), Box<dyn Error>> {
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
async fn relational_placement_binds_selected_grant() -> Result<(), Box<dyn Error>> {
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
async fn cross_runner_grant_predecessor_round_trips() -> Result<(), Box<dyn Error>> {
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

/// a returning runner's replacement grant must be the exact
/// successor of the immediately preceding cross-runner grant.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_stale_returning_runner_grant() -> Result<(), Box<dyn Error>> {
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

/// a profile-free tombstone retains the predecessor placement's approval policy even when the
/// successor placement selects a different one.
#[tokio::test]
#[ignore = "requires Docker"]
async fn profile_free_tombstone_uses_predecessor_approval_policy() -> Result<(), Box<dyn Error>> {
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
async fn grant_policy_resolution_excludes_sibling_lineage() -> Result<(), Box<dyn Error>> {
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
async fn grant_policy_rejects_cyclic_base_predecessor() -> Result<(), Box<dyn Error>> {
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
async fn grant_policy_rejects_missing_base_predecessor() -> Result<(), Box<dyn Error>> {
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
async fn profile_free_replacement_preserves_grant_lineage() -> Result<(), Box<dyn Error>> {
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
async fn registration_profile_approval_requires_tool_name_shape() -> Result<(), Box<dyn Error>> {
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
