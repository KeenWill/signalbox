//! Descriptor-relative managed workspace storage below the locked runner root.

use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt as _, PermissionsExt as _},
        io::AsRawFd as _,
    },
};

use rustix::{
    fs::{
        AtFlags, Mode, OFlags, RenameFlags, fchmod, mkdirat, openat, renameat, renameat_with,
        statat, unlinkat,
    },
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::{
    CanonicalUuid, ManifestLifecycle, PositiveU64, ReadyManifest, SandboxProfile,
    WorkspaceManifest, workspace_manifest_digest,
};
use uuid::Uuid;

const DIRECTORY_MODE: u32 = 0o700;
const DOCUMENT_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const MANIFEST_DOCUMENT_VERSION: u64 = 1;
const MANIFEST_FILE: &str = "workspace-manifest.json";
const MAXIMUM_MANIFEST_BYTES: u64 = 16 * 1024;
const SESSIONS_DIRECTORY: &str = "sessions";
const PRIVATE_WORKSPACE_DIRECTORY: &str = "work";

/// Complete durable facts needed to prepare one repository-free managed root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateWorkspaceRequest {
    session: CanonicalUuid,
    placement_revision: PositiveU64,
    runner: CanonicalUuid,
    sandbox_profile: SandboxProfile,
}

impl PrivateWorkspaceRequest {
    /// Constructs one explicit repository-free workspace request.
    pub const fn new(
        session: CanonicalUuid,
        placement_revision: PositiveU64,
        runner: CanonicalUuid,
        sandbox_profile: SandboxProfile,
    ) -> Self {
        Self {
            session,
            placement_revision,
            runner,
            sandbox_profile,
        }
    }

    /// Returns the owning session.
    pub const fn session(&self) -> CanonicalUuid {
        self.session
    }

    /// Returns the positive placement revision.
    pub const fn placement_revision(&self) -> PositiveU64 {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub const fn runner(&self) -> CanonicalUuid {
        self.runner
    }

    /// Returns the exact sandbox profile.
    pub const fn sandbox_profile(&self) -> SandboxProfile {
        self.sandbox_profile
    }

    fn relative_path(&self) -> String {
        format!(
            "{SESSIONS_DIRECTORY}/{}/{}/{PRIVATE_WORKSPACE_DIRECTORY}",
            self.session,
            self.placement_revision.get(),
        )
    }
}

/// Sanitized managed-workspace storage failure.
#[derive(Debug)]
pub enum RunnerWorkspaceError {
    /// A descriptor-relative filesystem operation failed.
    Io(io::Error),
    /// The private-root request names an unsupported sandbox profile.
    UnsupportedSandboxProfile,
    /// Existing workspace facts conflict with the requested placement.
    ManifestConflict,
    /// The protected manifest document is malformed or has the wrong identity.
    CorruptManifest,
    /// The protected manifest exceeds its fixed byte bound.
    ManifestTooLarge,
    /// Publication may have committed but the containing-directory sync failed.
    CommitAmbiguous(io::Error),
}

impl fmt::Display for RunnerWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "runner workspace storage failed",
            Self::UnsupportedSandboxProfile => {
                "runner private workspace requires the restricted sandbox profile"
            }
            Self::ManifestConflict => "runner workspace manifest conflicts with the request",
            Self::CorruptManifest => "runner workspace manifest is corrupt",
            Self::ManifestTooLarge => "runner workspace manifest exceeds its byte bound",
            Self::CommitAmbiguous(_) => "runner workspace publication commit is ambiguous",
        })
    }
}

impl Error for RunnerWorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) | Self::CommitAmbiguous(source) => Some(source),
            Self::UnsupportedSandboxProfile
            | Self::ManifestConflict
            | Self::CorruptManifest
            | Self::ManifestTooLarge => None,
        }
    }
}

/// Descriptor-pinned managed-workspace store sharing the runner-root lock.
#[derive(Debug)]
pub struct RunnerWorkspaceStore {
    root: File,
}

impl RunnerWorkspaceStore {
    pub(crate) const fn from_root(root: File) -> Self {
        Self { root }
    }

