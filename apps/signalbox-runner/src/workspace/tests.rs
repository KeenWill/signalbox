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
const OPEN_DIRECTORY_MODE: u32 = 0o750;
const EXPECTED_RELATIVE_PATH: &str = "sessions/018f6f10-0000-7000-8000-0000000000e1/3/work";
const CLONE_URL: &str = "https://github.com/KeenWill/signalbox.git";
const PREPARED_REPOSITORY_BYTES: &[u8] = b"repository\n";
fn fixture_root() -> (TempDir, RunnerStateRoot) {
    let parent = tempfile::tempdir().expect("temporary parent");
    let state = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("locked root");
    (parent, state)
}
fn request(runner: u128) -> PrivateWorkspaceRequest {
    PrivateWorkspaceRequest::new(
        CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
        PositiveU64::try_new(PLACEMENT_REVISION)
            .expect("the fixture placement revision is positive"),
        CanonicalUuid::from_uuid(Uuid::from_u128(runner)),
        SandboxProfile::WorkspaceRestricted,
    )
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

#[test]
fn private_root_publishes_exact_ready_facts_and_permissions() {
    let (parent, state) = fixture_root();
    let expected = request(RUNNER);
    let prepared = state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&expected)
        .expect("the private workspace publishes");
    let placement = parent
        .path()
        .join("runner-state")
        .join("sessions")
        .join(expected.session().to_string())
        .join(expected.placement_revision().get().to_string());
    let manifest_mode = fs::metadata(placement.join(MANIFEST_FILE))
        .expect("the protected manifest is inspectable")
        .permissions()
        .mode()
        & 0o7777;
    let expected_execution_directory = placement
        .join("work")
        .canonicalize()
        .expect("the published work directory has a canonical path");

    assert_eq!(prepared.manifest.session, expected.session());
    assert_eq!(prepared.manifest.runner, expected.runner());
    assert_eq!(prepared.manifest.lifecycle, ManifestLifecycle::Ready);
    assert_eq!(prepared.manifest.relative_path, EXPECTED_RELATIVE_PATH);
    assert_eq!(
        prepared.execution_directory.as_str(),
        expected_execution_directory
            .to_str()
            .expect("the fixture path is UTF-8")
    );
    assert_eq!(
        prepared.manifest_digest,
        workspace_manifest_digest(&prepared.manifest)
            .expect("the ready private manifest has its canonical digest")
    );
    assert!(prepared.manifest.repository.is_none());
    assert!(prepared.manifest.credential_profile.is_none());
    assert!(prepared.manifest.recovery.is_none());
    assert!(placement.join("work").is_dir());
    assert_eq!(manifest_mode, DOCUMENT_MODE);
}

#[test]
fn private_root_reopen_replays_the_exact_ready_manifest() {
    let (parent, state) = fixture_root();
    let first = state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&request(RUNNER))
        .expect("the private workspace publishes");
    drop(state);
    let reopened = RunnerStateRoot::open(&parent.path().join("runner-state"))
        .expect("the runner root reopens after restart");
    let replay = reopened
        .workspace_store()
        .expect("the reopened root forms a workspace store")
        .prepare_private_root(&request(RUNNER))
        .expect("the durable private workspace replays");

    assert_eq!(replay, first);
}

#[test]
fn private_root_replay_rejects_conflicting_runner_facts() {
    let (_parent, state) = fixture_root();
    state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&request(RUNNER))
        .expect("the private workspace publishes");
    let conflict = state
        .workspace_store()
        .expect("the locked root forms another workspace store")
        .prepare_private_root(&request(OTHER_RUNNER))
        .expect_err("another runner cannot reinterpret the workspace");

    assert!(matches!(conflict, RunnerWorkspaceError::ManifestConflict));
}

#[test]
fn private_root_replay_rejects_a_corrupt_protected_manifest() {
    let (parent, state) = fixture_root();
    let expected = request(RUNNER);
    state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&expected)
        .expect("the private workspace publishes");
    let manifest = parent
        .path()
        .join("runner-state")
        .join("sessions")
        .join(expected.session().to_string())
        .join(expected.placement_revision().get().to_string())
        .join(MANIFEST_FILE);
    fs::write(&manifest, b"{}\n").expect("the protected manifest fixture is corrupted");
    let failure = state
        .workspace_store()
        .expect("the locked root forms another workspace store")
        .prepare_private_root(&expected)
        .expect_err("a corrupt protected manifest fails closed");

    assert!(matches!(failure, RunnerWorkspaceError::CorruptManifest));
}

