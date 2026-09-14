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
    let candidate_connection = store
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
            .replacement_provisioning(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
            )
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
        let page = read_runner_status(&pool, 1, after.clone()).await?;
        assert_eq!(page.runners.len() + page.failures.len(), 1);
        if let Some(next) = &page.next_after {
            assert_ne!(Some(next), after.as_ref());
            cursors.push(next.clone());
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
        let signalbox_persistence::runner_protocol::status::RunnerStatusFailure::Provision {
            authorization: retained,
            detail: retained_detail,
            ..
        } = failure
        else {
            panic!("provisioning failure");
        };
        assert_eq!(retained, authorization);
        assert_eq!(*retained_detail, detail);
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
    let beyond = read_runner_status(
        &pool,
        100,
        Some(RunnerStatusAfter::WorkspaceLeak {
            runner: Uuid::max(),
            locator: "sessions".to_owned(),
            entry_digest:
                signalbox_persistence::runner_protocol::workspaces::RunnerEvidenceDigest::try_new(
                    "f".repeat(64),
                )
                .expect("canonical digest"),
        }),
    )
    .await?;
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

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_leak_pages_are_durable_exact_and_visible_without_a_session()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceLeakKind;
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let receipt = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(receipt.identities().enrollment())
        .await?;
    let facts: Vec<_> = ["sessions/orphan-a", "sessions/orphan-b"]
        .into_iter()
        .map(|locator| {
            let mut fact = report_fact(0);
            fact.locator = locator.to_owned();
            fact
        })
        .collect();
    let report = signalbox_runner_wire::leak_report_digest(&facts)?;
    let mut page = report_page(&report, 1, None, true, &facts);
    store
        .record_workspace_leak_page(receipt.identities().enrollment(), &page)
        .await?;
    store
        .record_workspace_leak_page(receipt.identities().enrollment(), &page)
        .await?;
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak_page")
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 1);
    page.facts[0].kind = RunnerWorkspaceLeakKind::CleanupFailed;
    assert!(
        store
            .record_workspace_leak_page(receipt.identities().enrollment(), &page)
            .await
            .is_err()
    );
    let first = read_runner_status(&pool, 2, None).await?;
    assert_eq!(first.runners.len(), 1);
    assert_eq!(first.leaks.len(), 1);
    assert_eq!(
        first.leaks[0].1.kind,
        RunnerWorkspaceLeakKind::UnknownManifest
    );
    let next = read_runner_status(&pool, 2, first.next_after).await?;
    assert!(next.runners.is_empty());
    assert!(next.failures.is_empty());
    assert_eq!(next.leaks.len(), 1);
    assert_eq!(next.leaks[0].1.locator.as_str(), "sessions/orphan-b");
    assert!(next.next_after.is_none());
    assert!(next.leaks[0].1.session.is_none());
    page.page = page.page.checked_next().expect("next page");
    page.prior_page_digest = Some(page.page_digest.clone());
    assert!(
        store
            .record_workspace_leak_page(receipt.identities().enrollment(), &page)
            .await
            .is_err(),
        "a final page cannot be extended"
    );
    Ok(())
}

fn report_fact(index: usize) -> signalbox_runner_wire::LeakFact {
    signalbox_runner_wire::LeakFact {
        kind: signalbox_runner_wire::LeakFactKind::UnknownManifest,
        locator: format!("sessions/orphan-{index:03}"),
        entry_digest: signalbox_runner_wire::Digest::try_new("c".repeat(64))
            .expect("arbitrary canonical entry identity"),
        session: None,
        placement_revision: None,
    }
}

