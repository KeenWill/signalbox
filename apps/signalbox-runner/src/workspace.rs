//! Descriptor-relative managed workspace storage below the locked runner root.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    future::Future,
    io::{self, Read, Write},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    rc::Rc,
};

use rustix::{
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, fchmod, mkdirat, openat, renameat,
        renameat_with, statat, unlinkat,
    },
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::{
    CanonicalUuid, Digest, ManifestLifecycle, PositiveU64, ProfileName, ReadyManifest, Recovery,
    RepositoryKey, SandboxProfile, WorkingDirectory, WorkspaceManifest, workspace_manifest_digest,
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
const REPOSITORY_WORKSPACE_DIRECTORY: &str = "repo";
const TRASH_DIRECTORY: &str = "trash";

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

/// Complete durable facts needed to publish one repository workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWorkspaceRequest {
    session: CanonicalUuid,
    placement_revision: PositiveU64,
    runner: CanonicalUuid,
    repository: RepositoryKey,
    canonical_clone_url_digest: Digest,
    credential_profile: Option<ProfileName>,
    sandbox_profile: SandboxProfile,
}

impl RepositoryWorkspaceRequest {
    /// Constructs one explicit repository-workspace request.
    pub const fn new(
        session: CanonicalUuid,
        placement_revision: PositiveU64,
        runner: CanonicalUuid,
        repository: RepositoryKey,
        canonical_clone_url_digest: Digest,
        credential_profile: Option<ProfileName>,
        sandbox_profile: SandboxProfile,
    ) -> Self {
        Self {
            session,
            placement_revision,
            runner,
            repository,
            canonical_clone_url_digest,
            credential_profile,
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

    /// Borrows the exact configured repository key.
    pub const fn repository(&self) -> &RepositoryKey {
        &self.repository
    }

    /// Borrows the canonical configured clone-URL digest.
    pub const fn canonical_clone_url_digest(&self) -> &Digest {
        &self.canonical_clone_url_digest
    }

    /// Borrows the independently optional selected credential profile.
    pub const fn credential_profile(&self) -> Option<&ProfileName> {
        self.credential_profile.as_ref()
    }

    /// Returns the exact sandbox profile.
    pub const fn sandbox_profile(&self) -> SandboxProfile {
        self.sandbox_profile
    }

    fn relative_path(&self) -> String {
        format!(
            "{SESSIONS_DIRECTORY}/{}/{}/{REPOSITORY_WORKSPACE_DIRECTORY}",
            self.session,
            self.placement_revision.get(),
        )
    }
}

/// Descriptor-authenticated empty repository directory supplied to one preparer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWorkspaceTarget {
    path: PathBuf,
}

impl RepositoryWorkspaceTarget {
    /// Borrows the canonical staging directory mounted for repository preparation.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Failure before or after one caller-owned repository preparation operation.
#[derive(Debug)]
pub enum PrepareRepositoryWorkspaceError<PreparationError> {
    /// Descriptor-relative storage or publication failed.
    Storage(RunnerWorkspaceError),
    /// The caller-owned restricted preparation operation failed.
    Preparation(PreparationError),
}

impl<PreparationError> fmt::Display for PrepareRepositoryWorkspaceError<PreparationError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Storage(_) => "runner repository workspace storage failed",
            Self::Preparation(_) => "runner repository workspace preparation failed",
        })
    }
}

impl<PreparationError> Error for PrepareRepositoryWorkspaceError<PreparationError>
where
    PreparationError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Preparation(error) => Some(error),
        }
    }
}

/// Sanitized managed-workspace storage failure.
#[derive(Debug)]
pub enum RunnerWorkspaceError {
    /// A descriptor-relative filesystem operation failed.
    Io(io::Error),
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
            Self::ManifestConflict | Self::CorruptManifest | Self::ManifestTooLarge => None,
        }
    }
}

/// Descriptor-pinned managed-workspace store sharing the runner-root lock.
#[derive(Debug)]
pub struct RunnerWorkspaceStore {
    root: File,
    canonical_root: PathBuf,
}

impl RunnerWorkspaceStore {
    pub(crate) const fn from_root(root: File, canonical_root: PathBuf) -> Self {
        Self {
            root,
            canonical_root,
        }
    }

    /// Creates and publishes one private root, or replays its exact ready manifest.
    pub fn prepare_private_root(
        &self,
        request: &PrivateWorkspaceRequest,
    ) -> Result<ReadyManifest, RunnerWorkspaceError> {
        let sessions = open_or_create_directory(&self.root, SESSIONS_DIRECTORY)?;
        let session_name = request.session().to_string();
        let session = open_or_create_directory(&sessions, &session_name)?;
        let placement_name = request.placement_revision().get().to_string();
        let execution_path = self.canonical_root.join(request.relative_path());
        match open_directory(&session, &placement_name) {
            Ok(placement) => read_ready_private_workspace(&placement, request, &execution_path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_workspace(&session, &placement_name, request, &execution_path)
            }
            Err(error) => Err(RunnerWorkspaceError::Io(error)),
        }
    }

