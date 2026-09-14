//! Filesystem publication, identity checks, and restart re-adoption.
use super::*;
use crate::RunnerStateRoot;
use signalbox_runner_wire::clone_url_digest;
use std::fs;
use tempfile::TempDir;
// Arbitrary identities distinguish the owner, foreign runner, and placement.
const SESSION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e1;
const RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e2;
const OTHER_RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e3;
const PLACEMENT_REVISION: u64 = 3;
const CLONE_URL: &str = "https://github.com/KeenWill/signalbox.git";
const PREPARED_REPOSITORY_BYTES: &[u8] = b"repository\n";
fn fixture_root() -> (TempDir, RunnerStateRoot) {
    let parent = tempfile::tempdir().expect("temporary parent");
    let state = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("locked root");
    (parent, state)
}
fn repository_request() -> RepositoryWorkspaceRequest {
    RepositoryWorkspaceRequest::new(
        CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
        PositiveU64::try_new(PLACEMENT_REVISION)
            .expect("the fixture placement revision is positive"),
        CanonicalUuid::from_uuid(Uuid::from_u128(RUNNER)),
        RepositoryKey::try_new("signalbox".to_owned())
            .expect("the fixture repository key is valid"),
        clone_url_digest(CLONE_URL),
        Some(
            ProfileName::try_new("github-runner".to_owned())
                .expect("the fixture profile name is valid"),
        ),
        SandboxProfile::WorkspaceRestricted,
    )
}

async fn publish_repository(
    state: &RunnerStateRoot,
    recovery: Recovery,
) -> super::PreparedWorkspace {
    publish_repository_request(state, &repository_request(), recovery).await
}

async fn publish_repository_request(
    state: &RunnerStateRoot,
    request: &RepositoryWorkspaceRequest,
    recovery: Recovery,
) -> super::PreparedWorkspace {
    state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_repository_workspace(request, |target| async move {
            fs::write(target.path().join("prepared"), PREPARED_REPOSITORY_BYTES)?;
            Ok::<Recovery, std::io::Error>(recovery)
        })
        .await
        .expect("the repository workspace publishes")
}

#[tokio::test]
async fn repository_workspace_publishes_exact_branch_ready_facts() {
    let (parent, state) = fixture_root();
    let expected = repository_request();
    let expected_recovery = Recovery::Branch {
        name: "main".to_owned(),
        revision: "a".repeat(40),
    };
    let prepared = publish_repository(&state, expected_recovery.clone()).await;
    let placement = parent
        .path()
        .join("runner-state")
        .join("sessions")
        .join(expected.session().to_string())
        .join(expected.placement_revision().get().to_string());
    let expected_execution_directory = placement
        .join("repo")
        .canonicalize()
        .expect("the published repository directory has a canonical path");
    let manifest_mode = fs::metadata(placement.join(MANIFEST_FILE))
        .expect("the protected manifest is inspectable")
        .permissions()
        .mode()
        & 0o7777;

    assert_eq!(prepared.manifest.session, expected.session());
    assert_eq!(prepared.manifest.runner, expected.runner());
    assert_eq!(prepared.manifest.lifecycle, ManifestLifecycle::Ready);
    assert_eq!(prepared.manifest.relative_path, expected.relative_path());
    assert_eq!(
        prepared.execution_directory.as_str(),
        expected_execution_directory
            .to_str()
            .expect("the fixture path is UTF-8")
    );
    assert_eq!(
        prepared.manifest_digest,
        workspace_manifest_digest(&prepared.manifest)
            .expect("the ready repository manifest has its canonical digest")
    );
    assert_eq!(
        prepared.manifest.repository.as_ref(),
        Some(expected.repository())
    );
    assert_eq!(
        prepared.manifest.canonical_clone_url_digest.as_ref(),
        Some(expected.canonical_clone_url_digest())
    );
    assert_eq!(
        prepared.manifest.credential_profile.as_ref(),
        expected.credential_profile()
    );
    assert_eq!(prepared.manifest.recovery, Some(expected_recovery));
    assert_eq!(manifest_mode, DOCUMENT_MODE);
    assert_eq!(
        fs::read(placement.join("repo").join("prepared"))
            .expect("the prepared repository file is readable"),
        PREPARED_REPOSITORY_BYTES
    );
}

