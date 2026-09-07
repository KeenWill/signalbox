//! Replacement authorization and exact workspace receipt consumption.

use super::recovery_commands::enrollment_request;
use super::*;
use signalbox_domain::{
    ProvisionedWorkspace, RunnerEnrollmentState, WorkspaceManifestId, WorkspaceRelativePath,
};
use signalbox_persistence::runner_protocol::RunnerRecoveryOutcome;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_provisioning_retains_one_receipt_and_consumes_it_with_installation()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(true).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_rejects_registration_only_candidates_without_staging_provisioning()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(false).await
}

async fn provision_with_candidate_capabilities(supported: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(predecessor.identities().runner()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let initial_root = private_workspace(
        session,
        predecessor.identities().runner(),
        placement.revision(),
    );
    let pin = placement
        .pin_and_offer_lease(
            predecessor.enrollment(),
            predecessor.registration().registration(),
            initial_root.working_directory.clone(),
            Some(initial_root),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the provisioning fixture retains its correlated authority");
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store.store_pin(&pin, predecessor.registration()).await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, session).await?;
    let request = enrollment_request();
    let request = if supported {
        request
    } else {
        signalbox_persistence::runner_protocol::PristineRunnerEnrollmentRequest::new(
            request.request(),
            request.issued(),
            [class()],
            RunnerAdvertisement::new([], [], [], [], [], []),
        )
    };
    let candidate = store.enroll_pristine(request).await?.into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    if !supported {
        let rejected =
            RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Rejected(
                signalbox_domain::RunnerRecoveryRejection::PlacementUnavailable,
            ));
        assert_eq!(store.replace_lost_runner(command.clone()).await?, rejected);
        assert_eq!(store.replace_lost_runner(command).await?, rejected);
        let staged: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM runner_replacement_stage) + (SELECT count(*) FROM runner_replacement_provisioning_authorization)").fetch_one(&pool).await?;
        assert_eq!(staged, 0);
        assert_eq!(
            store
                .load_connection(candidate.identities().enrollment())
                .await?
                .expect("candidate connection remains available")
                .state(),
            RunnerConnectionState::Connected
        );
        return Ok(());
    }
    assert_eq!(
        store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );
    let operations = store
        .replacement_provisioning(candidate.identities().enrollment())
        .await?;
    assert_eq!(operations.len(), 1);
    assert_eq!(
        store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );
    assert_eq!(
        store
            .replacement_provisioning(candidate.identities().enrollment())
            .await?,
        operations
    );
    assert_eq!(
        store.resume_runner_replacement(command.command_id).await?,
        RunnerRecoveryOutcome::Pending
    );
    let authorization = &operations[0];
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    let mut wrong = ready.clone();
    wrong.runner = predecessor.identities().runner();
    assert!(
        store
            .record_replacement_workspace_ready(authorization, &wrong)
            .await
            .is_err()
    );
    store
        .record_replacement_workspace_ready(authorization, &ready)
        .await?;
    store
        .record_replacement_workspace_ready(authorization, &ready)
        .await?;
    let mut changed = ready.clone();
    changed.manifest_id = WorkspaceManifestId::from_uuid(Uuid::now_v7());
    assert!(
        store
            .record_replacement_workspace_ready(authorization, &changed)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .expect("the provisioning fixture retains its correlated authority")
            .state(),
        RunnerEnrollmentState::Pending
    );

    let result = store.resume_runner_replacement(command.command_id).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: authorization.placement_revision
        })
    );
    assert_eq!(store.replace_lost_runner(command).await?, result);
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .expect("the provisioning fixture retains its correlated authority")
            .state(),
        RunnerEnrollmentState::Active
    );
    let consumed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_workspace_consumption")
            .fetch_one(&pool)
            .await?;
    assert_eq!(consumed, 1);
    Ok(())
}