#[test]
fn private_root_rejects_a_symlinked_sessions_directory() {
    let (parent, state) = fixture_root();
    let outside = parent.path().join("outside");
    fs::create_dir(&outside).expect("the outside fixture directory exists");
    std::os::unix::fs::symlink(
        &outside,
        parent.path().join("runner-state").join("sessions"),
    )
    .expect("the sessions alias fixture exists");
    let failure = state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&request(RUNNER))
        .expect_err("a symlink cannot stand in for the sessions directory");

    assert_eq!(failure.to_string(), "runner workspace storage failed");
    assert_eq!(
        fs::read_dir(&outside)
            .expect("the outside fixture remains readable")
            .count(),
        0
    );
}

#[test]
fn private_root_rejects_without_repairing_an_open_sessions_directory() {
    let (parent, state) = fixture_root();
    let sessions = parent.path().join("runner-state").join("sessions");
    fs::create_dir(&sessions).expect("the open sessions fixture exists");
    fs::set_permissions(&sessions, fs::Permissions::from_mode(OPEN_DIRECTORY_MODE))
        .expect("the sessions fixture is deliberately group-readable");
    let failure = state
        .workspace_store()
        .expect("the locked root forms a workspace store")
        .prepare_private_root(&request(RUNNER))
        .expect_err("an open sessions directory fails closed");
    let retained_mode = fs::metadata(&sessions)
        .expect("the rejected sessions directory remains inspectable")
        .permissions()
        .mode()
        & 0o7777;

    assert_eq!(failure.to_string(), "runner workspace storage failed");
    assert_eq!(retained_mode, OPEN_DIRECTORY_MODE);
}

#[test]
fn activated_private_root_reopens_with_its_files_and_same_ready_receipt() {
    let (parent, state) = fixture_root();
    let store = state.workspace_store().expect("workspace store");
    let first = store
        .prepare_private_root(&request(RUNNER))
        .expect("ready root");
    let work = Path::new(first.execution_directory.as_str());
    fs::write(work.join("session-notes"), b"retained session files").expect("session file");
    store.activate(&first).expect("ack activates manifest");
    store.activate(&first).expect("equal ack replays");
    let active = read_manifest(
        &open_directory(
            &File::open(work.parent().expect("placement")).expect("placement descriptor"),
            ".",
        )
        .expect("private placement"),
    )
    .expect("active manifest");
    assert_eq!(active.lifecycle, ManifestLifecycle::Active);
    drop(store);
    drop(state);
    let state = RunnerStateRoot::open(&parent.path().join("runner-state")).expect("reopened root");
    let replay = state
        .workspace_store()
        .expect("store")
        .prepare_private_root(&request(RUNNER))
        .expect("re-adopt files");
    assert_eq!(replay, first);
    assert_eq!(
        fs::read(work.join("session-notes")).expect("retained file"),
        b"retained session files"
    );
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

#[test]
fn release_unlinks_symlinks_and_removes_inaccessible_directories() {
    let (parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = store
        .prepare_private_root(&request(RUNNER))
        .expect("private workspace");
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
        reopened.retained_release().map(|(_, phase)| phase),
        Some(signalbox_runner_wire::ReleasePhase::ReleaseCompleted)
    );
    reopened
        .acknowledge_release(&correlation)
        .expect("exact completion acknowledgement");
    assert!(reopened.retained_release().is_none());
}

#[test]
fn accepted_release_restarts_after_trash_manifest_was_deleted() {
    let (parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = store
        .prepare_private_root(&request(RUNNER))
        .expect("private workspace");
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

#[test]
fn release_requires_the_provisioning_runner_and_exact_manifest() {
    let (_parent, mut state) = enrolled_workspace_root();
    let store = state.workspace_store().expect("owned workspace store");
    let prepared = store
        .prepare_private_root(&request(RUNNER))
        .expect("private workspace");
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