#[tokio::test]
async fn repository_workspace_reopen_replays_exact_commit_without_preparing_again() {
    let (parent, state) = fixture_root();
    let expected_recovery = Recovery::Commit {
        revision: "b".repeat(40),
    };
    let first = publish_repository(&state, expected_recovery).await;
    drop(state);
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state"))
        .expect("the runner root reopens after restart");
    let replay = reopened
        .workspace_store()
        .expect("the reopened root forms a workspace store")
        .prepare_repository_workspace(&repository_request(), |_| async {
            Err::<Recovery, std::io::Error>(std::io::Error::other(
                "replay must not prepare the repository",
            ))
        })
        .await
        .expect("the durable repository workspace replays");

    assert_eq!(replay, first);
}

#[tokio::test]
async fn repository_workspace_publishes_an_unborn_branch_without_a_revision() {
    let (_parent, state) = fixture_root();
    let mut request = repository_request();
    request.credential_profile = None;
    let expected_recovery = Recovery::UnbornBranch {
        name: "main".to_owned(),
    };
    let prepared = publish_repository_request(&state, &request, expected_recovery.clone()).await;

    assert_eq!(prepared.manifest.recovery, Some(expected_recovery));
    assert!(prepared.manifest.credential_profile.is_none());
}

#[tokio::test]
async fn repository_workspace_replay_rejects_a_changed_clone_url_identity() {
    let (_parent, state) = fixture_root();
    publish_repository(
        &state,
        Recovery::Branch {
            name: "main".to_owned(),
            revision: "c".repeat(40),
        },
    )
    .await;
    let mut conflict = repository_request();
    conflict.canonical_clone_url_digest =
        clone_url_digest("https://github.com/KeenWill/signalbox-renamed.git");
    let failure = state
        .workspace_store()
        .expect("the locked root forms another workspace store")
        .prepare_repository_workspace(&conflict, |_| async {
            Err::<Recovery, std::io::Error>(std::io::Error::other(
                "conflicting replay must not prepare the repository",
            ))
        })
        .await
        .expect_err("another clone URL cannot reinterpret the repository workspace");

    assert!(matches!(
        failure,
        PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::ManifestConflict)
    ));
}

fn enrolled_workspace_root() -> (TempDir, RunnerStateRoot) {
    let (parent, mut state) = fixture_root();
    let identity = |value| CanonicalUuid::from_uuid(Uuid::from_u128(value));
    state
        .record_receipt(crate::EnrollmentReceipt::new(
            state.state().request_id(),
            identity(RUNNER + 10),
            identity(RUNNER),
            identity(RUNNER + 11),
            PositiveU64::try_new(1).expect("first registration"),
            Digest::try_new("a".repeat(64)).expect("fixture digest"),
            crate::EnrollmentAuthority::Active,
        ))
        .expect("enrolled workspace owner");
    (parent, state)
}

fn release_correlation(prepared: &PreparedWorkspace) -> signalbox_runner_wire::ReleaseCorrelation {
    signalbox_runner_wire::ReleaseCorrelation {
        session_id: prepared.manifest.session,
        placement_revision: prepared.manifest.placement_revision,
        runner_id: prepared.manifest.runner,
        manifest_id: prepared.manifest.manifest_id,
    }
}

fn anonymous_repository_configuration() -> crate::RunnerConfiguration {
    crate::RunnerConfiguration::parse(
        &include_str!("../../../../config/signalbox-runner.example.toml")
            .replace("credential_profile = \"github-runner\"", ""),
    )
    .expect("anonymous repository configuration")
}