fn private_workspace(
    session: SessionId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
) -> ProvisionedWorkspace {
    let relative = format!(
        "sessions/{}/{}/work",
        session.into_uuid(),
        placement_revision.get()
    );
    ProvisionedWorkspace {
        session,
        runner,
        placement_revision,
        repository: None,
        canonical_clone_url_digest: None,
        credential_profile: None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: RunnerWorkingDirectory::try_new(format!("/workspace/{relative}"))
            .expect("absolute fixture working directory"),
        relative_path: WorkspaceRelativePath::try_new(relative)
            .expect("session-relative fixture workspace"),
        manifest_id: WorkspaceManifestId::from_uuid(Uuid::now_v7()),
        recovery: None,
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_provisioning_failure_is_terminal_and_exactly_replayed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(predecessor.identities().runner()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let initial_root = private_workspace(
        session,
        predecessor.identities().runner(),
        placement.revision(),
    );
    let pin = placement
        .pin_and_offer_lease(
            predecessor.enrollment(),
            predecessor.registration().registration(),
            initial_root.working_directory.clone(),
            Some(initial_root),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .unwrap();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store.store_pin(&pin, predecessor.registration()).await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    assert_eq!(
        store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );

    let operations = store
        .replacement_provisioning(candidate.identities().enrollment())
        .await?;
    let authorization = &operations[0];
    let detail = serde_json::json!({"code": "fixture_refusal", "message": "workspace unavailable", "payload": {}});
    let kind = signalbox_domain::RunnerProvisioningFailureKind::SandboxUnavailable;

    store
        .record_replacement_provisioning_failure(authorization, kind, &detail)
        .await?;
    store
        .record_replacement_provisioning_failure(authorization, kind, &detail)
        .await?;

    let rejected =
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Rejected(
            signalbox_domain::RunnerRecoveryRejection::ProvisioningFailed,
        ));
    assert_eq!(store.replace_lost_runner(command.clone()).await?, rejected);
    assert_eq!(
        store.resume_runner_replacement(command.command_id).await?,
        rejected
    );
    assert!(
        store
            .record_replacement_provisioning_failure(
                authorization,
                kind,
                &serde_json::json!({"changed": true})
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Pending
    );
    assert!(
        store
            .replacement_provisioning(candidate.identities().enrollment())
            .await?
            .is_empty()
    );
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    assert!(
        store
            .record_replacement_workspace_ready(authorization, &ready)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_startup_rejects_a_lost_candidate_without_consuming_or_releasing_its_receipt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(predecessor.identities().runner()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let initial_root = private_workspace(
        session,
        predecessor.identities().runner(),
        placement.revision(),
    );
    let pin = placement
        .pin_and_offer_lease(
            predecessor.enrollment(),
            predecessor.registration().registration(),
            initial_root.working_directory.clone(),
            Some(initial_root),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .unwrap();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store.store_pin(&pin, predecessor.registration()).await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    assert_eq!(
        store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );

    let operations = store
        .replacement_provisioning(candidate.identities().enrollment())
        .await?;
    let authorization = &operations[0];
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    store
        .record_replacement_workspace_ready(authorization, &ready)
        .await?;
    let connection = store
        .load_connection(candidate.identities().enrollment())
        .await?
        .unwrap();
    store
        .transition_connection(
            candidate.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;

    store.resume_runner_replacements().await?;
    store.resume_runner_replacements().await?;

    assert_eq!(
        store.replace_lost_runner(command).await?,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Rejected(
            signalbox_domain::RunnerRecoveryRejection::RunnerUnavailable
        ))
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Pending
    );
    assert!(
        store
            .replacement_workspace_releases(candidate.identities().enrollment())
            .await?
            .is_empty()
    );
    let consumed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_workspace_consumption")
            .fetch_one(&pool)
            .await?;
    assert_eq!(consumed, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_rejected_staging_releases_only_its_exact_ready_workspace()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_releases_a_late_correlated_workspace_receipt()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(true).await
}

async fn rejected_staging_releases_ready_workspace(late_ready: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(predecessor.identities().runner()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let initial_root = private_workspace(
        session,
        predecessor.identities().runner(),
        placement.revision(),
    );
    let pin = placement
        .pin_and_offer_lease(
            predecessor.enrollment(),
            predecessor.registration().registration(),
            initial_root.working_directory.clone(),
            Some(initial_root),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the recovery fixture retains its required correlated fact");
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store.store_pin(&pin, predecessor.registration()).await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    assert_eq!(
        store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );

    let operations = store
        .replacement_provisioning(candidate.identities().enrollment())
        .await?;
    let authorization = &operations[0];
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    if !late_ready {
        store
            .record_replacement_workspace_ready(authorization, &ready)
            .await?;
    }
    let candidate_connection = store
        .load_connection(candidate.identities().enrollment())
        .await?
        .expect("the recovery fixture retains its required correlated fact");
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    store
        .abandon_lost_runner(signalbox_domain::AbandonLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
        })
        .await?;

    let result = store.resume_runner_replacement(command.command_id).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Rejected(
            signalbox_domain::RunnerRecoveryRejection::PlacementNotLost
        ))
    );
    if late_ready {
        let mut wrong = ready.clone();
        wrong.runner = predecessor.identities().runner();
        assert!(
            store
                .record_replacement_workspace_ready(authorization, &wrong)
                .await
                .is_err()
        );
        store
            .record_replacement_workspace_ready(authorization, &ready)
            .await?;
        store
            .record_replacement_workspace_ready(authorization, &ready)
            .await?;
    }
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_replacement_workspace_release WHERE authorization_id = $1 AND connection_epoch = $2")
        .bind(authorization.authorization.into_uuid()).bind(Decimal::from(candidate_connection.epoch().get())).fetch_one(&pool).await?;
    assert_eq!(releases, 1);
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
    assert_eq!(
        store
            .replacement_workspace_releases(candidate.identities().enrollment())
            .await?,
        vec![ready.clone()]
    );
    assert!(
        store
            .record_replacement_workspace_released(
                candidate.identities().enrollment(),
                session,
                ready.placement_revision,
                ready.runner,
                WorkspaceManifestId::from_uuid(Uuid::now_v7())
            )
            .await
            .is_err()
    );
    store
        .record_replacement_workspace_released(
            candidate.identities().enrollment(),
            session,
            ready.placement_revision,
            ready.runner,
            ready.manifest_id,
        )
        .await?;
    store
        .record_replacement_workspace_released(
            candidate.identities().enrollment(),
            session,
            ready.placement_revision,
            ready.runner,
            ready.manifest_id,
        )
        .await?;
    assert!(
        store
            .replacement_workspace_releases(candidate.identities().enrollment())
            .await?
            .is_empty()
    );
    Ok(())
}
