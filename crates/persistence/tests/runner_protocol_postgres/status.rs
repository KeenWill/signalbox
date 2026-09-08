//! Exclusive status paging over durable runner facts.
use super::recovery_commands::enrollment_request;
use super::*;
use signalbox_persistence::runner_protocol::RunnerRecoveryOutcome;
use signalbox_persistence::runner_protocol::status::{
    RunnerStatusAfter, RunnerStatusFact, read_runner_status,
};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn runner_status_pages_retained_failures_without_repeating_current_facts()
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

    let detail = serde_json::json!({"code":"fixture_refusal", "message":"cannot open /private/host/work", "payload":{"credential_path":"/private/credentials/token", "attempts":2}});
    let mut authorizations = Vec::new();
    for _ in 0..3 {
        let command = signalbox_domain::ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            revision: None,
        };
        assert_eq!(
            store.replace_lost_runner(command).await?,
            RunnerRecoveryOutcome::Pending
        );
        let operations = store
            .replacement_provisioning(candidate.identities().enrollment())
            .await?;
        let authorization = operations
            .into_iter()
            .next()
            .expect("one authorized provision");
        store
            .record_replacement_provisioning_failure(
                &authorization,
                signalbox_domain::RunnerProvisioningFailureKind::SandboxUnavailable,
                &detail,
            )
            .await?;
        authorizations.push(authorization);
    }
    let mut after = None;
    let mut runners = Vec::new();
    let mut failures = Vec::new();
    let mut cursors = Vec::new();
    loop {
        let page = read_runner_status(&pool, 1, after).await?;
        assert_eq!(page.runners.len() + page.failures.len(), 1);
        if let Some(next) = page.next_after {
            assert_ne!(Some(next), after);
            cursors.push(next);
        }
        runners.extend(page.runners);
        failures.extend(page.failures);
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(runners.len(), 3);
    assert_eq!(failures.len(), 3);
    assert!(matches!(
        cursors.as_slice(),
        [
            RunnerStatusAfter::Enrollment(_),
            RunnerStatusAfter::Enrollment(_),
            RunnerStatusAfter::Placement(_),
            RunnerStatusAfter::OperationFailure(_),
            RunnerStatusAfter::OperationFailure(_)
        ]
    ));
    assert!(runners.contains(&RunnerStatusFact::Enrollment {
        runner: candidate.identities().runner(),
        request: candidate.request(),
        authority: signalbox_domain::RunnerEnrollmentState::Pending,
        connection: Some(RunnerConnectionState::Connected),
    }));
    assert!(
        matches!(runners.last(), Some(RunnerStatusFact::Placement { session: observed, runner }) if *observed == session && runner.state() == ProcessRunnerProjectionState::RunnerLost)
    );
    for (failure, authorization) in failures.iter().zip(&authorizations) {
        assert_eq!(&failure.authorization, authorization);
        assert_eq!(failure.detail, detail);
    }
    let mixed = read_runner_status(&pool, 4, None).await?;
    assert_eq!(mixed.runners, runners);
    assert_eq!(mixed.failures, failures[..1]);
    assert_eq!(
        mixed.next_after,
        Some(RunnerStatusAfter::OperationFailure(
            authorizations[0].authorization.into_uuid()
        ))
    );
    let all = read_runner_status(&pool, 100, None).await?;
    assert_eq!(all.runners, runners);
    assert_eq!(all.failures, failures);
    assert!(all.next_after.is_none());
    let beyond = read_runner_status(&pool, 100, Some(RunnerStatusAfter::WorkspaceLeak)).await?;
    assert!(beyond.runners.is_empty());
    assert!(beyond.failures.is_empty());
    assert!(beyond.next_after.is_none());
    Ok(())
}

#[tokio::test]
async fn runner_status_invalid_size_rejects_without_acquiring_a_connection()
-> Result<(), Box<dyn Error>> {
    let pool = PgPoolOptions::new().connect_lazy("postgres://localhost/status-test")?;
    for page_size in [0, 101] {
        assert!(matches!(
            read_runner_status(&pool, page_size, None).await,
            Err(signalbox_persistence::runner_protocol::status::RunnerStatusError::InvalidPageSize)
        ));
    }
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