async fn acknowledged_repository() -> (TempDir, RunnerStateRoot, PreparedWorkspace) {
    let (parent, mut state) = enrolled_workspace_root();
    let prepared = acknowledge_repository_revision(&mut state, PLACEMENT_REVISION).await;
    (parent, state, prepared)
}

async fn acknowledge_repository_revision(
    state: &mut RunnerStateRoot,
    revision: u64,
) -> PreparedWorkspace {
    let mut request = repository_request();
    request.placement_revision =
        PositiveU64::try_new(revision).expect("positive fixture placement");
    request.credential_profile = None;
    request.sandbox_profile = SandboxProfile::Ambient;
    let recovery = Recovery::UnbornBranch {
        name: "main".to_owned(),
    };
    let operation = signalbox_runner_wire::WorkspaceProvision {
        correlation: signalbox_runner_wire::ProvisionCorrelation {
            authorization_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            session_id: request.session,
            placement_revision: request.placement_revision,
            runner_id: request.runner,
            registration_revision: PositiveU64::try_new(1).expect("first registration"),
            repository: Some(request.repository.clone()),
            sandbox_profile: request.sandbox_profile,
            credential_profile: None,
        },
        recovery: Some(recovery.clone()),
    };
    state
        .record_provision(operation.clone(), clone_url_digest(CLONE_URL))
        .expect("durable request");
    let prepared = publish_repository_request(state, &request, recovery).await;
    state
        .record_workspace_ready(signalbox_runner_wire::WorkspaceReady {
            correlation: operation.correlation.clone(),
            working_directory: prepared.execution_directory.as_str().to_owned(),
            ready: signalbox_runner_wire::ReadyManifest {
                manifest: prepared.manifest.clone(),
                manifest_digest: prepared.manifest_digest.clone(),
            },
        })
        .expect("durable ready receipt");
    state
        .workspace_store()
        .expect("workspace store")
        .activate(&prepared)
        .expect("activate acknowledged workspace");
    state
        .acknowledge_workspace(&signalbox_runner_wire::WorkspaceRecorded {
            correlation: operation.correlation,
            manifest_id: prepared.manifest.manifest_id,
            manifest_digest: prepared.manifest_digest.clone(),
        })
        .expect("retain acknowledged identity");
    prepared
}

#[tokio::test]
async fn acknowledged_workspace_restart_authenticates_repository_mapping() {
    let (parent, state, prepared) = acknowledged_repository().await;
    drop(state);
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    assert!(reopened.retained_provision().is_none());
    reopened
        .authenticate_active_workspaces(&anonymous_repository_configuration())
        .expect("unchanged active repository authenticates after acknowledgement");
    assert_eq!(
        fs::read(Path::new(prepared.execution_directory.as_str()).join("prepared"))
            .expect("preserved repository content"),
        PREPARED_REPOSITORY_BYTES
    );
    let changed = crate::RunnerConfiguration::parse(
        &include_str!("../../../../config/signalbox-runner.example.toml")
            .replace("credential_profile = \"github-runner\"", "")
            .replace(CLONE_URL, "https://github.com/KeenWill/different.git"),
    )
    .expect("changed repository configuration");
    assert!(matches!(
        reopened.authenticate_active_workspaces(&changed),
        Err(crate::WorkspaceProvisionError::ManifestConflict)
    ));
}

#[tokio::test]
async fn acknowledged_workspace_restart_rejects_replaced_execution_directory() {
    let (parent, state, prepared) = acknowledged_repository().await;
    drop(state);
    let directory = Path::new(prepared.execution_directory.as_str());
    fs::rename(directory, directory.with_file_name("original-repository"))
        .expect("retain original directory while replacing its name");
    fs::create_dir(directory).expect("replacement directory");
    fs::set_permissions(directory, fs::Permissions::from_mode(DIRECTORY_MODE))
        .expect("replacement has valid permissions");
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    assert!(matches!(
        reopened.authenticate_active_workspaces(&anonymous_repository_configuration()),
        Err(crate::WorkspaceProvisionError::ManifestConflict)
    ));
}