    /// Creates and publishes one private root, or replays its exact ready manifest.
    pub fn prepare_private_root(
        &self,
        request: &PrivateWorkspaceRequest,
    ) -> Result<ReadyManifest, RunnerWorkspaceError> {
        if request.sandbox_profile() != SandboxProfile::WorkspaceRestricted {
            return Err(RunnerWorkspaceError::UnsupportedSandboxProfile);
        }
        let sessions = open_or_create_directory(&self.root, SESSIONS_DIRECTORY)?;
        let session_name = request.session().to_string();
        let session = open_or_create_directory(&sessions, &session_name)?;
        let placement_name = request.placement_revision().get().to_string();
        match open_directory(&session, &placement_name) {
            Ok(placement) => read_ready_private_workspace(&placement, request),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_workspace(&session, &placement_name, request)
            }
            Err(error) => Err(RunnerWorkspaceError::Io(error)),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    version: u64,
    manifest: WorkspaceManifest,
}

fn create_private_workspace(
    session: &File,
    placement_name: &str,
    request: &PrivateWorkspaceRequest,
) -> Result<ReadyManifest, RunnerWorkspaceError> {
    let manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let staging_name = format!(".{placement_name}-{manifest_id}.staging");
    mkdirat(
        session,
        staging_name.as_str(),
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .map_err(rustix_io)?;
    let staging = open_created_directory(session, &staging_name)?;
    let mut manifest = private_manifest(ManifestLifecycle::Staging, manifest_id, request);
    let prepared = (|| {
        write_manifest(&staging, &manifest)?;
        let work = open_or_create_directory(&staging, PRIVATE_WORKSPACE_DIRECTORY)?;
        work.sync_all().map_err(RunnerWorkspaceError::Io)?;
        manifest.lifecycle = ManifestLifecycle::Ready;
        write_manifest(&staging, &manifest)?;
        staging.sync_all().map_err(RunnerWorkspaceError::Io)?;
        if !path_names_directory(&staging, PRIVATE_WORKSPACE_DIRECTORY, &work)?
            || !path_names_directory(session, &staging_name, &staging)?
        {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        cleanup_unpublished_workspace(session, &staging_name, &staging)?;
        return Err(error);
    }
    if let Err(error) = renameat_with(
        session,
        staging_name.as_str(),
        session,
        placement_name,
        RenameFlags::NOREPLACE,
    ) {
        cleanup_unpublished_workspace(session, &staging_name, &staging)?;
        return Err(RunnerWorkspaceError::Io(io::Error::from_raw_os_error(
            error.raw_os_error(),
        )));
    }
    session
        .sync_all()
        .map_err(RunnerWorkspaceError::CommitAmbiguous)?;
    let placement = open_directory(session, placement_name).map_err(RunnerWorkspaceError::Io)?;
    read_ready_private_workspace(&placement, request)
}

fn cleanup_unpublished_workspace(
    session: &File,
    staging_name: &str,
    staging: &File,
) -> Result<(), RunnerWorkspaceError> {
    match unlinkat(staging, MANIFEST_FILE, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(rustix_io(error)),
    }
    match unlinkat(staging, PRIVATE_WORKSPACE_DIRECTORY, AtFlags::REMOVEDIR) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(rustix_io(error)),
    }
    if path_names_directory(session, staging_name, staging)? {
        unlinkat(session, staging_name, AtFlags::REMOVEDIR).map_err(rustix_io)?;
        session.sync_all().map_err(RunnerWorkspaceError::Io)?;
    }
    Ok(())
}

fn private_manifest(
    lifecycle: ManifestLifecycle,
    manifest_id: CanonicalUuid,
    request: &PrivateWorkspaceRequest,
) -> WorkspaceManifest {
    WorkspaceManifest {
        lifecycle,
        manifest_id,
        session: request.session(),
        placement_revision: request.placement_revision(),
        runner: request.runner(),
        repository: None,
        canonical_clone_url_digest: None,
        credential_profile: None,
        sandbox_profile: request.sandbox_profile(),
        relative_path: request.relative_path(),
        recovery: None,
    }
}

fn read_ready_private_workspace(
    placement: &File,
    request: &PrivateWorkspaceRequest,
) -> Result<ReadyManifest, RunnerWorkspaceError> {
    let manifest = read_manifest(placement)?;
    let expected = private_manifest(ManifestLifecycle::Ready, manifest.manifest_id, request);
    if manifest != expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let _work = open_directory(placement, PRIVATE_WORKSPACE_DIRECTORY)
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    let manifest_digest =
        workspace_manifest_digest(&manifest).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    Ok(ReadyManifest {
        manifest,
        manifest_digest,
    })
}

fn open_or_create_directory(parent: &File, name: &str) -> Result<File, RunnerWorkspaceError> {
    match open_directory(parent, name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(rustix_io)?;
            let directory = open_created_directory(parent, name)?;
            parent.sync_all().map_err(RunnerWorkspaceError::Io)?;
            Ok(directory)
        }
        Err(error) => Err(RunnerWorkspaceError::Io(error)),
    }
}

fn open_directory(parent: &File, name: &str) -> Result<File, io::Error> {
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let directory = File::from(descriptor);
    validate_directory(parent, name, &directory)?;
    Ok(directory)
}

fn validate_directory(parent: &File, name: &str, directory: &File) -> Result<(), io::Error> {
    let metadata = directory.metadata()?;
    let path_status = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    if !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
        || metadata.dev() != path_status.st_dev
        || metadata.ino() != path_status.st_ino
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace directory identity is invalid",
        ));
    }
    Ok(())
}

