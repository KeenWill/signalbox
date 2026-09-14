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
    provision_with_candidate_capabilities(ProvisionCase::Supported).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_rejects_registration_only_candidates_without_staging_provisioning()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::Unsupported).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_rejects_checkout_revision_without_repository_before_staging()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::RevisionWithoutRepository).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unborn_repository_placement_and_replacement_receipt_round_trip_without_a_commit()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::UnbornRepository).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProvisionCase {
    Supported,
    UnbornRepository,
    Unsupported,
    RevisionWithoutRepository,
}

async fn provision_with_candidate_capabilities(case: ProvisionCase) -> Result<(), Box<dyn Error>> {
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
            workspace: if case == ProvisionCase::UnbornRepository {
                WorkspaceRequirement::RepositoryWorktree {
                    repository: repository_key(),
                }
            } else {
                WorkspaceRequirement::None
            },
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let initial_root = case_workspace(
        case,
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
    let request = if case != ProvisionCase::Unsupported {
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
    let candidate_connection = store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: (case == ProvisionCase::RevisionWithoutRepository)
            .then(|| WorkspaceRevision::try_new("a".repeat(40)).expect("checkout SHA is valid")),
    };
    if !matches!(
        case,
        ProvisionCase::Supported | ProvisionCase::UnbornRepository
    ) {
        let rejected =
            RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Rejected(
                if case == ProvisionCase::RevisionWithoutRepository {
                    signalbox_domain::RunnerRecoveryRejection::RevisionWithoutRepository
                } else {
                    signalbox_domain::RunnerRecoveryRejection::PlacementUnavailable
                },
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
    if case == ProvisionCase::Supported {
        let mut malformed = pool.begin().await?;
        sqlx::query("ALTER TABLE runner_replacement_provisioning_authorization DISABLE TRIGGER runner_replacement_provisioning_authorization_is_append_only")
        .execute(&mut *malformed).await?;
        let error = sqlx::query("UPDATE runner_replacement_provisioning_authorization SET repository_key = $1 WHERE command_id = $2")
        .bind(repository_key().as_str()).bind(command.command_id.into_uuid())
        .execute(&mut *malformed).await.expect_err("a repository authorization without a revision cannot persist");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("runner_replacement_repository_recovery_pair")
        );
        malformed.rollback().await?;
    }
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
    let ready = case_workspace(
        case,
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
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
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

    assert_eq!(
        store.resume_runner_replacement(command.command_id).await?,
        RunnerRecoveryOutcome::Pending
    );
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
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
    let placement = store
        .load_placement(session)
        .await?
        .expect("installed placement");
    if case == ProvisionCase::UnbornRepository {
        let persisted: (String, Option<String>, String) = sqlx::query_as("SELECT workspace_recovery_kind, workspace_revision, workspace_branch_name FROM runner_session_placement_record WHERE session_id = $1 AND workspace_manifest_id = $2")
            .bind(session.into_uuid()).bind(ready.manifest_id.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(
            persisted,
            ("unborn_branch".to_owned(), None, "main".to_owned())
        );
        assert_eq!(
            placement.placement().revision(),
            authorization.placement_revision
        );
    }
    Ok(())
}

fn case_workspace(
    case: ProvisionCase,
    session: SessionId,
    runner: RunnerId,
    revision: RunnerGeneration,
) -> ProvisionedWorkspace {
    let mut workspace = private_workspace(session, runner, revision);
    if case == ProvisionCase::UnbornRepository {
        workspace.repository = Some(repository_key());
        workspace.canonical_clone_url_digest = Some(
            signalbox_domain::CanonicalCloneUrlDigest::try_new("a".repeat(64))
                .expect("arbitrary fixture digest"),
        );
        workspace.recovery = Some(signalbox_domain::WorkspaceRecovery::UnbornBranch {
            name: signalbox_domain::WorkspaceBranchName::try_new("main".to_owned())
                .expect("branch name"),
        });
        let relative = format!("sessions/{}/{}/repo", session.into_uuid(), revision.get());
        workspace.working_directory =
            RunnerWorkingDirectory::try_new(format!("/workspace/{relative}"))
                .expect("fixture directory");
        workspace.relative_path =
            WorkspaceRelativePath::try_new(relative).expect("fixed repository path");
    }
    workspace
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
    rejected_staging_releases_ready_workspace(false, false, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_releases_a_late_correlated_workspace_receipt()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(true, false, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_release_receipt_cannot_cross_the_retained_cleanup_epoch()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(false, true, false).await
}

async fn rejected_staging_releases_ready_workspace(
    late_ready: bool,
    successor_epoch: bool,
    reconnect_release: bool,
) -> Result<(), Box<dyn Error>> {
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
    if reconnect_release {
        store
            .transition_connection(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        let next = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert_ne!(next.epoch(), candidate_connection.epoch());
        store
            .reauthorize_replacement_workspace_release(
                candidate.identities().enrollment(),
                next.epoch(),
                session,
                ready.placement_revision,
                ready.manifest_id,
            )
            .await?;
        assert_eq!(
            store
                .replacement_workspace_releases(candidate.identities().enrollment())
                .await?,
            vec![ready.clone()]
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
        let next = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        store
            .reauthorize_replacement_workspace_release(
                candidate.identities().enrollment(),
                next.epoch(),
                session,
                ready.placement_revision,
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
        return Ok(());
    }
    store
        .validate_replacement_workspace_release(
            candidate.identities().enrollment(),
            session,
            ready.placement_revision,
            ready.runner,
            ready.manifest_id,
        )
        .await?;
    if successor_epoch {
        store
            .transition_connection(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        let successor = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert_ne!(successor.epoch(), candidate_connection.epoch());
        assert!(
            store
                .replacement_workspace_releases(candidate.identities().enrollment())
                .await?
                .is_empty()
        );
        assert!(
            store
                .record_replacement_workspace_released(
                    candidate.identities().enrollment(),
                    session,
                    ready.placement_revision,
                    ready.runner,
                    ready.manifest_id
                )
                .await
                .is_err()
        );
        let released: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_replacement_workspace_released WHERE authorization_id = $1").bind(authorization.authorization.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(released, 0);
        return Ok(());
    }
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

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reconnect_reauthorizes_release_and_reconciles_completed_replay()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(false, false, true).await
}