#[tokio::test]
async fn released_acknowledged_workspace_does_not_block_restart() {
    let (parent, mut state, prepared) = acknowledged_repository().await;
    let correlation = release_correlation(&prepared);
    state
        .record_release(correlation.clone())
        .expect("accept release");
    state
        .workspace_store()
        .expect("workspace store")
        .release(&state.accepted_release().expect("release authority"))
        .expect("remove active workspace");
    drop(state);
    let mut reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    reopened
        .authenticate_active_workspaces(&anonymous_repository_configuration())
        .expect("retained release owns the removed workspace");
    reopened
        .complete_release(&correlation)
        .expect("complete release");
    reopened
        .acknowledge_release(&correlation, false)
        .expect("acknowledge release");
    drop(reopened);
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    assert!(reopened.retained_release().is_none());
    reopened
        .authenticate_active_workspaces(&anonymous_repository_configuration())
        .expect("completed release forgets the removed active identity");
}

#[tokio::test]
async fn release_unlinks_symlinks_and_removes_inaccessible_directories() {
    let (parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let work = Path::new(prepared.execution_directory.as_str());
    let victim = parent.path().join("outside");
    fs::write(&victim, b"preserve outside bytes").expect("outside fixture");
    std::os::unix::fs::symlink(&victim, work.join("link")).expect("workspace symlink");
    let sealed = work.join("sealed");
    fs::create_dir(&sealed).expect("nested directory");
    fs::write(sealed.join("content"), b"remove nested bytes").expect("nested fixture");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o0)).expect("remove directory access");
    let correlation = release_correlation(&prepared);
    state
        .record_release(correlation.clone())
        .expect("accepted journal committed");
    let accepted = state.accepted_release().expect("journal cleanup authority");
    store.release(&accepted).expect("descriptor cleanup");
    assert!(!work.exists());
    assert_eq!(
        fs::read(&victim).expect("outside remains"),
        b"preserve outside bytes"
    );
    state
        .complete_release(&correlation)
        .expect("completion journal committed");
    drop(store);
    drop(state);
    let mut reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    assert_eq!(
        reopened.retained_release().map(|(_, phase, _)| phase),
        Some(signalbox_runner_wire::ReleasePhase::ReleaseCompleted)
    );
    reopened
        .acknowledge_release(&correlation, false)
        .expect("exact completion acknowledgement");
    assert!(reopened.retained_release().is_none());
}