fn open_created_directory(parent: &File, name: &str) -> Result<File, RunnerWorkspaceError> {
    let pinned_descriptor = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    let pinned = File::from(pinned_descriptor);
    let descriptor_path = format!("/proc/self/fd/{}", pinned.as_raw_fd());
    std::fs::set_permissions(
        descriptor_path,
        std::fs::Permissions::from_mode(DIRECTORY_MODE),
    )
    .map_err(RunnerWorkspaceError::Io)?;
    let descriptor = openat(
        &pinned,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    fchmod(&descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(rustix_io)?;
    let directory = File::from(descriptor);
    validate_directory(parent, name, &directory).map_err(RunnerWorkspaceError::Io)?;
    Ok(directory)
}

fn path_names_directory(
    parent: &File,
    name: &str,
    directory: &File,
) -> Result<bool, RunnerWorkspaceError> {
    let metadata = directory.metadata().map_err(RunnerWorkspaceError::Io)?;
    let status = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(rustix_io)?;
    Ok(metadata.is_dir() && metadata.dev() == status.st_dev && metadata.ino() == status.st_ino)
}

fn read_manifest(directory: &File) -> Result<WorkspaceManifest, RunnerWorkspaceError> {
    let descriptor = openat(
        directory,
        MANIFEST_FILE,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(RunnerWorkspaceError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.permissions().mode() & PERMISSION_MASK != DOCUMENT_MODE
    {
        return Err(RunnerWorkspaceError::CorruptManifest);
    }
    if metadata.len() > MAXIMUM_MANIFEST_BYTES {
        return Err(RunnerWorkspaceError::ManifestTooLarge);
    }
    let mut encoded = Vec::new();
    Read::by_ref(&mut file)
        .take(MAXIMUM_MANIFEST_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(RunnerWorkspaceError::Io)?;
    if encoded.len() as u64 > MAXIMUM_MANIFEST_BYTES {
        return Err(RunnerWorkspaceError::ManifestTooLarge);
    }
    let document: ManifestDocument =
        serde_json::from_slice(&encoded).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    if document.version != MANIFEST_DOCUMENT_VERSION || document.manifest.validate().is_err() {
        return Err(RunnerWorkspaceError::CorruptManifest);
    }
    Ok(document.manifest)
}

fn write_manifest(
    directory: &File,
    manifest: &WorkspaceManifest,
) -> Result<(), RunnerWorkspaceError> {
    manifest
        .validate()
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    let document = ManifestDocument {
        version: MANIFEST_DOCUMENT_VERSION,
        manifest: manifest.clone(),
    };
    let mut encoded =
        serde_json::to_vec(&document).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAXIMUM_MANIFEST_BYTES {
        return Err(RunnerWorkspaceError::ManifestTooLarge);
    }
    let temporary_name = format!(".{MANIFEST_FILE}-{}.tmp", Uuid::now_v7());
    let descriptor = openat(
        directory,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(rustix_io)?;
    fchmod(&descriptor, Mode::RUSR | Mode::WUSR).map_err(rustix_io)?;
    let mut temporary = File::from(descriptor);
    let prepared = temporary
        .write_all(&encoded)
        .and_then(|()| temporary.sync_all());
    if let Err(source) = prepared {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(RunnerWorkspaceError::Io(source));
    }
    if let Err(error) = renameat(directory, temporary_name.as_str(), directory, MANIFEST_FILE) {
        let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        return Err(rustix_io(error));
    }
    directory.sync_all().map_err(RunnerWorkspaceError::Io)
}

fn rustix_io(error: rustix::io::Errno) -> RunnerWorkspaceError {
    RunnerWorkspaceError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use expect_test::expect;
    use signalbox_runner_wire::{
        CanonicalUuid, ManifestLifecycle, PositiveU64, SandboxProfile, workspace_manifest_digest,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::RunnerStateRoot;

    use super::{
        MANIFEST_FILE, PRIVATE_WORKSPACE_DIRECTORY, PrivateWorkspaceRequest, RunnerWorkspaceError,
        cleanup_unpublished_workspace, open_created_directory,
    };

    const SESSION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e1;
    const RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e2;
    const OTHER_RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e3;
    const PLACEMENT_REVISION: u64 = 3;
    const OPEN_DIRECTORY_MODE: u32 = 0o750;
    const EXPECTED_DOCUMENT_MODE: u32 = 0o600;

    fn fixture_root() -> (TempDir, RunnerStateRoot) {
        let parent = tempfile::tempdir().expect("the workspace fixture parent exists");
        let root = RunnerStateRoot::open(&parent.path().join("runner-state"))
            .expect("the owner-private runner root opens");
        (parent, root)
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

        assert_eq!(prepared.manifest.session, expected.session());
        assert_eq!(prepared.manifest.runner, expected.runner());
        assert_eq!(prepared.manifest.lifecycle, ManifestLifecycle::Ready);
        assert_eq!(prepared.manifest.relative_path, expected.relative_path());
        assert_eq!(
            prepared.manifest_digest,
            workspace_manifest_digest(&prepared.manifest)
                .expect("the ready private manifest has its canonical digest")
        );
        assert!(prepared.manifest.repository.is_none());
        assert!(prepared.manifest.credential_profile.is_none());
        assert!(prepared.manifest.recovery.is_none());
        assert!(placement.join("work").is_dir());
        assert_eq!(manifest_mode, EXPECTED_DOCUMENT_MODE);
    }

    #[test]
    fn created_directory_is_reopened_after_restoring_user_permissions() {
        let (parent, _state) = fixture_root();
        let root_path = parent.path().join("runner-state");
        let inaccessible = root_path.join("inaccessible");
        fs::create_dir(&inaccessible).expect("the inaccessible fixture directory exists");
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000))
            .expect("the fixture starts without user access");
        let root = fs::File::open(&root_path).expect("the runner root remains openable");

        let reopened = open_created_directory(&root, "inaccessible")
            .expect("user permissions are restored before reopening");
        let reopened_mode = reopened
            .metadata()
            .expect("the reopened directory has metadata")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(reopened_mode, 0o700);
    }

    #[test]
    fn unpublished_workspace_cleanup_removes_the_staging_tree() {
        let (parent, _state) = fixture_root();
        let session_path = parent.path().join("runner-state").join("cleanup-session");
        let staging_path = session_path.join(".cleanup.staging");
        fs::create_dir(&session_path).expect("the cleanup session fixture exists");
        fs::create_dir(&staging_path).expect("the cleanup staging fixture exists");
        fs::create_dir(staging_path.join(PRIVATE_WORKSPACE_DIRECTORY))
            .expect("the unpublished work fixture exists");
        fs::write(staging_path.join(MANIFEST_FILE), b"unpublished")
            .expect("the unpublished manifest fixture exists");
        let session = fs::File::open(&session_path).expect("the cleanup session opens");
        let staging = fs::File::open(&staging_path).expect("the cleanup staging directory opens");

        cleanup_unpublished_workspace(&session, ".cleanup.staging", &staging)
            .expect("the definitely unpublished staging tree is removed");

        assert!(!staging_path.exists());
    }

    #[test]
    fn private_root_rejects_ambient_profile_before_creating_storage() {
        let (parent, state) = fixture_root();
        let ambient = PrivateWorkspaceRequest::new(
            CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
            PositiveU64::try_new(PLACEMENT_REVISION)
                .expect("the fixture placement revision is positive"),
            CanonicalUuid::from_uuid(Uuid::from_u128(RUNNER)),
            SandboxProfile::Ambient,
        );
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&ambient)
            .expect_err("an ambient private workspace is not admissible");

        assert!(matches!(
            failure,
            RunnerWorkspaceError::UnsupportedSandboxProfile
        ));
        assert!(!parent.path().join("runner-state").join("sessions").exists());
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

        expect![[r#"runner workspace storage failed"#]].assert_eq(&failure.to_string());
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

        expect![[r#"runner workspace storage failed"#]].assert_eq(&failure.to_string());
        assert_eq!(retained_mode, OPEN_DIRECTORY_MODE);
    }
}