    /// Prepares and atomically publishes one repository, or replays its ready manifest.
    pub async fn prepare_repository_workspace<Prepare, Preparation, PreparationError>(
        &self,
        request: &RepositoryWorkspaceRequest,
        prepare: Prepare,
    ) -> Result<ReadyManifest, PrepareRepositoryWorkspaceError<PreparationError>>
    where
        Prepare: FnOnce(RepositoryWorkspaceTarget) -> Preparation,
        Preparation: Future<Output = Result<Recovery, PreparationError>>,
    {
        let sessions = open_or_create_directory(&self.root, SESSIONS_DIRECTORY)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let session_name = request.session().to_string();
        let session = open_or_create_directory(&sessions, &session_name)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let placement_name = request.placement_revision().get().to_string();
        let execution_path = self.canonical_root.join(request.relative_path());
        match open_directory(&session, &placement_name) {
            Ok(placement) => {
                return read_ready_repository_workspace(&placement, request, &execution_path)
                    .map_err(PrepareRepositoryWorkspaceError::Storage);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PrepareRepositoryWorkspaceError::Storage(
                    RunnerWorkspaceError::Io(error),
                ));
            }
        }

        let manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
        let staging_name = format!(".{placement_name}-{manifest_id}.staging");
        let cleanup_parent = session
            .try_clone()
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        mkdirat(
            &session,
            staging_name.as_str(),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        )
        .map_err(rustix_io)
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let staging = open_created_directory(&session, &staging_name)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let mut staging =
            UnpublishedDirectory::new(cleanup_parent, OsString::from(&staging_name), staging);
        let repository = open_or_create_directory(
            staging
                .directory()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?,
            REPOSITORY_WORKSPACE_DIRECTORY,
        )
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let staging_execution_path = self
            .canonical_root
            .join(SESSIONS_DIRECTORY)
            .join(&session_name)
            .join(&staging_name)
            .join(REPOSITORY_WORKSPACE_DIRECTORY);
        let target_path = checked_execution_directory(&staging_execution_path, &repository)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let recovery = match prepare(RepositoryWorkspaceTarget {
            path: PathBuf::from(target_path.as_str()),
        })
        .await
        {
            Ok(recovery) => recovery,
            Err(error) => {
                staging
                    .cleanup()
                    .map_err(PrepareRepositoryWorkspaceError::Storage)?;
                return Err(PrepareRepositoryWorkspaceError::Preparation(error));
            }
        };
        checked_execution_directory(&staging_execution_path, &repository)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let mut manifest =
            repository_manifest(ManifestLifecycle::Staging, manifest_id, request, recovery);
        write_manifest(
            staging
                .directory()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?,
            &manifest,
        )
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        sync_directory_tree(&repository).map_err(PrepareRepositoryWorkspaceError::Storage)?;
        manifest.lifecycle = ManifestLifecycle::Ready;
        write_manifest(
            staging
                .directory()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?,
            &manifest,
        )
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        staging
            .directory()
            .map_err(PrepareRepositoryWorkspaceError::Storage)?
            .sync_all()
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(
            staging
                .directory()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?,
            REPOSITORY_WORKSPACE_DIRECTORY,
            &repository,
        )
        .map_err(RunnerWorkspaceError::Io)
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(
            &session,
            &staging_name,
            staging
                .directory()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?,
        )
        .map_err(RunnerWorkspaceError::Io)
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(&self.root, SESSIONS_DIRECTORY, &sessions)
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(&sessions, &session_name, &session)
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let publication = renameat_with(
            &session,
            staging_name.as_str(),
            &session,
            placement_name.as_str(),
            RenameFlags::NOREPLACE,
        );
        if let Err(error) = publication {
            if error != rustix::io::Errno::EXIST {
                return Err(PrepareRepositoryWorkspaceError::Storage(rustix_io(error)));
            }
            staging
                .cleanup()
                .map_err(PrepareRepositoryWorkspaceError::Storage)?;
            let placement = open_directory(&session, &placement_name)
                .map_err(RunnerWorkspaceError::Io)
                .map_err(PrepareRepositoryWorkspaceError::Storage)?;
            return read_ready_repository_workspace(&placement, request, &execution_path)
                .map_err(PrepareRepositoryWorkspaceError::Storage);
        }
        staging.disarm();
        session
            .sync_all()
            .map_err(RunnerWorkspaceError::CommitAmbiguous)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let placement = open_directory(&session, &placement_name)
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        read_ready_repository_workspace(&placement, request, &execution_path)
            .map_err(PrepareRepositoryWorkspaceError::Storage)
    }

    /// Deletes one exact private root only after its release is durably accepted.
    pub fn release_private_root(
        &self,
        accepted: &crate::AcceptedWorkspaceRelease,
    ) -> Result<(), RunnerWorkspaceError> {
        let correlation = accepted.correlation();
        let trash_name = correlation.manifest_id.to_string();
        let trash = open_optional_directory(&self.root, TRASH_DIRECTORY)?;
        let trashed = match trash.as_ref() {
            Some(trash) => open_optional_directory(trash, &trash_name)?,
            None => None,
        };
        let located = open_private_placement(&self.root, correlation)?;
        match (located, trashed) {
            (Some(_), Some(_)) => Err(RunnerWorkspaceError::ManifestConflict),
            (Some((session, placement)), None) => {
                release_published_private_workspace(&self.root, &session, placement, correlation)
            }
            (None, Some(placement)) => {
                let trash = trash.ok_or(RunnerWorkspaceError::ManifestConflict)?;
                finish_private_workspace_deletion(&trash, &trash_name, placement, correlation)
            }
            (None, None) => Ok(()),
        }
    }
}

