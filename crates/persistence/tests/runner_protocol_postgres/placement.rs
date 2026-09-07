//! Placement coverage.

use super::*;

pub(crate) fn repository_key() -> WorkspaceRepositoryKey {
    WorkspaceRepositoryKey::try_new("signalbox".to_owned())
        .expect("the fixture repository key is valid")
}

pub(crate) fn always_confirm_catalog() -> RunnerCatalog {
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

pub(crate) fn workspaceless_advertisement() -> RunnerAdvertisement {
    RunnerAdvertisement::new(
        [class()],
        [tool("inspect")],
        [profile(), replacement_profile()],
        [],
        sandbox_profiles(),
        [repository_entry()],
    )
}

pub(crate) async fn clone_registration_without_advancing_head(
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

pub(crate) async fn rejected_workspace_branch(
    pool: &PgPool,
    session: SessionId,
    branch: &str,
) -> sqlx::Error {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn registration_round_trips_canonical_evidence() -> Result<(), Box<dyn Error>> {
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

/// first-pin authority decodes the closed enrollment discriminator
/// before applying active-enrollment policy.
#[tokio::test]
#[ignore = "requires Docker"]
async fn store_pin_rejects_corrupt_enrollment_discriminator() -> Result<(), Box<dyn Error>> {
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
async fn failed_registration_write_preserves_prior_authority() -> Result<(), Box<dyn Error>> {
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
async fn insert_enrollment_requires_pristine_registration_authority() -> Result<(), Box<dyn Error>>
{
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
async fn outstanding_preparation_fails_registration_before_durable_writes()
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
async fn historical_registration_load_remains_stale() -> Result<(), Box<dyn Error>> {
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
async fn stale_loaded_enrollment_cannot_bind_historical_registration() -> Result<(), Box<dyn Error>>
{
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
async fn orphan_revocation_audit_cannot_commit() -> Result<(), Box<dyn Error>> {
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
async fn historical_enrollment_audit_rechecks_its_own_revision() -> Result<(), Box<dyn Error>> {
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
async fn current_registration_preserves_complete_placement() -> Result<(), Box<dyn Error>> {
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
async fn current_registration_preserves_workspace() -> Result<(), Box<dyn Error>> {
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

/// the atomic initial pin takes the session scheduler
/// before the placement head, matching every later lease append.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn initial_pin_locks_scheduler_before_placement() -> Result<(), Box<dyn Error>> {
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
async fn placement_projection_locks_scheduler_before_placement() -> Result<(), Box<dyn Error>> {
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
async fn current_registration_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
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
async fn current_registration_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn enrollment_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn enrollment_audit_classes_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn registration_inventories_reject_truncate() -> Result<(), Box<dyn Error>> {
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
async fn appended_registration_must_advance_current_head() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_store_rejects_runner_replacement_without_command_authority()
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

/// the relational replacement shape preserves the checked future
/// same-runner recovery reserved exclusively for registration-triggered loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn registration_loss_admits_same_runner_replacement_shape() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn first_placement_record_is_created_unpinned() -> Result<(), Box<dyn Error>> {
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
async fn placement_required_flag_matches_registered_locus() -> Result<(), Box<dyn Error>> {
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

/// an exact selection that predates enrollment retains its absent
/// baseline when that late enrollment is lost before pin.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_lost_before_pin_round_trips_exact_identity() -> Result<(), Box<dyn Error>> {
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
async fn transcript_snapshot_authenticates_current_pre_pin_runner_loss()
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
async fn session_summary_authenticates_current_pre_pin_runner_loss() -> Result<(), Box<dyn Error>> {
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
async fn pre_pin_replacement_round_trips_append_only_history() -> Result<(), Box<dyn Error>> {
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
async fn pre_pin_replacement_rejects_retained_lost_selector() -> Result<(), Box<dyn Error>> {
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
async fn runner_loss_requires_the_exact_pinned_runner() -> Result<(), Box<dyn Error>> {
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
async fn generic_store_rejects_pre_pin_loss_without_transactional_authority()
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
async fn generic_store_rejects_pinned_loss_without_transactional_authority()
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_store_rejects_pre_pin_replacement_without_command_authority()
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn abandoned_pre_pin_placement_round_trips_terminal_state() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn abandoned_pinned_placement_round_trips_retained_authority() -> Result<(), Box<dyn Error>> {
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

/// every historical pin reconstitutes against its own
/// canonical validated registration rather than only the current successor's.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_replacement_authenticates_historical_pin_registration() -> Result<(), Box<dyn Error>>
{
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn loaded_placement_retains_reconciliation_registration() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn current_placement_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
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
async fn current_placement_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn appended_placement_must_advance_current_head() -> Result<(), Box<dyn Error>> {
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
async fn worktree_pin_requires_provisioned_facts() -> Result<(), Box<dyn Error>> {
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
async fn reconstitution_rejects_cross_wired_registration() -> Result<(), Box<dyn Error>> {
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
async fn idempotent_registration_tool_requires_runner_only_locus() -> Result<(), Box<dyn Error>> {
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
async fn registration_tool_requires_selector_discriminator() -> Result<(), Box<dyn Error>> {
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
async fn reconstitution_rejects_cross_wired_enrollment() -> Result<(), Box<dyn Error>> {
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

/// an exact identity selected before its enrollment exists
/// derives its first loss baseline at pin when no intervening loss exists.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pre_enrollment_exact_selection_pins_without_loss() -> Result<(), Box<dyn Error>> {
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

/// placement callers cannot forge the adapter-derived loss
/// baseline, even when the supplied enrollment and epoch exist durably.
#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_baseline_rejects_caller_input() -> Result<(), Box<dyn Error>> {
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