#[tokio::test]
async fn accepted_release_restarts_after_trash_manifest_was_deleted() {
    let (parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let correlation = release_correlation(&prepared);
    state
        .record_release(correlation.clone())
        .expect("accepted journal committed");
    let placement = Path::new(prepared.execution_directory.as_str())
        .parent()
        .expect("placement");
    let trash = parent.path().join("runner-state/trash");
    fs::create_dir(&trash).expect("trash fixture");
    fs::set_permissions(&trash, fs::Permissions::from_mode(DIRECTORY_MODE)).expect("private trash");
    let renamed = trash.join(correlation.manifest_id.to_string());
    fs::rename(placement, &renamed).expect("simulate completed rename");
    fs::remove_file(renamed.join(MANIFEST_FILE)).expect("simulate partial deletion");
    drop(store);
    drop(state);
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("restart");
    let store = reopened
        .workspace_store()
        .expect("reopened workspace store");
    let accepted = reopened
        .accepted_release()
        .expect("retained journal authority");
    store
        .release(&accepted)
        .expect("journal authorizes remaining deletion");
    store
        .release(&accepted)
        .expect("completed deletion replays");
    assert!(!renamed.exists());
}

#[tokio::test]
async fn startup_inventory_preserves_releasing_manifest_before_and_after_rename() {
    let (parent, state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let placement_path = Path::new(prepared.execution_directory.as_str())
        .parent()
        .expect("placement");
    let placement = File::open(placement_path).expect("placement descriptor");
    let mut releasing = read_manifest(&placement).expect("ready manifest");
    releasing.lifecycle = ManifestLifecycle::Releasing;
    write_manifest(&placement, &releasing).expect("releasing manifest");
    drop(placement);
    let in_place = store
        .startup_leaks(prepared.manifest.runner)
        .expect("in-place releasing inventory");
    assert_eq!(in_place.len(), 1);
    assert_eq!(
        in_place[0].kind,
        signalbox_runner_wire::LeakFactKind::Unreconciled
    );
    assert_eq!(in_place[0].entry_digest, prepared.manifest_digest);
    assert_eq!(in_place[0].locator, prepared.manifest.relative_path);

    let trash = parent.path().join("runner-state/trash");
    fs::create_dir(&trash).expect("trash fixture");
    fs::set_permissions(&trash, fs::Permissions::from_mode(DIRECTORY_MODE)).expect("private trash");
    fs::rename(
        placement_path,
        trash.join(prepared.manifest.manifest_id.to_string()),
    )
    .expect("simulate committed release rename");
    fs::remove_file(
        trash
            .join(prepared.manifest.manifest_id.to_string())
            .join(REPOSITORY_WORKSPACE_DIRECTORY)
            .join("prepared"),
    )
    .expect("simulate partial recursive deletion of repository contents");
    fs::remove_dir(
        trash
            .join(prepared.manifest.manifest_id.to_string())
            .join(REPOSITORY_WORKSPACE_DIRECTORY),
    )
    .expect("simulate partial recursive deletion with the manifest retained");

    let facts = store
        .startup_leaks(prepared.manifest.runner)
        .expect("descriptor inventory");
    assert_eq!(facts.len(), 1);
    let fact = &facts[0];
    assert_eq!(fact.kind, signalbox_runner_wire::LeakFactKind::Unreconciled);
    assert_eq!(fact.locator, prepared.manifest.relative_path);
    assert_eq!(fact.entry_digest, prepared.manifest_digest);
    assert_eq!(fact.session, Some(prepared.manifest.session));
    assert_eq!(
        fact.placement_revision,
        Some(prepared.manifest.placement_revision)
    );
    fs::remove_file(
        trash
            .join(prepared.manifest.manifest_id.to_string())
            .join(MANIFEST_FILE),
    )
    .expect("simulate manifest deletion before the final directory removal fails");
    let facts = store
        .startup_leaks(prepared.manifest.runner)
        .expect("manifest-free trash inventory");
    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].locator,
        format!("trash/{}", prepared.manifest.manifest_id)
    );
    assert_eq!(
        facts[0].kind,
        signalbox_runner_wire::LeakFactKind::RetiredPresent
    );
    assert_eq!(facts[0].session, None);
    assert_eq!(facts[0].placement_revision, None);
}

#[tokio::test]
async fn startup_inventory_preserves_observed_in_place_conflict_digests() {
    let (_parent, state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let placement_path = Path::new(prepared.execution_directory.as_str())
        .parent()
        .expect("placement");
    let placement = File::open(placement_path).expect("placement descriptor");
    let mut conflicting = read_manifest(&placement).expect("ready manifest");
    conflicting.runner = CanonicalUuid::from_uuid(Uuid::from_u128(OTHER_RUNNER));
    for lifecycle in [ManifestLifecycle::Active, ManifestLifecycle::Releasing] {
        conflicting.lifecycle = lifecycle;
        write_manifest(&placement, &conflicting).expect("conflicting manifest");
        let facts = store
            .startup_leaks(prepared.manifest.runner)
            .expect("conflict inventory");
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].kind,
            signalbox_runner_wire::LeakFactKind::ManifestConflict
        );
        assert_eq!(
            facts[0].entry_digest,
            workspace_manifest_digest(&conflicting).expect("observed digest")
        );
    }
}