fn repository_manifest(
    lifecycle: ManifestLifecycle,
    manifest_id: CanonicalUuid,
    request: &RepositoryWorkspaceRequest,
    recovery: Recovery,
) -> WorkspaceManifest {
    WorkspaceManifest {
        lifecycle,
        manifest_id,
        session: request.session(),
        placement_revision: request.placement_revision(),
        runner: request.runner(),
        repository: Some(request.repository().clone()),
        canonical_clone_url_digest: Some(request.canonical_clone_url_digest().clone()),
        credential_profile: request.credential_profile().cloned(),
        sandbox_profile: request.sandbox_profile(),
        relative_path: request.relative_path(),
        recovery: Some(recovery),
    }
}

fn read_ready_repository_workspace(
    placement: &File,
    request: &RepositoryWorkspaceRequest,
    execution_path: &Path,
) -> Result<ReadyManifest, RunnerWorkspaceError> {
    let manifest = read_manifest(placement)?;
    let recovery = manifest
        .recovery
        .clone()
        .ok_or(RunnerWorkspaceError::ManifestConflict)?;
    let expected = repository_manifest(
        ManifestLifecycle::Ready,
        manifest.manifest_id,
        request,
        recovery,
    );
    if manifest != expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let repository = open_directory(placement, REPOSITORY_WORKSPACE_DIRECTORY)
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    let execution_directory = checked_execution_directory(execution_path, &repository)?;
    let manifest_digest =
        workspace_manifest_digest(&manifest).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    Ok(ReadyManifest {
        manifest,
        manifest_digest,
        execution_directory,
    })
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
    execution_path: &Path,
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
    if let Err(error) = renameat_with(
        session,
        staging_name.as_str(),
        session,
        placement_name,
        RenameFlags::NOREPLACE,
    ) {
        return Err(RunnerWorkspaceError::Io(io::Error::from_raw_os_error(
            error.raw_os_error(),
        )));
    }
    session
        .sync_all()
        .map_err(RunnerWorkspaceError::CommitAmbiguous)?;
    let placement = open_directory(session, placement_name).map_err(RunnerWorkspaceError::Io)?;
    read_ready_private_workspace(&placement, request, execution_path)
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
    execution_path: &Path,
) -> Result<ReadyManifest, RunnerWorkspaceError> {
    let manifest = read_manifest(placement)?;
    let expected = private_manifest(ManifestLifecycle::Ready, manifest.manifest_id, request);
    if manifest != expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let work = open_directory(placement, PRIVATE_WORKSPACE_DIRECTORY)
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    let execution_directory = checked_execution_directory(execution_path, &work)?;
    let manifest_digest =
        workspace_manifest_digest(&manifest).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    Ok(ReadyManifest {
        manifest,
        manifest_digest,
        execution_directory,
    })
}

