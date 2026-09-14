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

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_private_placement_releases_only_to_its_connected_owner()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::RetiredWorkspace).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_plain_directory_has_neither_release_nor_leak() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, runner, _, pin) = stored_pin_fixture(&pool).await?;
    store.register(&runner, narrowed_advertisement()).await?;
    append_runner_registration_loss_projection(&pool, pin.placement.session()).await?;
    store
        .abandon_lost_runner(signalbox_domain::AbandonLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session: pin.placement.session(),
        })
        .await?;
    let retained: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM runner_workspace_release) + (SELECT count(*) FROM runner_workspace_leak)").fetch_one(&pool).await?;
    assert_eq!(retained, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_replaced_connection_cannot_load_workspace_provisioning() -> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::Reconnected).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_replaced_connection_cannot_record_or_replay_workspace_ready()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::StaleReady).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_registration_cannot_dispatch_a_prior_workspace_authorization()
-> Result<(), Box<dyn Error>> {
    provision_with_candidate_capabilities(ProvisionCase::RegistrationChanged).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProvisionCase {
    Supported,
    Reconnected,
    RegistrationChanged,
    StaleReady,
    RetiredWorkspace,
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
    let mut candidate_connection = store
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
        ProvisionCase::Supported
            | ProvisionCase::Reconnected
            | ProvisionCase::RegistrationChanged
            | ProvisionCase::StaleReady
            | ProvisionCase::RetiredWorkspace
            | ProvisionCase::UnbornRepository
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
    if case == ProvisionCase::Reconnected {
        let prior_epoch = candidate_connection.epoch();
        candidate_connection = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert_ne!(candidate_connection.epoch(), prior_epoch);
        assert!(
            store
                .replacement_provisioning(candidate.identities().enrollment(), prior_epoch)
                .await?
                .is_empty()
        );
    }
    let operations = store
        .replacement_provisioning(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
        )
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
            .replacement_provisioning(
                candidate.identities().enrollment(),
                candidate_connection.epoch()
            )
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
    let stale_epoch = if case == ProvisionCase::StaleReady {
        let prior_epoch = candidate_connection.epoch();
        candidate_connection = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert!(matches!(
            store
                .record_replacement_workspace_ready(
                    authorization,
                    prior_epoch,
                    &ready,
                    &ready_digest()
                )
                .await,
            Err(RunnerProtocolStoreError::Domain(
                signalbox_domain::RunnerDomainError::InvalidState
            ))
        ));
        let receipts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_replacement_workspace_ready WHERE authorization_id = $1",
        )
        .bind(authorization.authorization.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(receipts, 0);
        assert_eq!(
            store.resume_runner_replacement(command.command_id).await?,
            RunnerRecoveryOutcome::Pending
        );
        Some(prior_epoch)
    } else {
        None
    };
    let mut wrong = ready.clone();
    wrong.runner = predecessor.identities().runner();
    assert!(
        store
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &wrong,
                &ready_digest()
            )
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
        .record_replacement_workspace_ready(
            authorization,
            candidate_connection.epoch(),
            &ready,
            &ready_digest(),
        )
        .await?;
    store
        .record_replacement_workspace_ready(
            authorization,
            candidate_connection.epoch(),
            &ready,
            &ready_digest(),
        )
        .await?;
    if let Some(prior_epoch) = stale_epoch {
        assert!(matches!(
            store
                .record_replacement_workspace_ready(
                    authorization,
                    prior_epoch,
                    &ready,
                    &ready_digest()
                )
                .await,
            Err(RunnerProtocolStoreError::Domain(
                signalbox_domain::RunnerDomainError::InvalidState
            ))
        ));
    }
    let mut changed = ready.clone();
    changed.manifest_id = WorkspaceManifestId::from_uuid(Uuid::now_v7());
    assert!(
        store
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &changed,
                &ready_digest()
            )
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
    if case == ProvisionCase::RetiredWorkspace {
        assert!(
            store
                .workspace_releases(
                    candidate.identities().enrollment(),
                    candidate_connection.epoch()
                )
                .await?
                .is_empty(),
            "an installed current manifest cannot be released"
        );
        let prior_epoch = candidate_connection.epoch();
        candidate_connection = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        let active = store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .expect("promoted owner");
        store.register(&active, narrowed_advertisement()).await?;
        append_runner_registration_loss_projection(&pool, session).await?;
        store
            .abandon_lost_runner(signalbox_domain::AbandonLostRunner {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
            })
            .await?;
        assert!(
            store
                .workspace_releases(candidate.identities().enrollment(), prior_epoch)
                .await?
                .is_empty()
        );
        let release = release_receipt(&ready, ready.manifest_id);
        assert_eq!(
            store
                .workspace_releases(
                    candidate.identities().enrollment(),
                    candidate_connection.epoch()
                )
                .await?,
            vec![release.clone()]
        );
        assert!(
            store
                .record_workspace_release_outcome(
                    predecessor.identities().enrollment(),
                    store
                        .load_connection(predecessor.identities().enrollment())
                        .await?
                        .expect("release owner connection")
                        .epoch(),
                    &release,
                    None
                )
                .await
                .is_err()
        );
        store
            .record_workspace_release_outcome(
                candidate.identities().enrollment(),
                store
                    .load_connection(candidate.identities().enrollment())
                    .await?
                    .expect("release owner connection")
                    .epoch(),
                &release,
                None,
            )
            .await?;
    }
    if case == ProvisionCase::RegistrationChanged {
        let active = store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .expect("promoted owner");
        store.register(&active, narrowed_advertisement()).await?;
        append_runner_registration_loss_projection(&pool, session).await?;
        let restored = store.register(&active, advertisement()).await?;
        let retry = signalbox_domain::ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            revision: None,
        };
        assert_eq!(
            store.replace_lost_runner(retry).await?,
            RunnerRecoveryOutcome::Pending
        );
        let pending = store
            .replacement_provisioning(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
            )
            .await?;
        assert_eq!(pending.len(), 1);
        let updated = store
            .resume_registration(
                candidate.request(),
                candidate.identities(),
                restored.revision(),
                advertisement().with_default_working_directory(Some(
                    RunnerWorkingDirectory::try_new("/updated-workspace".to_owned())
                        .expect("changed directory"),
                )),
            )
            .await?;
        assert_ne!(
            updated.registration().revision().get(),
            pending[0].registration_revision.get()
        );
        let next = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert!(
            store
                .replacement_provisioning(candidate.identities().enrollment(), next.epoch())
                .await?
                .is_empty()
        );
        assert_eq!(
            store
                .replacement_provisioning_authorization(pending[0].authorization)
                .await?,
            Some(pending[0].clone())
        );
        assert_eq!(
            store
                .load_connection(candidate.identities().enrollment())
                .await?
                .expect("live successor")
                .state(),
            RunnerConnectionState::Connected
        );
    }
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
    provisioning_failure_fixture(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provisioning_failure_rejects_fenced_frames_and_replays() -> Result<(), Box<dyn Error>> {
    provisioning_failure_fixture(true).await
}

async fn provisioning_failure_fixture(fenced: bool) -> Result<(), Box<dyn Error>> {
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
        .expect("retained provisioning fixture authority");
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
    let candidate_connection = store
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
        .replacement_provisioning(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
        )
        .await?;
    let authorization = &operations[0];
    let detail = serde_json::json!({"code": "fixture_refusal", "message": "workspace unavailable", "payload": {}});
    let kind = signalbox_domain::RunnerProvisioningFailureKind::SandboxUnavailable;

    let old_epoch = candidate_connection.epoch();
    let candidate_connection = if fenced {
        let current = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert!(
            store
                .record_replacement_provisioning_failure(authorization, old_epoch, kind, &detail)
                .await
                .is_err()
        );
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_replacement_provisioning_failure WHERE authorization_id = $1")
            .bind(authorization.authorization.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(retained, 0);
        current
    } else {
        candidate_connection
    };
    store
        .record_replacement_provisioning_failure(
            authorization,
            candidate_connection.epoch(),
            kind,
            &detail,
        )
        .await?;
    store
        .record_replacement_provisioning_failure(
            authorization,
            candidate_connection.epoch(),
            kind,
            &detail,
        )
        .await?;

    if fenced {
        assert!(
            store
                .record_replacement_provisioning_failure(authorization, old_epoch, kind, &detail)
                .await
                .is_err()
        );
    }
    store
        .reconcile_replacement_provisioning_failure(authorization, kind, &detail)
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
                candidate_connection.epoch(),
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
            .expect("retained provisioning fixture authority")
            .state(),
        RunnerEnrollmentState::Pending
    );
    assert!(
        store
            .replacement_provisioning(
                candidate.identities().enrollment(),
                candidate_connection.epoch()
            )
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
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &ready,
                &ready_digest()
            )
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
    let candidate_connection = store
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
        .replacement_provisioning(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
        )
        .await?;
    let authorization = &operations[0];
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    store
        .record_replacement_workspace_ready(
            authorization,
            candidate_connection.epoch(),
            &ready,
            &ready_digest(),
        )
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
            .workspace_releases(
                candidate.identities().enrollment(),
                candidate_connection.epoch()
            )
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
    rejected_staging_releases_ready_workspace(false, ReleaseEpoch::Current, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_releases_a_late_correlated_workspace_receipt()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(true, ReleaseEpoch::Current, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_release_receipt_cannot_cross_the_retained_cleanup_epoch()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(false, ReleaseEpoch::Lost, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cleanup_failure_retires_release_and_preserves_exact_detail() -> Result<(), Box<dyn Error>>
{
    rejected_staging_releases_ready_workspace(false, ReleaseEpoch::Current, true).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn release_outcomes_reject_a_fenced_connection_before_storage_and_replay()
-> Result<(), Box<dyn Error>> {
    for cleanup_failed in [false, true] {
        rejected_staging_releases_ready_workspace(false, ReleaseEpoch::Fenced, cleanup_failed)
            .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retained_release_outcomes_reconcile_before_a_new_epoch_opens() -> Result<(), Box<dyn Error>>
{
    for cleanup_failed in [false, true] {
        rejected_staging_releases_ready_workspace(false, ReleaseEpoch::Retained, cleanup_failed)
            .await?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ReleaseEpoch {
    Current,
    Lost,
    HeartbeatLoss,
    Fenced,
    Retained,
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retained_release_authority_is_not_reissued_after_heartbeat_loss()
-> Result<(), Box<dyn Error>> {
    rejected_staging_releases_ready_workspace(false, ReleaseEpoch::HeartbeatLoss, false).await
}

async fn rejected_staging_releases_ready_workspace(
    late_ready: bool,
    release_epoch: ReleaseEpoch,
    cleanup_failed: bool,
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
    let candidate_connection = store
        .open_connection(candidate.identities().enrollment())
        .await?;
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::DaemonShutdown,
        )
        .await?;
    let candidate_connection = store
        .open_connection(candidate.identities().enrollment())
        .await?;
    assert!(candidate_connection.epoch().get() > 1);
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
        .replacement_provisioning(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
        )
        .await?;
    let authorization = &operations[0];
    let ready = private_workspace(
        session,
        candidate.identities().runner(),
        authorization.placement_revision,
    );
    if !late_ready {
        store
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &ready,
                &ready_digest(),
            )
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
                .record_replacement_workspace_ready(
                    authorization,
                    candidate_connection.epoch(),
                    &wrong,
                    &ready_digest()
                )
                .await
                .is_err()
        );
        store
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &ready,
                &ready_digest(),
            )
            .await?;
        store
            .record_replacement_workspace_ready(
                authorization,
                candidate_connection.epoch(),
                &ready,
                &ready_digest(),
            )
            .await?;
    }
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_release WHERE authorization_id = $1 AND connection_epoch = $2")
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
            .workspace_releases(
                candidate.identities().enrollment(),
                candidate_connection.epoch()
            )
            .await?,
        vec![release_receipt(&ready, ready.manifest_id)]
    );
    if matches!(release_epoch, ReleaseEpoch::Fenced | ReleaseEpoch::Retained) {
        let enrollment = candidate.identities().enrollment();
        let release = release_receipt(&ready, ready.manifest_id);
        let detail = serde_json::json!({"code":"cleanup_denied", "message":"retained cleanup failure", "payload":{}});
        let failure = cleanup_failed.then_some(&detail);
        let original_epoch = candidate_connection.epoch();
        if matches!(release_epoch, ReleaseEpoch::Retained) {
            store
                .reconcile_workspace_release_outcome(enrollment, &release, failure)
                .await?;
            assert_eq!(
                store
                    .load_connection(enrollment)
                    .await?
                    .expect("retained head")
                    .epoch(),
                original_epoch
            );
        }
        let successor = store.open_connection(enrollment).await?;
        assert!(
            store
                .record_workspace_release_outcome(enrollment, original_epoch, &release, failure)
                .await
                .is_err()
        );
        let recorded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_workspace_release_outcome WHERE manifest_id = $1",
        )
        .bind(ready.manifest_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            recorded,
            i64::from(matches!(release_epoch, ReleaseEpoch::Retained))
        );
        for _ in 0..2 {
            store
                .record_workspace_release_outcome(enrollment, successor.epoch(), &release, failure)
                .await?;
        }
        assert!(
            store
                .record_workspace_release_outcome(enrollment, original_epoch, &release, failure)
                .await
                .is_err()
        );
        let recorded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_workspace_release_outcome WHERE manifest_id = $1",
        )
        .bind(ready.manifest_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(recorded, 1);
        return Ok(());
    }
    if matches!(
        release_epoch,
        ReleaseEpoch::Lost | ReleaseEpoch::HeartbeatLoss
    ) {
        store
            .transition_connection(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
                if matches!(release_epoch, ReleaseEpoch::HeartbeatLoss) {
                    RunnerConnectionTransition::HeartbeatTimeout
                } else {
                    RunnerConnectionTransition::TransportClosed
                },
            )
            .await?;
        let successor = store
            .open_connection(candidate.identities().enrollment())
            .await?;
        assert_ne!(successor.epoch(), candidate_connection.epoch());
        assert!(
            store
                .workspace_releases(
                    candidate.identities().enrollment(),
                    candidate_connection.epoch()
                )
                .await?
                .is_empty()
        );
        assert!(
            store
                .record_workspace_release_outcome(
                    candidate.identities().enrollment(),
                    store
                        .load_connection(candidate.identities().enrollment())
                        .await?
                        .expect("release owner connection")
                        .epoch(),
                    &release_receipt(&ready, ready.manifest_id),
                    None
                )
                .await
                .is_err()
        );
        let released: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_release_outcome outcome JOIN runner_workspace_release release USING (manifest_id) WHERE release.authorization_id = $1 AND outcome.outcome = 'completed'").bind(authorization.authorization.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(released, 0);
        use signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceReleaseState;
        assert_eq!(
            store
                .workspace_release_state(
                    candidate.identities().enrollment(),
                    &release_receipt(&ready, ready.manifest_id)
                )
                .await?,
            Some(RunnerWorkspaceReleaseState::Unowned)
        );
        let page =
            signalbox_persistence::runner_protocol::status::read_runner_status(&pool, 100, None)
                .await?;
        assert!(
            page.leaks
                .iter()
                .any(|(runner, leak)| *runner == ready.runner
                    && leak.locator == ready.relative_path
                    && leak.entry_digest.as_str() == ready_digest().as_str())
        );
        startup_report_matches_the_retained_release_leak(
            &store,
            &pool,
            candidate.identities().enrollment(),
            &ready,
        )
        .await?;
        return Ok(());
    }
    assert!(
        store
            .record_workspace_release_outcome(
                candidate.identities().enrollment(),
                store
                    .load_connection(candidate.identities().enrollment())
                    .await?
                    .expect("release owner connection")
                    .epoch(),
                &release_receipt(&ready, WorkspaceManifestId::from_uuid(Uuid::now_v7())),
                None
            )
            .await
            .is_err()
    );
    if !cleanup_failed {
        assert_cleanable_report_has_no_leak(
            &store,
            &pool,
            candidate.identities().enrollment(),
            &ready,
            false,
        )
        .await?;
    }
    let detail = serde_json::json!({"code":"rename-denied", "message":"cannot rename /private/work", "payload":{"path":"/private/work"}});
    if cleanup_failed {
        for _ in 0..2 {
            store
                .record_workspace_release_outcome(
                    candidate.identities().enrollment(),
                    store
                        .load_connection(candidate.identities().enrollment())
                        .await?
                        .expect("release owner connection")
                        .epoch(),
                    &release_receipt(&ready, ready.manifest_id),
                    Some(&detail),
                )
                .await?;
        }
        use signalbox_persistence::runner_protocol::workspaces::{
            RunnerWorkspaceLeakKind, RunnerWorkspaceReleaseState,
        };
        assert_eq!(
            store
                .workspace_release_state(
                    candidate.identities().enrollment(),
                    &release_receipt(&ready, ready.manifest_id)
                )
                .await?,
            Some(RunnerWorkspaceReleaseState::CleanupFailed(detail))
        );
        assert!(
            store
                .record_workspace_release_outcome(
                    candidate.identities().enrollment(),
                    store
                        .load_connection(candidate.identities().enrollment())
                        .await?
                        .expect("release owner connection")
                        .epoch(),
                    &release_receipt(&ready, ready.manifest_id),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            store
                .workspace_releases(
                    candidate.identities().enrollment(),
                    candidate_connection.epoch()
                )
                .await?
                .is_empty()
        );
        let page =
            signalbox_persistence::runner_protocol::status::read_runner_status(&pool, 100, None)
                .await?;
        assert!(
            page.leaks
                .iter()
                .any(|(runner, leak)| *runner == ready.runner
                    && leak.kind == RunnerWorkspaceLeakKind::CleanupFailed
                    && leak.entry_digest.as_str() == ready_digest().as_str())
        );
        startup_report_matches_the_retained_release_leak(
            &store,
            &pool,
            candidate.identities().enrollment(),
            &ready,
        )
        .await?;
        return Ok(());
    }
    store
        .record_workspace_release_outcome(
            candidate.identities().enrollment(),
            store
                .load_connection(candidate.identities().enrollment())
                .await?
                .expect("release owner connection")
                .epoch(),
            &release_receipt(&ready, ready.manifest_id),
            None,
        )
        .await?;
    store
        .record_workspace_release_outcome(
            candidate.identities().enrollment(),
            store
                .load_connection(candidate.identities().enrollment())
                .await?
                .expect("release owner connection")
                .epoch(),
            &release_receipt(&ready, ready.manifest_id),
            None,
        )
        .await?;
    assert!(
        store
            .workspace_releases(
                candidate.identities().enrollment(),
                candidate_connection.epoch()
            )
            .await?
            .is_empty()
    );
    assert_cleanable_report_has_no_leak(
        &store,
        &pool,
        candidate.identities().enrollment(),
        &ready,
        true,
    )
    .await?;
    Ok(())
}

fn ready_digest() -> signalbox_persistence::runner_protocol::workspaces::RunnerEvidenceDigest {
    // Arbitrary canonical identity for typed persistence receipt fixtures.
    signalbox_persistence::runner_protocol::workspaces::RunnerEvidenceDigest::try_new(
        "d".repeat(64),
    )
    .expect("fixture digest")
}

fn release_receipt(
    workspace: &ProvisionedWorkspace,
    manifest: WorkspaceManifestId,
) -> signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceRelease {
    signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceRelease {
        session: workspace.session,
        placement_revision: workspace.placement_revision,
        runner: workspace.runner,
        manifest,
    }
}

async fn startup_report_matches_the_retained_release_leak(
    store: &RunnerProtocolStore,
    pool: &PgPool,
    enrollment: signalbox_domain::RunnerEnrollmentId,
    ready: &ProvisionedWorkspace,
) -> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{
        CanonicalUuid, Digest, LeakFact, LeakFactKind, PositiveU64, leak_report_digest,
    };
    let stored: String = sqlx::query_scalar(
        "SELECT entry_digest FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
    )
    .bind(ready.runner.into_uuid())
    .bind(ready.relative_path.as_str())
    .fetch_one(pool)
    .await?;
    assert_eq!(
        stored,
        ready_digest().as_str(),
        "release diagnostics retain the WorkspaceReady digest"
    );
    let facts = [LeakFact {
        kind: LeakFactKind::Unreconciled,
        locator: ready.relative_path.as_str().to_owned(),
        entry_digest: Digest::try_new(ready_digest().as_str().to_owned())?,
        session: Some(CanonicalUuid::from_uuid(ready.session.into_uuid())),
        placement_revision: Some(PositiveU64::try_new(ready.placement_revision.get())?),
    }];
    let report = leak_report_digest(&facts)?;
    let page = super::status::report_page(&report, 1, None, true, &facts);
    store
        .record_workspace_leak_page(
            enrollment,
            store
                .load_connection(enrollment)
                .await?
                .expect("retained fixture connection")
                .epoch(),
            &page,
        )
        .await?;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
    )
    .bind(ready.runner.into_uuid())
    .bind(ready.relative_path.as_str())
    .fetch_one(pool)
    .await?;
    assert_eq!(
        rows, 1,
        "startup reconciles the existing release leak instead of duplicating it"
    );
    let trash = [LeakFact {
        kind: LeakFactKind::RetiredPresent,
        locator: format!("trash/{}", ready.manifest_id.into_uuid()),
        // Arbitrary observed metadata digest after the ready manifest was deleted.
        entry_digest: Digest::try_new("e".repeat(64))?,
        session: None,
        placement_revision: None,
    }];
    let report = leak_report_digest(&trash)?;
    store
        .record_workspace_leak_page(
            enrollment,
            store
                .load_connection(enrollment)
                .await?
                .expect("retained fixture connection")
                .epoch(),
            &super::status::report_page(&report, 1, None, true, &trash),
        )
        .await?;
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(ready.runner.into_uuid())
            .fetch_one(pool)
            .await?;
    assert_eq!(
        rows, 1,
        "manifest-free trash reconciles the retained release diagnostic"
    );
    let empty_digest = leak_report_digest(&[])?;
    let empty = super::status::report_page(&empty_digest, 1, None, true, &[]);
    store
        .record_workspace_leak_page(
            enrollment,
            store
                .load_connection(enrollment)
                .await?
                .expect("current owner")
                .epoch(),
            &empty,
        )
        .await?;
    let retained: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(ready.runner.into_uuid())
            .fetch_one(pool)
            .await?;
    assert_eq!(
        retained, 1,
        "independent release outcome survives an empty report"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lost_offered_lease_retains_its_manifest_as_a_leak() -> Result<(), Box<dyn Error>> {
    lost_live_lease_retains_manifest(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lost_claimed_lease_retains_its_manifest_as_a_leak() -> Result<(), Box<dyn Error>> {
    lost_live_lease_retains_manifest(true).await
}

async fn lost_live_lease_retains_manifest(claimed: bool) -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::status::read_runner_status;
    use signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceLeakKind;
    let (_container, pool) = migrated_postgres().await?;
    let workspace = private_workspace(
        SessionId::from_uuid(uuid(SESSION)),
        enrollment().runner(),
        RunnerGeneration::try_from_u64(1).expect("first placement"),
    );
    let (store, enrolled, _, pin, epoch) = stored_active_pin_fixture_with_workspace(
        &pool,
        ActivePinEffectCase::EffectFree,
        Some(workspace.clone()),
    )
    .await?;
    if claimed {
        store
            .claim_tool_lease(enrolled.enrollment(), epoch, pin.lease.correlation())
            .await?;
    }
    store
        .transition_connection(
            enrolled.enrollment(),
            epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(enrolled.enrollment())
        .await?
        .expect("durable loss");
    store
        .propagate_connection_loss_session(loss, pin.placement.session())
        .await?;
    store
        .propagate_connection_loss_session(loss, pin.placement.session())
        .await?;
    assert!(
        store
            .workspace_releases(enrolled.enrollment(), epoch)
            .await?
            .is_empty(),
        "disconnected owners receive no release authority"
    );
    let page = read_runner_status(&pool, 100, None).await?;
    let digest = signalbox_runner_wire::workspace_manifest_digest(
        &signalbox_runner_wire::WorkspaceManifest::from_domain(
            signalbox_runner_wire::ManifestLifecycle::Ready,
            &workspace,
        )?,
    )?;
    assert_eq!(
        page.leaks.len(),
        1,
        "loss retains the orphaned workspace exactly once"
    );
    let (runner, leak) = &page.leaks[0];
    assert_eq!(*runner, workspace.runner);
    assert_eq!(leak.kind, RunnerWorkspaceLeakKind::RetiredPresent);
    assert_eq!(leak.locator, workspace.relative_path);
    assert_eq!(leak.entry_digest.as_str(), digest.as_str());
    assert_eq!(leak.session, Some(workspace.session));
    assert_eq!(leak.placement_revision, Some(workspace.placement_revision));
    Ok(())
}

async fn assert_cleanable_report_has_no_leak(
    store: &RunnerProtocolStore,
    pool: &PgPool,
    enrollment: signalbox_domain::RunnerEnrollmentId,
    ready: &ProvisionedWorkspace,
    completed: bool,
) -> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{
        CanonicalUuid, Digest, LeakFact, LeakFactKind, PositiveU64, leak_report_digest,
    };
    let mut facts = vec![LeakFact {
        kind: LeakFactKind::Unreconciled,
        locator: ready.relative_path.as_str().to_owned(),
        entry_digest: Digest::try_new(ready_digest().as_str().to_owned())?,
        session: Some(CanonicalUuid::from_uuid(ready.session.into_uuid())),
        placement_revision: Some(PositiveU64::try_new(ready.placement_revision.get())?),
    }];
    if completed {
        // Successful cleanup removed the original trash directory, so a later entry
        // using that manifest name is new evidence rather than retained cleanup.
        facts.push(LeakFact {
            kind: LeakFactKind::RetiredPresent,
            locator: format!("trash/{}", ready.manifest_id.into_uuid()),
            entry_digest: Digest::try_new("e".repeat(64))?,
            session: None,
            placement_revision: None,
        });
    }
    facts.sort();
    let report = leak_report_digest(&facts)?;
    store
        .record_workspace_leak_page(
            enrollment,
            store
                .load_connection(enrollment)
                .await?
                .expect("retained fixture connection")
                .epoch(),
            &super::status::report_page(&report, 1, None, true, &facts),
        )
        .await?;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
    )
    .bind(ready.runner.into_uuid())
    .bind(ready.relative_path.as_str())
    .fetch_one(pool)
    .await?;
    assert_eq!(
        rows, 0,
        "pending and completed releases are reconciled by startup reporting"
    );
    if completed {
        let trash_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
        )
        .bind(ready.runner.into_uuid())
        .bind(format!("trash/{}", ready.manifest_id.into_uuid()))
        .fetch_one(pool)
        .await?;
        assert_eq!(
            trash_rows, 1,
            "trash recreated after completed cleanup remains visible"
        );
    }
    Ok(())
}
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_report_reconciles_current_initial_workspace() -> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{
        CanonicalUuid, Digest, LeakFact, LeakFactKind, ManifestLifecycle, PositiveU64,
        WorkspaceManifest, leak_report_digest, workspace_manifest_digest,
    };
    let (_container, pool) = migrated_postgres().await?;
    let workspace = private_workspace(
        SessionId::from_uuid(uuid(SESSION)),
        enrollment().runner(),
        RunnerGeneration::try_from_u64(1).expect("first placement"),
    );
    let (store, enrolled, _, _, _) = stored_active_pin_fixture_with_workspace(
        &pool,
        ActivePinEffectCase::EffectFree,
        Some(workspace.clone()),
    )
    .await?;
    let ready_digest = workspace_manifest_digest(&WorkspaceManifest::from_domain(
        ManifestLifecycle::Ready,
        &workspace,
    )?)?;
    let mut fact = LeakFact {
        kind: LeakFactKind::Unreconciled,
        locator: workspace.relative_path.as_str().to_owned(),
        entry_digest: ready_digest,
        session: Some(CanonicalUuid::from_uuid(workspace.session.into_uuid())),
        placement_revision: Some(PositiveU64::try_new(workspace.placement_revision.get())?),
    };
    let report = leak_report_digest(std::slice::from_ref(&fact))?;
    store
        .record_workspace_leak_page(
            enrolled.enrollment(),
            store
                .load_connection(enrolled.enrollment())
                .await?
                .expect("retained fixture connection")
                .epoch(),
            &super::status::report_page(&report, 1, None, true, std::slice::from_ref(&fact)),
        )
        .await?;
    let leaks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(workspace.runner.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        leaks, 0,
        "the current initial workspace is authoritative without a replacement receipt"
    );

    fact.entry_digest = Digest::try_new("e".repeat(64))?;
    let report = leak_report_digest(std::slice::from_ref(&fact))?;
    store
        .record_workspace_leak_page(
            enrolled.enrollment(),
            store
                .load_connection(enrolled.enrollment())
                .await?
                .expect("retained fixture connection")
                .epoch(),
            &super::status::report_page(&report, 1, None, true, std::slice::from_ref(&fact)),
        )
        .await?;
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(workspace.runner.into_uuid())
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        kinds,
        ["manifest_conflict"],
        "matching placement identity does not authenticate a changed digest"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_report_preserves_retired_initial_workspace_release_outcomes()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceReleaseState as ReleaseState;
    use signalbox_runner_wire::{
        CanonicalUuid, LeakFact, LeakFactKind, ManifestLifecycle, PositiveU64, WorkspaceManifest,
        leak_report_digest, workspace_manifest_digest,
    };
    for outcome in [
        ReleaseState::CleanupFailed(
            serde_json::json!({"code":"cleanup_denied","message":"cannot delete","payload":{}}),
        ),
        ReleaseState::Unowned,
        ReleaseState::Pending,
        ReleaseState::Completed,
    ] {
        let (_container, pool) = migrated_postgres().await?;
        let workspace = private_workspace(
            SessionId::from_uuid(uuid(SESSION)),
            enrollment().runner(),
            RunnerGeneration::try_from_u64(1).expect("first placement"),
        );
        insert_session(&pool).await?;
        insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
        let store = RunnerProtocolStore::new(pool.clone(), catalog());
        let enrolled = enrollment();
        store.insert_enrollment(&enrolled).await?;
        let registration = store.register(&enrolled, advertisement()).await?;
        let placement = SessionRunnerPlacement::new(
            workspace.session,
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::Identity(workspace.runner),
                working_directory: WorkingDirectorySelection::RunnerDefault,
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: workspace.sandbox,
                permission_overrides: no_permission_overrides(),
            },
        );
        store.store_placement(&placement, None, None).await?;
        let pin = placement
            .pin_and_offer_lease(
                &enrolled,
                registration.registration(),
                workspace.working_directory.clone(),
                Some(workspace.clone()),
                authorized(INITIAL_PHYSICAL_ATTEMPT),
                offer_request(),
            )
            .expect("initial workspace authorization");
        let epoch = store.open_connection(enrolled.enrollment()).await?.epoch();
        store.store_pin(&pin, &registration).await?;
        let correlation = pin.lease.correlation();
        let claimed = pin.lease.claim(correlation.clone()).expect("exact offer");
        store.store_lease(&claimed).await?;
        store
            .store_lease(&claimed.complete(correlation).expect("exact claim"))
            .await?;
        let registration = store.register(&enrolled, narrowed_advertisement()).await?;
        append_runner_registration_loss_projection(&pool, workspace.session).await?;
        assert_eq!(
            store
                .abandon_lost_runner(signalbox_domain::AbandonLostRunner {
                    command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                    session: workspace.session,
                })
                .await?,
            RunnerRecoveryOutcome::Recorded(signalbox_domain::AbandonLostRunnerResult::Abandoned)
        );
        let release = release_receipt(&workspace, workspace.manifest_id);
        assert_eq!(
            store
                .workspace_releases(enrolled.enrollment(), epoch)
                .await?,
            std::slice::from_ref(&release)
        );
        match &outcome {
            ReleaseState::CleanupFailed(detail) => {
                store
                    .record_workspace_release_outcome(
                        enrolled.enrollment(),
                        store
                            .load_connection(enrolled.enrollment())
                            .await?
                            .expect("release owner connection")
                            .epoch(),
                        &release,
                        Some(detail),
                    )
                    .await?
            }
            ReleaseState::Unowned => {
                store
                    .transition_connection(
                        enrolled.enrollment(),
                        epoch,
                        RunnerConnectionTransition::TransportClosed,
                    )
                    .await?;
            }
            ReleaseState::Completed => {
                store
                    .record_workspace_release_outcome(
                        enrolled.enrollment(),
                        store
                            .load_connection(enrolled.enrollment())
                            .await?
                            .expect("release owner connection")
                            .epoch(),
                        &release,
                        None,
                    )
                    .await?
            }
            ReleaseState::Pending => {}
        }
        if let ReleaseState::CleanupFailed(detail) = &outcome {
            use signalbox_persistence::runner_protocol::status::{
                RunnerStatusFailure, read_runner_status,
            };
            let mut after = None;
            let mut failures = Vec::new();
            loop {
                let page = read_runner_status(&pool, 1, after).await?;
                failures.extend(page.failures);
                match page.next_after {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            assert_eq!(
                failures,
                [RunnerStatusFailure::Release {
                    correlation: release.clone(),
                    detail: detail.clone()
                }]
            );
        }

        if matches!(outcome, ReleaseState::Unowned) {
            store.open_connection(enrolled.enrollment()).await?;
        }
        let facts = [LeakFact {
            kind: LeakFactKind::Unreconciled,
            locator: workspace.relative_path.as_str().to_owned(),
            entry_digest: workspace_manifest_digest(&WorkspaceManifest::from_domain(
                ManifestLifecycle::Ready,
                &workspace,
            )?)?,
            session: Some(CanonicalUuid::from_uuid(workspace.session.into_uuid())),
            placement_revision: Some(PositiveU64::try_new(workspace.placement_revision.get())?),
        }];
        let report = leak_report_digest(&facts)?;
        let mut page = super::status::report_page(&report, 1, None, true, &facts);
        page.registration_revision = RunnerGeneration::try_from_u64(registration.revision().get())
            .expect("nonzero registration");
        store
            .record_workspace_leak_page(
                enrolled.enrollment(),
                store
                    .load_connection(enrolled.enrollment())
                    .await?
                    .expect("retained fixture connection")
                    .epoch(),
                &page,
            )
            .await?;
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
        )
        .bind(workspace.runner.into_uuid())
        .bind(workspace.relative_path.as_str())
        .fetch_all(&pool)
        .await?;
        let expected = match outcome {
            ReleaseState::CleanupFailed(_) => vec!["cleanup_failed"],
            ReleaseState::Unowned => vec!["retired_present"],
            _ => vec![],
        };
        assert_eq!(
            kinds, expected,
            "startup must preserve the {outcome:?} release disposition"
        );
        let conflict = [LeakFact {
            kind: LeakFactKind::ManifestConflict,
            locator: format!("trash/{}", workspace.manifest_id.into_uuid()),
            ..facts[0].clone()
        }];
        let report = leak_report_digest(&conflict)?;
        let mut page = super::status::report_page(&report, 1, None, true, &conflict);
        page.registration_revision = RunnerGeneration::try_from_u64(registration.revision().get())
            .expect("nonzero registration");
        store
            .record_workspace_leak_page(
                enrolled.enrollment(),
                store
                    .load_connection(enrolled.enrollment())
                    .await?
                    .expect("retained fixture connection")
                    .epoch(),
                &page,
            )
            .await?;
        let kind: String = sqlx::query_scalar(
            "SELECT kind FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2",
        )
        .bind(workspace.runner.into_uuid())
        .bind(&conflict[0].locator)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            kind, "manifest_conflict",
            "a retained release cannot suppress a readable conflicting trash manifest"
        );
        let unreadable = [LeakFact {
            kind: LeakFactKind::RetiredPresent,
            locator: format!("trash/{}", workspace.manifest_id.into_uuid()),
            // Arbitrary directory metadata after the manifest is absent.
            entry_digest: signalbox_runner_wire::Digest::try_new("e".repeat(64))?,
            session: None,
            placement_revision: None,
        }];
        let report = leak_report_digest(&unreadable)?;
        let mut page = super::status::report_page(&report, 1, None, true, &unreadable);
        page.registration_revision = RunnerGeneration::try_from_u64(registration.revision().get())
            .expect("nonzero registration");
        store
            .record_workspace_leak_page(
                enrolled.enrollment(),
                store
                    .load_connection(enrolled.enrollment())
                    .await?
                    .expect("retained fixture connection")
                    .epoch(),
                &page,
            )
            .await?;
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM runner_workspace_leak WHERE runner_id = $1 AND locator = $2 AND entry_digest = $3",
        ).bind(workspace.runner.into_uuid()).bind(&unreadable[0].locator)
            .bind(unreadable[0].entry_digest.as_str()).fetch_all(&pool).await?;
        let expected = match outcome {
            ReleaseState::Completed => vec!["retired_present"],
            ReleaseState::CleanupFailed(_) | ReleaseState::Unowned | ReleaseState::Pending => {
                vec![]
            }
        };
        assert_eq!(
            kinds, expected,
            "only a pending or failed release explains manifest-free trash; {outcome:?}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_diagnostics_preserve_abandoned_lost_initial_workspace()
-> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{
        CanonicalUuid, LeakFact, LeakFactKind, ManifestLifecycle, PositiveU64, WorkspaceManifest,
        leak_report_digest, workspace_manifest_digest,
    };
    let (_container, pool) = migrated_postgres().await?;
    let workspace = private_workspace(
        SessionId::from_uuid(uuid(SESSION)),
        enrollment().runner(),
        RunnerGeneration::try_from_u64(1).expect("initial placement"),
    );
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let enrolled = enrollment();
    store.insert_enrollment(&enrolled).await?;
    let registration = store.register(&enrolled, advertisement()).await?;
    let placement = SessionRunnerPlacement::new(
        workspace.session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(workspace.runner),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: workspace.sandbox,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &enrolled,
            registration.registration(),
            workspace.working_directory.clone(),
            Some(workspace.clone()),
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("initial workspace authorization");
    let epoch = store.open_connection(enrolled.enrollment()).await?.epoch();
    store.store_pin(&pin, &registration).await?;
    let correlation = pin.lease.correlation();
    let claimed = pin.lease.claim(correlation.clone()).expect("exact offer");
    store.store_lease(&claimed).await?;
    store
        .store_lease(&claimed.complete(correlation).expect("exact claim"))
        .await?;
    store
        .transition_connection(
            enrolled.enrollment(),
            epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(enrolled.enrollment())
        .await?
        .expect("retained connection loss");
    store
        .propagate_connection_loss_session(loss, pin.placement.session())
        .await?;
    assert_eq!(
        store
            .abandon_lost_runner(signalbox_domain::AbandonLostRunner {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                session: workspace.session,
            })
            .await?,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::AbandonLostRunnerResult::Abandoned)
    );
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_release")
        .fetch_one(&pool)
        .await?;
    assert_eq!(releases, 0);
    let before: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(workspace.runner.into_uuid())
            .fetch_all(&pool)
            .await?;
    assert_eq!(before, ["retired_present"]);
    let epoch = store.open_connection(enrolled.enrollment()).await?.epoch();
    let facts = [LeakFact {
        kind: LeakFactKind::Unreconciled,
        locator: workspace.relative_path.as_str().to_owned(),
        entry_digest: workspace_manifest_digest(&WorkspaceManifest::from_domain(
            ManifestLifecycle::Ready,
            &workspace,
        )?)?,
        session: Some(CanonicalUuid::from_uuid(workspace.session.into_uuid())),
        placement_revision: Some(PositiveU64::try_new(workspace.placement_revision.get())?),
    }];
    let report = leak_report_digest(&facts)?;
    let page = super::status::report_page(&report, 1, None, true, &facts);
    store
        .record_workspace_leak_page(enrolled.enrollment(), epoch, &page)
        .await?;
    let after: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM runner_workspace_leak WHERE runner_id = $1")
            .bind(workspace.runner.into_uuid())
            .fetch_all(&pool)
            .await?;
    assert_eq!(after, ["retired_present"]);
    Ok(())
}