pub(super) fn report_page(
    report: &signalbox_runner_wire::Digest,
    page_number: u64,
    prior: Option<&signalbox_runner_wire::Digest>,
    final_page: bool,
    facts: &[signalbox_runner_wire::LeakFact],
) -> signalbox_persistence::runner_protocol::workspaces::RunnerWorkspaceLeakPage {
    use signalbox_persistence::runner_protocol::workspaces::{
        RunnerEvidenceDigest, RunnerWorkspaceLeak, RunnerWorkspaceLeakKind, RunnerWorkspaceLeakPage,
    };
    use signalbox_runner_wire::{
        LeakFactKind as Kind, LeakPageDigestInput, PositiveU64, leak_page_digest,
    };
    let page_digest = leak_page_digest(LeakPageDigestInput {
        registration_revision: PositiveU64::try_new(1).expect("first registration"),
        report_digest: report,
        page: PositiveU64::try_new(page_number).expect("positive page"),
        prior_page_digest: prior,
        final_page,
        facts,
    })
    .expect("individually valid page, including its claimed report digest");
    let digest = |value: &signalbox_runner_wire::Digest| {
        RunnerEvidenceDigest::try_new(value.as_str().to_owned()).expect("canonical wire digest")
    };
    RunnerWorkspaceLeakPage {
        registration_revision: RunnerGeneration::try_from_u64(1).expect("first registration"),
        report_digest: digest(report),
        page: RunnerGeneration::try_from_u64(page_number).expect("positive page"),
        prior_page_digest: prior.map(digest),
        final_page,
        page_digest: digest(&page_digest),
        facts: facts
            .iter()
            .map(|fact| RunnerWorkspaceLeak {
                kind: match fact.kind {
                    Kind::UnknownManifest => RunnerWorkspaceLeakKind::UnknownManifest,
                    Kind::RetiredPresent => RunnerWorkspaceLeakKind::RetiredPresent,
                    Kind::ManifestConflict => RunnerWorkspaceLeakKind::ManifestConflict,
                    Kind::CleanupFailed => RunnerWorkspaceLeakKind::CleanupFailed,
                    Kind::Unreconciled => RunnerWorkspaceLeakKind::Unreconciled,
                },
                locator: WorkspaceRelativePath::try_new(fact.locator.clone())
                    .expect("relative locator"),
                entry_digest: digest(&fact.entry_digest),
                session: fact.session.map(|id| SessionId::from_uuid(id.into_uuid())),
                placement_revision: fact.placement_revision.map(|revision| {
                    RunnerGeneration::try_from_u64(revision.get()).expect("positive revision")
                }),
            })
            .collect(),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn final_leak_page_rejects_a_false_complete_report_digest() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let receipt = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let enrollment = receipt.identities().enrollment();
    store.open_connection(enrollment).await?;
    let facts = vec![report_fact(0)];
    let claimed = signalbox_runner_wire::leak_report_digest(&[])?;
    let page = report_page(&claimed, 1, None, true, &facts);
    assert!(
        store
            .record_workspace_leak_page(enrollment, &page)
            .await
            .is_err()
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak_page")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        rows, 0,
        "the rejected final page has no durable acknowledgement"
    );
    let projected: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak")
        .fetch_one(&pool)
        .await?;
    assert_eq!(projected, 0, "the failed transaction projects no facts");
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn final_leak_page_rejects_reversed_and_duplicate_page_boundaries()
-> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{Digest, leak_report_digest};
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let receipt = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let enrollment = receipt.identities().enrollment();
    store.open_connection(enrollment).await?;
    for duplicate in [false, true] {
        let first: Vec<_> = (1..=64).map(report_fact).collect();
        let last = if duplicate {
            first.last().expect("full first page").clone()
        } else {
            report_fact(0)
        };
        let mut canonical = first.clone();
        canonical.push(last.clone());
        canonical.sort();
        canonical.dedup();
        let report = leak_report_digest(&canonical)?;
        let page_one = report_page(&report, 1, None, false, &first);
        store
            .record_workspace_leak_page(enrollment, &page_one)
            .await?;
        let prior = Digest::try_new(page_one.page_digest.as_str().to_owned())?;
        let page_two = report_page(&report, 2, Some(&prior), true, &[last]);
        assert!(
            store
                .record_workspace_leak_page(enrollment, &page_two)
                .await
                .is_err()
        );
        let pages: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_workspace_leak_page WHERE report_digest = $1",
        )
        .bind(report.as_str())
        .fetch_one(&pool)
        .await?;
        assert_eq!(pages, 1, "only the first page remains retained");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn final_leak_page_acknowledges_the_exact_assembled_report() -> Result<(), Box<dyn Error>> {
    use signalbox_runner_wire::{Digest, leak_report_digest};
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let receipt = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let enrollment = receipt.identities().enrollment();
    store.open_connection(enrollment).await?;
    let facts: Vec<_> = (0..65).map(report_fact).collect();
    let report = leak_report_digest(&facts)?;
    let first = report_page(&report, 1, None, false, &facts[..64]);
    store.record_workspace_leak_page(enrollment, &first).await?;
    let prior = Digest::try_new(first.page_digest.as_str().to_owned())?;
    let last = report_page(&report, 2, Some(&prior), true, &facts[64..]);
    store.record_workspace_leak_page(enrollment, &last).await?;
    store.record_workspace_leak_page(enrollment, &last).await?;
    let pages: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak_page")
        .fetch_one(&pool)
        .await?;
    assert_eq!(pages, 2);
    let projected: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_workspace_leak")
        .fetch_one(&pool)
        .await?;
    assert_eq!(projected, 65);
    Ok(())
}