fn checked_execution_directory(
    path: &Path,
    directory: &File,
) -> Result<WorkingDirectory, RunnerWorkspaceError> {
    let canonical = std::fs::canonicalize(path).map_err(RunnerWorkspaceError::Io)?;
    let path_metadata = std::fs::metadata(&canonical).map_err(RunnerWorkspaceError::Io)?;
    let descriptor_metadata = directory.metadata().map_err(RunnerWorkspaceError::Io)?;
    if path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
    {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let text = canonical
        .to_str()
        .ok_or(RunnerWorkspaceError::CorruptManifest)?;
    WorkingDirectory::try_new(text.to_owned()).map_err(|_| RunnerWorkspaceError::CorruptManifest)
}

fn open_private_placement(
    root: &File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<Option<(File, File)>, RunnerWorkspaceError> {
    let Some(sessions) = open_optional_directory(root, SESSIONS_DIRECTORY)? else {
        return Ok(None);
    };
    let session_name = correlation.session_id.to_string();
    let Some(session) = open_optional_directory(&sessions, &session_name)? else {
        return Ok(None);
    };
    let placement_name = correlation.placement_revision.get().to_string();
    let placement = open_optional_directory(&session, &placement_name)?;
    Ok(placement.map(|placement| (session, placement)))
}

fn release_published_private_workspace(
    root: &File,
    session: &File,
    placement: File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<(), RunnerWorkspaceError> {
    let mut manifest = read_private_release_manifest(&placement, correlation)?;
    manifest.lifecycle = ManifestLifecycle::Releasing;
    write_manifest(&placement, &manifest)?;
    let placement_name = correlation.placement_revision.get().to_string();
    if !path_names_directory(session, &placement_name, &placement)? {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let trash = open_or_create_directory(root, TRASH_DIRECTORY)?;
    let trash_name = correlation.manifest_id.to_string();
    renameat_with(
        session,
        placement_name.as_str(),
        &trash,
        trash_name.as_str(),
        RenameFlags::NOREPLACE,
    )
    .map_err(rustix_io)?;
    session
        .sync_all()
        .and_then(|()| trash.sync_all())
        .map_err(RunnerWorkspaceError::CommitAmbiguous)?;
    if !path_names_directory(&trash, &trash_name, &placement)? {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    finish_private_workspace_deletion(&trash, &trash_name, placement, correlation)
}

fn finish_private_workspace_deletion(
    trash: &File,
    trash_name: &str,
    placement: File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<(), RunnerWorkspaceError> {
    let manifest = read_private_release_manifest(&placement, correlation)?;
    if manifest.lifecycle != ManifestLifecycle::Releasing
        || !path_names_directory(trash, trash_name, &placement)?
    {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    remove_open_directory_tree(trash, OsStr::new(trash_name), placement)?;
    trash
        .sync_all()
        .map_err(RunnerWorkspaceError::CommitAmbiguous)
}

fn read_private_release_manifest(
    placement: &File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<WorkspaceManifest, RunnerWorkspaceError> {
    let manifest = read_manifest(placement)?;
    if !matches!(
        manifest.lifecycle,
        ManifestLifecycle::Ready | ManifestLifecycle::Active | ManifestLifecycle::Releasing
    ) {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let request = PrivateWorkspaceRequest::new(
        correlation.session_id,
        correlation.placement_revision,
        correlation.runner_id,
        manifest.sandbox_profile,
    );
    let expected = private_manifest(manifest.lifecycle, correlation.manifest_id, &request);
    if manifest != expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    Ok(manifest)
}

fn open_or_create_directory(parent: &File, name: &str) -> Result<File, RunnerWorkspaceError> {
    match open_directory(parent, name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_or_open_directory(parent, name)
        }
        Err(error) => Err(RunnerWorkspaceError::Io(error)),
    }
}

fn create_or_open_directory(parent: &File, name: &str) -> Result<File, RunnerWorkspaceError> {
    let directory = match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_created_directory(parent, name)?,
        Err(error) if error == rustix::io::Errno::EXIST => {
            open_directory(parent, name).map_err(RunnerWorkspaceError::Io)?
        }
        Err(error) => return Err(rustix_io(error)),
    };
    parent.sync_all().map_err(RunnerWorkspaceError::Io)?;
    Ok(directory)
}

fn open_optional_directory(
    parent: &File,
    name: &str,
) -> Result<Option<File>, RunnerWorkspaceError> {
    match open_directory(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
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
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

struct UnpublishedDirectory {
    parent: File,
    name: OsString,
    directory: Option<File>,
}

impl UnpublishedDirectory {
    fn new(parent: File, name: OsString, directory: File) -> Self {
        Self {
            parent,
            name,
            directory: Some(directory),
        }
    }

    fn directory(&self) -> Result<&File, RunnerWorkspaceError> {
        self.directory
            .as_ref()
            .ok_or(RunnerWorkspaceError::ManifestConflict)
    }

    fn cleanup(mut self) -> Result<(), RunnerWorkspaceError> {
        self.remove()?;
        self.parent
            .sync_all()
            .map_err(RunnerWorkspaceError::CommitAmbiguous)
    }

    fn disarm(&mut self) {
        self.directory = None;
    }

    fn remove(&mut self) -> Result<(), RunnerWorkspaceError> {
        let directory = self
            .directory
            .take()
            .ok_or(RunnerWorkspaceError::ManifestConflict)?;
        remove_open_directory_tree(&self.parent, &self.name, directory)
    }
}

impl Drop for UnpublishedDirectory {
    fn drop(&mut self) {
        if self.directory.is_some() {
            let _ = self.remove();
            let _ = self.parent.sync_all();
        }
    }
}

impl DirectoryIdentity {
    fn from_file(directory: &File) -> Result<Self, RunnerWorkspaceError> {
        let metadata = directory.metadata().map_err(RunnerWorkspaceError::Io)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn names(self, parent: &File, name: &OsStr) -> Result<bool, RunnerWorkspaceError> {
        let status = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(rustix_io)?;
        Ok(self.device == status.st_dev && self.inode == status.st_ino)
    }
}

enum DurabilityStep {
    Inspect { parent: Rc<File>, name: OsString },
    SyncDirectory(Rc<File>),
}

fn sync_directory_tree(directory: &File) -> Result<(), RunnerWorkspaceError> {
    let root = Rc::new(directory.try_clone().map_err(RunnerWorkspaceError::Io)?);
    let mut steps = vec![DurabilityStep::SyncDirectory(Rc::clone(&root))];
    push_durability_entries(&mut steps, root)?;
    while let Some(step) = steps.pop() {
        match step {
            DurabilityStep::Inspect { parent, name } => {
                let status =
                    statat(parent.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW).map_err(rustix_io)?;
                let file_type = FileType::from_raw_mode(status.st_mode);
                if file_type == FileType::Directory {
                    let descriptor = openat(
                        parent.as_ref(),
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(rustix_io)?;
                    let child = Rc::new(File::from(descriptor));
                    if !DirectoryIdentity::from_file(&child)?.names(parent.as_ref(), &name)? {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    steps.push(DurabilityStep::SyncDirectory(Rc::clone(&child)));
                    push_durability_entries(&mut steps, child)?;
                } else if file_type == FileType::RegularFile {
                    let descriptor = openat(
                        parent.as_ref(),
                        &name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(rustix_io)?;
                    let file = File::from(descriptor);
                    let metadata = file.metadata().map_err(RunnerWorkspaceError::Io)?;
                    if metadata.dev() != status.st_dev || metadata.ino() != status.st_ino {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    file.sync_all().map_err(RunnerWorkspaceError::Io)?;
                } else if file_type != FileType::Symlink {
                    return Err(RunnerWorkspaceError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "prepared repository contains an unsupported file type",
                    )));
                }
            }
            DurabilityStep::SyncDirectory(directory) => {
                directory.sync_all().map_err(RunnerWorkspaceError::Io)?;
            }
        }
    }
    Ok(())
}

fn push_durability_entries(
    steps: &mut Vec<DurabilityStep>,
    directory: Rc<File>,
) -> Result<(), RunnerWorkspaceError> {
    let mut entries = Dir::read_from(directory.as_ref()).map_err(rustix_io)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(rustix_io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == OsStr::new(".") || name == OsStr::new("..") {
            continue;
        }
        steps.push(DurabilityStep::Inspect {
            parent: Rc::clone(&directory),
            name: OsString::from_vec(name.as_bytes().to_vec()),
        });
    }
    Ok(())
}

enum RemovalStep {
    Inspect {
        parent: Rc<File>,
        name: OsString,
    },
    RemoveDirectory {
        parent: Rc<File>,
        name: OsString,
        identity: DirectoryIdentity,
    },
}

fn remove_open_directory_tree(
    parent: &File,
    name: &OsStr,
    directory: File,
) -> Result<(), RunnerWorkspaceError> {
    let identity = DirectoryIdentity::from_file(&directory)?;
    let parent = Rc::new(parent.try_clone().map_err(RunnerWorkspaceError::Io)?);
    let directory = Rc::new(directory);
    let mut steps = vec![RemovalStep::RemoveDirectory {
        parent,
        name: name.to_owned(),
        identity,
    }];
    push_directory_entries(&mut steps, Rc::clone(&directory))?;
    while let Some(step) = steps.pop() {
        match step {
            RemovalStep::Inspect { parent, name } => {
                let status =
                    statat(parent.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW).map_err(rustix_io)?;
                if FileType::from_raw_mode(status.st_mode) == FileType::Directory {
                    let descriptor = openat(
                        parent.as_ref(),
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(rustix_io)?;
                    let child = Rc::new(File::from(descriptor));
                    let identity = DirectoryIdentity::from_file(&child)?;
                    if !identity.names(parent.as_ref(), &name)? {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    steps.push(RemovalStep::RemoveDirectory {
                        parent,
                        name,
                        identity,
                    });
                    push_directory_entries(&mut steps, child)?;
                } else {
                    unlinkat(parent.as_ref(), &name, AtFlags::empty()).map_err(rustix_io)?;
                }
            }
            RemovalStep::RemoveDirectory {
                parent,
                name,
                identity,
            } => {
                if !identity.names(parent.as_ref(), &name)? {
                    return Err(RunnerWorkspaceError::ManifestConflict);
                }
                unlinkat(parent.as_ref(), &name, AtFlags::REMOVEDIR).map_err(rustix_io)?;
            }
        }
    }
    Ok(())
}

fn push_directory_entries(
    steps: &mut Vec<RemovalStep>,
    directory: Rc<File>,
) -> Result<(), RunnerWorkspaceError> {
    let mut entries = Dir::read_from(directory.as_ref()).map_err(rustix_io)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(rustix_io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == OsStr::new(".") || name == OsStr::new("..") {
            continue;
        }
        steps.push(RemovalStep::Inspect {
            parent: Rc::clone(&directory),
            name: OsString::from_vec(name.as_bytes().to_vec()),
        });
    }
    Ok(())
}

fn read_manifest(directory: &File) -> Result<WorkspaceManifest, RunnerWorkspaceError> {
    let descriptor = openat(
        directory,
        MANIFEST_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
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
    directory
        .sync_all()
        .map_err(RunnerWorkspaceError::CommitAmbiguous)
}

fn rustix_io(error: rustix::io::Errno) -> RunnerWorkspaceError {
    RunnerWorkspaceError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
}

#[cfg(test)]
mod tests {
    use std::{fs, future, os::unix::fs::PermissionsExt as _, sync::Arc};

    use signalbox_runner_wire::{
        Advertisement, CanonicalUuid, ManifestLifecycle, PositiveU64, ProfileName, Recovery,
        ReleaseCorrelation, ReleasePhase, RepositoryKey, SandboxProfile, WorkspaceOperation,
        advertisement_digest, clone_url_digest, workspace_manifest_digest,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{EnrollmentAuthority, EnrollmentReceipt, RunnerStateRoot};

    use super::{
        DOCUMENT_MODE, MANIFEST_FILE, PrepareRepositoryWorkspaceError, PrivateWorkspaceRequest,
        RepositoryWorkspaceRequest, RunnerWorkspaceError, TRASH_DIRECTORY,
    };

    const SESSION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e1;
    const RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e2;
    const OTHER_RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e3;
    const OTHER_MANIFEST: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e6;
    const PLACEMENT_REVISION: u64 = 3;
    const ENROLLMENT: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e4;
    const AUTHENTICATION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e5;
    const OPEN_DIRECTORY_MODE: u32 = 0o750;
    const EXPECTED_RELATIVE_PATH: &str = "sessions/018f6f10-0000-7000-8000-0000000000e1/3/work";
    const CLONE_URL: &str = "https://github.com/KeenWill/signalbox.git";

    fn fixture_root() -> (TempDir, RunnerStateRoot) {
        let parent = tempfile::tempdir().expect("the workspace fixture parent exists");
        let root = RunnerStateRoot::open(&parent.path().join("runner-state"))
            .expect("the owner-private runner root opens");
        (parent, root)
    }

    fn enrolled_fixture_root() -> (TempDir, RunnerStateRoot) {
        let (parent, mut root) = fixture_root();
        let advertisement = Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: Vec::new(),
            repositories: Vec::new(),
        };
        let receipt = EnrollmentReceipt::new(
            root.state().request_id(),
            CanonicalUuid::from_uuid(Uuid::from_u128(ENROLLMENT)),
            CanonicalUuid::from_uuid(Uuid::from_u128(RUNNER)),
            CanonicalUuid::from_uuid(Uuid::from_u128(AUTHENTICATION)),
            PositiveU64::try_new(1).expect("the fixture registration revision is positive"),
            advertisement_digest(&advertisement)
                .expect("the explicit empty advertisement has a digest"),
            EnrollmentAuthority::Active,
        );
        root.record_receipt(receipt)
            .expect("the fixture enrollment receipt is durable");
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
    ) -> signalbox_runner_wire::ReadyManifest {
        publish_repository_request(state, &repository_request(), recovery).await
    }

    async fn publish_repository_request(
        state: &RunnerStateRoot,
        request: &RepositoryWorkspaceRequest,
        recovery: Recovery,
    ) -> signalbox_runner_wire::ReadyManifest {
        state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(request, |target| async move {
                fs::write(target.path().join("prepared"), b"repository\n")?;
                Ok::<Recovery, std::io::Error>(recovery)
            })
            .await
            .expect("the repository workspace publishes")
    }

    fn release_correlation(prepared: &signalbox_runner_wire::ReadyManifest) -> ReleaseCorrelation {
        ReleaseCorrelation {
            session_id: prepared.manifest.session,
            placement_revision: prepared.manifest.placement_revision,
            runner_id: prepared.manifest.runner,
            manifest_id: prepared.manifest.manifest_id,
        }
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
            b"repository\n"
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
        let prepared =
            publish_repository_request(&state, &request, expected_recovery.clone()).await;

        assert_eq!(prepared.manifest.recovery, Some(expected_recovery));
        assert!(prepared.manifest.credential_profile.is_none());
    }

    #[tokio::test]
    async fn repository_workspace_preparation_failure_never_publishes_the_placement() {
        let (parent, state) = fixture_root();
        let expected = repository_request();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&expected, |target| async move {
                fs::write(target.path().join("partial"), b"partial repository\n")?;
                Err::<Recovery, std::io::Error>(std::io::Error::other(
                    "the fixture preparation fails",
                ))
            })
            .await
            .expect_err("a failed preparation cannot publish a workspace");
        let placement = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string())
            .join(expected.placement_revision().get().to_string());
        let session = placement
            .parent()
            .expect("the placement fixture has a session parent");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Preparation(_)
        ));
        assert!(!placement.exists());
        assert_eq!(
            fs::read_dir(session)
                .expect("the session remains readable after failed preparation")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn repository_workspace_cancellation_removes_unpublished_staging() {
        let (parent, state) = fixture_root();
        let expected = repository_request();
        let session = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string());
        let (started, preparation_started) = tokio::sync::oneshot::channel();
        let preparation = tokio::spawn(async move {
            state
                .workspace_store()
                .expect("the locked root forms a workspace store")
                .prepare_repository_workspace(&expected, |target| async move {
                    fs::write(target.path().join("partial"), b"partial repository\n")?;
                    let _ = started.send(());
                    future::pending::<Result<Recovery, std::io::Error>>().await
                })
                .await
        });
        preparation_started
            .await
            .expect("repository preparation reaches its pending operation");

        preparation.abort();
        let cancellation = preparation
            .await
            .expect_err("the repository preparation task is cancelled");

        assert!(cancellation.is_cancelled());
        assert_eq!(
            fs::read_dir(session)
                .expect("the session remains readable after cancellation")
                .count(),
            0
        );
    }

    #[test]
    fn directory_creation_reopens_a_concurrent_winner() {
        let parent = tempfile::tempdir().expect("the directory race fixture parent exists");
        let winner_name = "winner";
        let winner = parent.path().join(winner_name);
        fs::create_dir(&winner).expect("the concurrent winner creates the directory");
        fs::set_permissions(&winner, fs::Permissions::from_mode(0o700))
            .expect("the concurrent winner directory is owner-private");
        let parent_descriptor =
            fs::File::open(parent.path()).expect("the directory race fixture parent opens");

        let reopened = super::create_or_open_directory(&parent_descriptor, winner_name)
            .expect("the concurrent winner is reopened and validated");

        assert!(
            super::path_names_directory(&parent_descriptor, winner_name, &reopened)
                .expect("the reopened winner retains its exact directory identity")
        );
    }

    #[tokio::test]
    async fn repository_workspace_rejects_unchecked_recovery_before_publication() {
        let (parent, state) = fixture_root();
        let expected = repository_request();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&expected, |_| async {
                Ok::<Recovery, std::io::Error>(Recovery::UnbornBranch {
                    name: "..".to_owned(),
                })
            })
            .await
            .expect_err("an unchecked recovery value cannot publish a workspace");
        let placement = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string())
            .join(expected.placement_revision().get().to_string());
        let session = placement
            .parent()
            .expect("the placement fixture has a session parent");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::CorruptManifest)
        ));
        assert!(!placement.exists());
        assert_eq!(
            fs::read_dir(session)
                .expect("the session remains readable after rejected recovery")
                .count(),
            0
        );
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

    #[tokio::test]
    async fn repository_workspace_rechecks_the_staging_directory_identity_before_publish() {
        let (_parent, state) = fixture_root();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&repository_request(), |target| async move {
                let replacement = target.path().with_file_name("replacement-repo");
                fs::rename(target.path(), &replacement)?;
                fs::create_dir(target.path())?;
                fs::set_permissions(target.path(), fs::Permissions::from_mode(0o700))?;
                Ok::<Recovery, std::io::Error>(Recovery::Branch {
                    name: "main".to_owned(),
                    revision: "d".repeat(40),
                })
            })
            .await
            .expect_err("a replaced staging path cannot be published");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::ManifestConflict)
        ));
    }

    #[tokio::test]
    async fn repository_workspace_rechecks_permissions_before_publish() {
        let (_parent, state) = fixture_root();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&repository_request(), |target| async move {
                let staging = target
                    .path()
                    .parent()
                    .expect("the repository fixture has a staging parent");
                fs::set_permissions(staging, fs::Permissions::from_mode(OPEN_DIRECTORY_MODE))?;
                Ok::<Recovery, std::io::Error>(Recovery::Commit {
                    revision: "e".repeat(40),
                })
            })
            .await
            .expect_err("an open staging directory cannot be published");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::Io(_))
        ));
    }

    #[tokio::test]
    async fn repository_workspace_rechecks_session_permissions_before_publish() {
        let (_parent, state) = fixture_root();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&repository_request(), |target| async move {
                let session = target
                    .path()
                    .parent()
                    .and_then(std::path::Path::parent)
                    .expect("the repository fixture has a session ancestor");
                fs::set_permissions(session, fs::Permissions::from_mode(OPEN_DIRECTORY_MODE))?;
                Ok::<Recovery, std::io::Error>(Recovery::Commit {
                    revision: "1".repeat(40),
                })
            })
            .await
            .expect_err("an open session directory cannot publish a repository");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::Io(_))
        ));
    }

    #[tokio::test]
    async fn repository_workspace_rechecks_sessions_permissions_before_publish() {
        let (_parent, state) = fixture_root();
        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_repository_workspace(&repository_request(), |target| async move {
                let sessions = target
                    .path()
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::parent)
                    .expect("the repository fixture has a sessions ancestor");
                fs::set_permissions(sessions, fs::Permissions::from_mode(OPEN_DIRECTORY_MODE))?;
                Ok::<Recovery, std::io::Error>(Recovery::Commit {
                    revision: "2".repeat(40),
                })
            })
            .await
            .expect_err("an open sessions directory cannot publish a repository");

        assert!(matches!(
            failure,
            PrepareRepositoryWorkspaceError::Storage(RunnerWorkspaceError::Io(_))
        ));
    }

    #[test]
    fn prepared_repository_durability_walks_nested_files_and_symlinks() {
        let repository = tempfile::tempdir().expect("the repository fixture exists");
        let nested = repository.path().join("objects").join("pack");
        fs::create_dir_all(&nested).expect("the nested repository fixture exists");
        fs::write(nested.join("pack"), b"prepared repository\n")
            .expect("the nested repository file exists");
        std::os::unix::fs::symlink("objects/pack/pack", repository.path().join("HEAD"))
            .expect("the repository symlink fixture exists");
        let descriptor =
            fs::File::open(repository.path()).expect("the repository fixture descriptor opens");

        super::sync_directory_tree(&descriptor)
            .expect("every prepared repository level becomes durable");
    }

    #[tokio::test]
    async fn repository_workspace_concurrent_publication_replays_the_winner() {
        let (parent, state) = fixture_root();
        let expected = repository_request();
        let first_store = state
            .workspace_store()
            .expect("the locked root forms the first workspace store");
        let second_store = state
            .workspace_store()
            .expect("the locked root forms the second workspace store");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let first = first_store.prepare_repository_workspace(&expected, move |target| async move {
            fs::write(target.path().join("prepared"), b"first\n")?;
            first_barrier.wait().await;
            Ok::<Recovery, std::io::Error>(Recovery::Commit {
                revision: "f".repeat(40),
            })
        });
        let second =
            second_store.prepare_repository_workspace(&expected, move |target| async move {
                fs::write(target.path().join("prepared"), b"second\n")?;
                second_barrier.wait().await;
                Ok::<Recovery, std::io::Error>(Recovery::Commit {
                    revision: "0".repeat(40),
                })
            });
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("the first publication reaches the ready workspace");
        let second = second.expect("the concurrent publication replays the ready workspace");
        let session = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string());

        assert_eq!(second, first);
        assert_eq!(
            fs::read_dir(session)
                .expect("the session directory remains readable")
                .count(),
            1
        );
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
    fn accepted_private_root_release_deletes_without_following_symlinks() {
        let (parent, mut state) = enrolled_fixture_root();
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
        let outside = parent.path().join("outside");
        fs::write(&outside, b"retained\n").expect("the outside fixture exists");
        fs::create_dir(placement.join("work").join("nested"))
            .expect("the nested workspace fixture exists");
        fs::write(
            placement.join("work").join("nested").join("owned"),
            b"owned\n",
        )
        .expect("the owned workspace fixture exists");
        std::os::unix::fs::symlink(&outside, placement.join("work").join("outside-link"))
            .expect("the workspace symlink fixture exists");
        let correlation = release_correlation(&prepared);
        let accepted = state
            .accept_workspace_release(correlation.clone())
            .expect("the release is journaled before deletion");

        state
            .workspace_store()
            .expect("the locked root forms a cleanup store")
            .release_private_root(&accepted)
            .expect("the private workspace is deleted");
        state
            .record_workspace_release_phase(correlation.clone(), ReleasePhase::ReleaseCompleted)
            .expect("deletion completion is durable");

        assert!(!placement.exists());
        assert_eq!(
            fs::read(&outside).expect("the outside file survives cleanup"),
            b"retained\n"
        );
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation,
                phase: ReleasePhase::ReleaseCompleted,
            })
        );
    }

    #[test]
    fn accepted_private_root_release_replays_after_deletion() {
        let (_parent, mut state) = enrolled_fixture_root();
        let prepared = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect("the private workspace publishes");
        let accepted = state
            .accept_workspace_release(release_correlation(&prepared))
            .expect("the release is journaled before deletion");
        let store = state
            .workspace_store()
            .expect("the locked root forms a cleanup store");

        store
            .release_private_root(&accepted)
            .expect("the private workspace is deleted");
        store
            .release_private_root(&accepted)
            .expect("absence replays the completed deletion");
    }

    #[test]
    fn accepted_private_root_release_rejects_another_manifest_identity() {
        let (parent, mut state) = enrolled_fixture_root();
        let expected = request(RUNNER);
        let prepared = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&expected)
            .expect("the private workspace publishes");
        let mut correlation = release_correlation(&prepared);
        correlation.manifest_id = CanonicalUuid::from_uuid(Uuid::from_u128(OTHER_MANIFEST));
        let accepted = state
            .accept_workspace_release(correlation)
            .expect("the mismatched release fixture is durably accepted");
        let failure = state
            .workspace_store()
            .expect("the locked root forms a cleanup store")
            .release_private_root(&accepted)
            .expect_err("another manifest identity cannot delete the workspace");
        let placement = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string())
            .join(expected.placement_revision().get().to_string());

        assert!(matches!(failure, RunnerWorkspaceError::ManifestConflict));
        assert!(placement.is_dir());
    }

    #[test]
    fn accepted_private_root_release_resumes_from_the_releasing_trash_entry() {
        let (parent, mut state) = enrolled_fixture_root();
        let expected = request(RUNNER);
        let prepared = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&expected)
            .expect("the private workspace publishes");
        let correlation = release_correlation(&prepared);
        let accepted = state
            .accept_workspace_release(correlation.clone())
            .expect("the release is journaled before deletion");
        let session = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string());
        let placement = session.join(expected.placement_revision().get().to_string());
        let trash = parent.path().join("runner-state").join(TRASH_DIRECTORY);
        fs::create_dir(&trash).expect("the owner-private trash fixture exists");
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o700))
            .expect("the trash fixture is owner-private");
        let placement_descriptor = super::open_directory(
            &fs::File::open(&session).expect("the session fixture opens"),
            &expected.placement_revision().get().to_string(),
        )
        .expect("the placement fixture opens");
        let mut manifest = super::read_manifest(&placement_descriptor)
            .expect("the ready manifest fixture is readable");
        manifest.lifecycle = ManifestLifecycle::Releasing;
        super::write_manifest(&placement_descriptor, &manifest)
            .expect("the releasing manifest fixture is durable");
        let trashed = trash.join(correlation.manifest_id.to_string());
        fs::rename(&placement, &trashed).expect("the accepted release fixture reaches trash");

        state
            .workspace_store()
            .expect("the locked root forms a cleanup store")
            .release_private_root(&accepted)
            .expect("the accepted trash deletion resumes");

        assert!(!trashed.exists());
    }
}