#[tokio::test]
async fn startup_inventory_collapses_duplicate_releasing_manifests() {
    let (parent, state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let placement_path = Path::new(prepared.execution_directory.as_str())
        .parent()
        .expect("placement");
    let placement = File::open(placement_path).expect("placement descriptor");
    let mut releasing = read_manifest(&placement).expect("ready manifest");
    releasing.lifecycle = ManifestLifecycle::Releasing;
    write_manifest(&placement, &releasing).expect("in-place releasing manifest");

    let trash = parent.path().join("runner-state/trash");
    fs::create_dir(&trash).expect("trash fixture");
    fs::set_permissions(&trash, fs::Permissions::from_mode(DIRECTORY_MODE)).expect("private trash");
    let duplicate = trash.join(prepared.manifest.manifest_id.to_string());
    fs::create_dir(&duplicate).expect("duplicate trash placement");
    fs::set_permissions(&duplicate, fs::Permissions::from_mode(DIRECTORY_MODE))
        .expect("private duplicate placement");
    let duplicate = File::open(duplicate).expect("duplicate placement descriptor");
    write_manifest(&duplicate, &releasing).expect("duplicate releasing manifest");

    let facts = store
        .startup_leaks(prepared.manifest.runner)
        .expect("duplicate inventory");
    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].kind,
        signalbox_runner_wire::LeakFactKind::Unreconciled
    );
    assert_eq!(facts[0].locator, prepared.manifest.relative_path);
    assert_eq!(facts[0].entry_digest, prepared.manifest_digest);
    leaks::pages(PositiveU64::try_new(1).expect("registration"), &facts)
        .expect("duplicate observations form a canonical report");
}

