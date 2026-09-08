//! Lease coverage.

use super::*;

pub(crate) const RETRY_ATTEMPT: u128 = 0x9601;
pub(crate) const SECOND_LATER_LEASE_PHYSICAL_ATTEMPT: PhysicalAttemptFacts = PhysicalAttemptFacts {
    attempt: 0x9605,
    request: 0x9703,
    turn: 0x9803,
};
pub(crate) fn lease_with_cross_wired_dispatch(
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

pub(crate) async fn insert_lease_generation_direct(
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn current_registration_gates_new_leases() -> Result<(), Box<dyn Error>> {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn registration_replacement_serializes_later_lease_admission() -> Result<(), Box<dyn Error>> {
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

    let deadline = tokio::time::Instant::now() + SERIALIZATION_TEST_TIMEOUT;
    let (_container, pool) = tokio::time::timeout_at(deadline, migrated_postgres())
        .await
        .expect("registration replacement fixture setup must finish within its deadline")?;
    let serialization = tokio::time::timeout_at(deadline, async {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn physical_attempt_lease_binding_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let truncated = sqlx::query("TRUNCATE runner_physical_attempt_lease_binding")
        .execute(&pool)
        .await
        .expect_err("attempt-to-lease lineage cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn concurrent_attempt_binding_has_one_lease_lineage() -> Result<(), Box<dyn Error>> {
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
async fn request_cannot_start_second_lease_lineage() -> Result<(), Box<dyn Error>> {
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
async fn connected_later_lease_offer_admits_exact_claim() -> Result<(), Box<dyn Error>> {
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
async fn orphan_request_lease_binding_cannot_commit() -> Result<(), Box<dyn Error>> {
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
async fn request_lease_binding_rejects_truncate() -> Result<(), Box<dyn Error>> {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn concurrent_enrollment_revocation_blocks_a_later_lease() -> Result<(), Box<dyn Error>> {
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
async fn direct_lease_admission_serializes_enrollment_revocation() -> Result<(), Box<dyn Error>> {
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

/// later lease admission takes enrollment authority before
/// the placement head, matching profile replacement's durable lock order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn lease_offer_locks_enrollment_before_placement() -> Result<(), Box<dyn Error>> {
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
async fn initial_pin_requires_loadable_offered_lease() -> Result<(), Box<dyn Error>> {
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

/// the placement-loss fact itself cannot name an unrelated
/// same-session attempt that has no lease on the lost runner and revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_record_rejects_unleased_same_session_attempt() -> Result<(), Box<dyn Error>> {
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

/// a claimed-retry predecessor is no longer the physical
/// attempt interrupted by loss after its replacement becomes current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_rejects_retired_claimed_retry_attempt() -> Result<(), Box<dyn Error>> {
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

/// execution-possible loss of retryable pure work parks the
/// turn with its exact in-flight source attempt for successor reissuance.
#[tokio::test]
#[ignore = "requires Docker"]
async fn retryable_pure_loss_wait_retains_in_flight_attempt() -> Result<(), Box<dyn Error>> {
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

/// initial pinning is a multi-aggregate transaction; the
/// generic placement writer cannot bypass its connection and lease authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_store_rejects_initial_pin_without_lease_authority() -> Result<(), Box<dyn Error>> {
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

/// an initial pin after pre-pin replacement authenticates
/// a freshly provisioned workspace at that successor placement revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pre_pin_successor_rejects_stale_workspace_generation() -> Result<(), Box<dyn Error>> {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn direct_lease_admission_serializes_runner_loss() -> Result<(), Box<dyn Error>> {
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
async fn initial_lease_rejects_cross_wired_dispatch_fence() -> Result<(), Box<dyn Error>> {
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
async fn later_lease_event_rejects_cross_wired_dispatch_fence() -> Result<(), Box<dyn Error>> {
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
async fn current_lease_event_head_cannot_rewind() -> Result<(), Box<dyn Error>> {
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
async fn current_lease_event_head_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn lease_event_history_rejects_truncate() -> Result<(), Box<dyn Error>> {
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
async fn appended_lease_event_must_advance_current_head() -> Result<(), Box<dyn Error>> {
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
async fn every_generation_requires_offered_event_head() -> Result<(), Box<dyn Error>> {
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

/// runner replacement provisions any runner-owned
/// workspace at the successor placement revision rather than retaining an
/// older workspace generation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_replacement_rejects_stale_workspace_generation() -> Result<(), Box<dyn Error>> {
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn claimed_retry_reservation_rejects_terminal_source() -> Result<(), Box<dyn Error>> {
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
async fn replacement_attempt_commits_only_with_successor_lease() -> Result<(), Box<dyn Error>> {
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
async fn idempotent_claimed_loss_retires_physical_attempt() -> Result<(), Box<dyn Error>> {
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
async fn claimed_retry_state_survives_reconstitution() -> Result<(), Box<dyn Error>> {
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
async fn unclaimed_retry_authority_survives_reconstitution() -> Result<(), Box<dyn Error>> {
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
async fn unclaimed_loss_requires_live_source_attempt() -> Result<(), Box<dyn Error>> {
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
async fn retryable_loss_serializes_with_attempt_termination() -> Result<(), Box<dyn Error>> {
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
async fn first_generation_requires_null_predecessor() -> Result<(), Box<dyn Error>> {
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
async fn relational_retry_rejects_claimed_attempt_reuse() -> Result<(), Box<dyn Error>> {
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