#[tokio::test]
async fn release_requires_the_provisioning_runner_and_exact_manifest() {
    let (_parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let mut correlation = release_correlation(&prepared);
    correlation.runner_id = CanonicalUuid::from_uuid(Uuid::from_u128(OTHER_RUNNER));
    assert!(state.record_release(correlation).is_err());
    let mut correlation = release_correlation(&prepared);
    correlation.manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    state
        .record_release(correlation)
        .expect("accepted different manifest correlation");
    assert!(matches!(
        store.release(&state.accepted_release().expect("journal authority")),
        Err(RunnerWorkspaceError::ManifestConflict)
    ));
    assert!(Path::new(prepared.execution_directory.as_str()).exists());
}

#[tokio::test]
async fn startup_inventory_reports_manifests_and_strays_without_following_links() {
    let (parent, state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    store.activate(&prepared).expect("active workspace");
    let sessions = parent.path().join("runner-state/sessions");
    std::os::unix::fs::symlink(parent.path(), sessions.join("unknown")).expect("untrusted link");
    let facts = store
        .startup_leaks(prepared.manifest.runner)
        .expect("descriptor inventory");
    assert_eq!(facts.len(), 2);
    assert!(facts.iter().any(|fact| fact.kind
        == signalbox_runner_wire::LeakFactKind::Unreconciled
        && fact.entry_digest == prepared.manifest_digest
        && fact.locator == prepared.manifest.relative_path));
    assert!(facts.iter().any(|fact| fact.kind
        == signalbox_runner_wire::LeakFactKind::UnknownManifest
        && fact.locator == "sessions/unknown"));
    let pages = leaks::pages(PositiveU64::try_new(1).expect("registration"), &facts)
        .expect("bounded report");
    assert_eq!(pages.len(), 1);
    let page = pages.front().expect("report page");
    assert!(page.final_page);
    page.validate().expect("canonical page");
    assert!(Path::new(prepared.execution_directory.as_str()).exists());
}

#[tokio::test]
async fn failed_repository_preparation_removes_its_unpublished_staging_tree() {
    let (parent, state) = fixture_root();
    let store = state.workspace_store().expect("workspace store");
    let request = repository_request();
    let result = store
        .prepare_repository_workspace(&request, |target| async move {
            fs::write(
                target.path().join("partial-clone"),
                PREPARED_REPOSITORY_BYTES,
            )?;
            Err::<Recovery, io::Error>(io::Error::other("checkout refused"))
        })
        .await;
    assert!(matches!(
        result,
        Err(PrepareRepositoryWorkspaceError::Preparation(_))
    ));
    let session = parent
        .path()
        .join("runner-state/sessions")
        .join(request.session().to_string());
    while fs::read_dir(&session).expect("session directory").count() != 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fs::read_dir(session).expect("session directory").count(), 0);
}

#[tokio::test]
async fn aborted_repository_preparation_removes_its_unpublished_staging_tree() {
    let (parent, state) = fixture_root();
    let store = state.workspace_store().expect("workspace store");
    let request = repository_request();
    let session = parent
        .path()
        .join("runner-state/sessions")
        .join(request.session().to_string());
    let (started, entered) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        store
            .prepare_repository_workspace(&request, |target| async move {
                fs::write(
                    target.path().join("partial-clone"),
                    PREPARED_REPOSITORY_BYTES,
                )?;
                started.send(()).expect("preparation entered");
                std::future::pending::<Result<Recovery, io::Error>>().await
            })
            .await
    });
    entered.await.expect("staging exists");
    worker.abort();
    assert!(worker.await.expect_err("worker aborted").is_cancelled());
    while fs::read_dir(&session).expect("session directory").count() != 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fs::read_dir(session).expect("session directory").count(), 0);
}
#[test]
fn reconnect_scan_waits_for_canceled_staging_cleanup() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("one runtime worker and one blocking worker");
    runtime.block_on(async {
        let (_parent, state) = fixture_root();
        let preparation_store = state.workspace_store().expect("preparation store");
        let scan_store = state
            .workspace_store()
            .expect("reconnect store shares cleanup lifetime");
        let request = repository_request();
        let runner = request.runner();
        let (entered, preparing) = tokio::sync::oneshot::channel();
        let preparation = tokio::spawn(async move {
            preparation_store
                .prepare_repository_workspace(&request, |target| async move {
                    fs::write(
                        target.path().join("partial-clone"),
                        PREPARED_REPOSITORY_BYTES,
                    )?;
                    entered.send(()).expect("staging observer");
                    std::future::pending::<Result<Recovery, io::Error>>().await
                })
                .await
        });
        preparing
            .await
            .expect("staging exists before transport loss");
        let (unblock, held) = std::sync::mpsc::channel();
        let (entered, occupied) = tokio::sync::oneshot::channel();
        let blocking = tokio::task::spawn_blocking(move || {
            entered.send(()).expect("blocking-pool observer");
            held.recv().expect("blocking-pool release");
        });
        occupied.await.expect("hold filesystem workers");
        preparation.abort();
        let mut scan = Box::pin(scan_store.scan_startup_leaks(runner));
        // Poll reconnect before cancellation drops the staging guard. An uncoordinated
        // scan would queue ahead of deletion on the occupied blocking pool.
        std::future::poll_fn(|context| {
            assert!(scan.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(
            preparation
                .await
                .expect_err("transport canceled preparation")
                .is_cancelled()
        );
        unblock.send(()).expect("permit cleanup and scanning");
        blocking.await.expect("blocking worker released");
        let facts = scan.await.expect("reconnect scan completes after cleanup");
        assert!(
            facts.is_empty(),
            "successful unpublished cleanup must not produce a permanent leak fact: {facts:?}"
        );
    });
}

#[tokio::test]
async fn active_receipts_larger_than_a_frame_do_not_block_workspace_acknowledgement() {
    use crate::active_workspaces::ActiveWorkspaces;
    let (parent, state, _) = acknowledged_repository().await;
    let root = parent.path().join("runner-state");
    let path = root.join("active-workspaces.json");
    let mut receipts: ActiveWorkspaces =
        serde_json::from_slice(&fs::read(&path).expect("active receipt document"))
            .expect("protected receipts");
    let seed = receipts
        .records
        .values()
        .next()
        .expect("acknowledged fixture")
        .clone();
    let entry_bytes = serde_json::to_vec(&seed).expect("receipt encoding").len();
    // Generate enough syntactically valid retained receipts to cross the operation frame
    // ceiling. Their filesystem authentication is exercised by the restart tests above.
    for _ in 0..=signalbox_runner_wire::MAX_FRAME_BYTES / entry_bytes {
        let mut record = seed.clone();
        let manifest = &mut record.ready.ready.manifest;
        manifest.manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
        record.ready.ready.manifest_digest =
            workspace_manifest_digest(manifest).expect("ready digest");
        receipts.records.insert(manifest.manifest_id, record);
    }
    let encoded = serde_json::to_vec(&receipts).expect("retained receipt inventory");
    assert!(encoded.len() > signalbox_runner_wire::MAX_FRAME_BYTES);
    fs::write(&path, encoded).expect("retained inventory fixture");
    drop(state);
    let mut reopened = RunnerStateRoot::open(&root)
        .expect("receipt inventory is independent of the frame ceiling");
    let next = acknowledge_repository_revision(&mut reopened, PLACEMENT_REVISION + 1).await;
    assert!(
        reopened.retained_provision().is_none(),
        "acknowledgement clears the pending operation"
    );
    assert!(
        fs::metadata(root.join("operation-journal.json"))
            .expect("operation journal")
            .len()
            < signalbox_runner_wire::MAX_FRAME_BYTES as u64
    );
    drop(reopened);
    let reopened = RunnerStateRoot::open(&root).expect("acknowledgement survives another restart");
    assert!(reopened.retained_provision().is_none());
    let receipts: ActiveWorkspaces =
        serde_json::from_slice(&fs::read(path).expect("retained inventory"))
            .expect("receipt document");
    assert!(receipts.records.contains_key(&next.manifest.manifest_id));
}

#[tokio::test]
async fn startup_inventory_reports_inconsistent_trash_manifests_as_conflicts() {
    let (parent, state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = publish_repository(
        &state,
        Recovery::UnbornBranch {
            name: "main".to_owned(),
        },
    )
    .await;
    let placement_path = Path::new(prepared.execution_directory.as_str())
        .parent()
        .expect("placement");
    let trash = parent.path().join("runner-state/trash");
    fs::create_dir(&trash).expect("trash fixture");
    fs::set_permissions(&trash, fs::Permissions::from_mode(DIRECTORY_MODE)).expect("private trash");
    let trashed = trash.join(prepared.manifest.manifest_id.to_string());
    fs::rename(placement_path, &trashed).expect("release rename");
    let directory = File::open(&trashed).expect("trash descriptor");
    let mut releasing = read_manifest(&directory).expect("manifest");
    releasing.lifecycle = ManifestLifecycle::Releasing;
    let mut wrong_id = releasing.clone();
    wrong_id.manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let mut wrong_runner = releasing.clone();
    wrong_runner.runner = CanonicalUuid::from_uuid(Uuid::from_u128(OTHER_RUNNER));
    let mut wrong_path = releasing.clone();
    wrong_path.relative_path = format!("sessions/{}/99/repo", releasing.session);
    let mut wrong_lifecycle = releasing.clone();
    wrong_lifecycle.lifecycle = ManifestLifecycle::Active;
    for manifest in [wrong_id, wrong_runner, wrong_path, wrong_lifecycle] {
        write_manifest(&directory, &manifest).expect("readable inconsistent manifest");
        let facts = store
            .startup_leaks(prepared.manifest.runner)
            .expect("inventory");
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].kind,
            signalbox_runner_wire::LeakFactKind::ManifestConflict
        );
        assert_eq!(
            facts[0].locator,
            format!("trash/{}", prepared.manifest.manifest_id)
        );
        assert_eq!(
            facts[0].entry_digest,
            workspace_manifest_digest(&manifest).expect("observed digest")
        );
    }
}
