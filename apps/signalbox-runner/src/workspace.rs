//! Descriptor-relative managed workspace storage below the locked runner root.

pub(crate) mod leaks;
pub(crate) mod provision;
pub(crate) mod release;

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
    CanonicalUuid, Digest, ManifestLifecycle, PositiveU64, ProfileName, Recovery, RepositoryKey,
    SandboxProfile, WorkingDirectory, WorkspaceManifest, workspace_manifest_digest,
};
use uuid::Uuid;

const DIRECTORY_MODE: u32 = 0o700;
const DOCUMENT_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const MANIFEST_DOCUMENT_VERSION: u64 = 1;
const MANIFEST_FILE: &str = "workspace-manifest.json";
const MAXIMUM_MANIFEST_BYTES: u64 = signalbox_runner_wire::MAX_FRAME_BYTES as u64;
const SESSIONS_DIRECTORY: &str = "sessions";
const PRIVATE_WORKSPACE_DIRECTORY: &str = "work";
const REPOSITORY_WORKSPACE_DIRECTORY: &str = "repo";

/// Complete durable facts needed to publish one repository workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryWorkspaceRequest {
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
    pub(crate) const fn new(
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
    pub(crate) const fn session(&self) -> CanonicalUuid {
        self.session
    }

    /// Returns the positive placement revision.
    pub(crate) const fn placement_revision(&self) -> PositiveU64 {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub(crate) const fn runner(&self) -> CanonicalUuid {
        self.runner
    }

    /// Borrows the exact configured repository key.
    pub(crate) const fn repository(&self) -> &RepositoryKey {
        &self.repository
    }

    /// Borrows the canonical configured clone-URL digest.
    pub(crate) const fn canonical_clone_url_digest(&self) -> &Digest {
        &self.canonical_clone_url_digest
    }

    /// Borrows the independently optional selected credential profile.
    pub(crate) const fn credential_profile(&self) -> Option<&ProfileName> {
        self.credential_profile.as_ref()
    }

    /// Returns the exact sandbox profile.
    pub(crate) const fn sandbox_profile(&self) -> SandboxProfile {
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
pub(crate) struct RepositoryWorkspaceTarget {
    path: PathBuf,
}

impl RepositoryWorkspaceTarget {
    /// Borrows the canonical staging directory mounted for repository preparation.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Failure before or after one caller-owned repository preparation operation.
#[derive(Debug)]
pub(crate) enum PrepareRepositoryWorkspaceError<PreparationError> {
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
pub(crate) enum RunnerWorkspaceError {
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

/// Complete runner-local workspace publication evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedWorkspace {
    pub(crate) manifest: WorkspaceManifest,
    pub(crate) manifest_digest: Digest,
    pub(crate) execution_directory: WorkingDirectory,
}

/// Descriptor-pinned managed-workspace store sharing the runner-root lock.
#[derive(Debug)]
pub(crate) struct RunnerWorkspaceStore {
    root: File,
    canonical_root: PathBuf,
    staging_cleanup: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl RunnerWorkspaceStore {
    pub(crate) fn from_root(
        root: File,
        canonical_root: PathBuf,
        staging_cleanup: std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            root,
            canonical_root,
            staging_cleanup,
        }
    }

    pub(crate) fn authenticate_active(
        &self,
        ready: &signalbox_runner_wire::WorkspaceReady,
    ) -> Result<DirectoryIdentity, RunnerWorkspaceError> {
        self.authenticate_published(ready, false)
    }

    pub(crate) fn authenticate_pending_ready(
        &self,
        ready: &signalbox_runner_wire::WorkspaceReady,
    ) -> Result<DirectoryIdentity, RunnerWorkspaceError> {
        self.authenticate_published(ready, true)
    }

    fn authenticate_published(
        &self,
        ready: &signalbox_runner_wire::WorkspaceReady,
        allow_ready: bool,
    ) -> Result<DirectoryIdentity, RunnerWorkspaceError> {
        validate_root_directory(&self.canonical_root, &self.root)
            .map_err(RunnerWorkspaceError::Io)?;
        let expected = &ready.ready.manifest;
        let sessions =
            open_directory(&self.root, SESSIONS_DIRECTORY).map_err(RunnerWorkspaceError::Io)?;
        let session = open_directory(&sessions, &expected.session.to_string())
            .map_err(RunnerWorkspaceError::Io)?;
        let placement = open_directory(&session, &expected.placement_revision.get().to_string())
            .map_err(RunnerWorkspaceError::Io)?;
        let mut manifest = read_manifest(&placement)?;
        if manifest.lifecycle != ManifestLifecycle::Active
            && !(allow_ready && manifest.lifecycle == ManifestLifecycle::Ready)
        {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
        manifest.lifecycle = ManifestLifecycle::Ready;
        if manifest != *expected
            || workspace_manifest_digest(&manifest)
                .map_err(|_| RunnerWorkspaceError::CorruptManifest)?
                != ready.ready.manifest_digest
        {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
        let leaf = if manifest.repository.is_some() {
            REPOSITORY_WORKSPACE_DIRECTORY
        } else {
            PRIVATE_WORKSPACE_DIRECTORY
        };
        let directory = open_directory(&placement, leaf).map_err(RunnerWorkspaceError::Io)?;
        let execution = checked_execution_directory(
            &self.canonical_root.join(&manifest.relative_path),
            &directory,
        )?;
        if execution.as_str() != ready.working_directory {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
        DirectoryIdentity::from_file(&directory)
    }

    pub(crate) fn activate(
        &self,
        prepared: &PreparedWorkspace,
    ) -> Result<(), RunnerWorkspaceError> {
        validate_root_directory(&self.canonical_root, &self.root)
            .map_err(RunnerWorkspaceError::Io)?;
        let sessions =
            open_directory(&self.root, SESSIONS_DIRECTORY).map_err(RunnerWorkspaceError::Io)?;
        let session = open_directory(&sessions, &prepared.manifest.session.to_string())
            .map_err(RunnerWorkspaceError::Io)?;
        let placement = open_directory(
            &session,
            &prepared.manifest.placement_revision.get().to_string(),
        )
        .map_err(RunnerWorkspaceError::Io)?;
        let mut current = read_manifest(&placement)?;
        let mut expected = prepared.manifest.clone();
        expected.lifecycle = current.lifecycle;
        if current != expected
            || !matches!(
                current.lifecycle,
                ManifestLifecycle::Ready | ManifestLifecycle::Active
            )
        {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
        if current.lifecycle == ManifestLifecycle::Ready {
            current.lifecycle = ManifestLifecycle::Active;
            write_manifest(&placement, &current)?;
        }
        Ok(())
    }

    /// Prepares and atomically publishes one repository, or replays its ready manifest.
    pub(crate) async fn prepare_repository_workspace<Prepare, Preparation, PreparationError>(
        &self,
        request: &RepositoryWorkspaceRequest,
        prepare: Prepare,
    ) -> Result<PreparedWorkspace, PrepareRepositoryWorkspaceError<PreparationError>>
    where
        Prepare: FnOnce(RepositoryWorkspaceTarget) -> Preparation,
        Preparation: Future<Output = Result<Recovery, PreparationError>>,
    {
        let staging_guard = self.staging_cleanup.clone().lock_owned().await;
        let sessions = open_or_create_directory(&self.root, SESSIONS_DIRECTORY)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let session_name = request.session().to_string();
        let session = open_or_create_directory(&sessions, &session_name)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let placement_name = request.placement_revision().get().to_string();
        let execution_path = self.canonical_root.join(request.relative_path());
        validate_execution_directory_representation(&execution_path)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        match open_directory(&session, &placement_name) {
            Ok(placement) => {
                validate_root_directory(&self.canonical_root, &self.root)
                    .map_err(RunnerWorkspaceError::Io)
                    .map_err(PrepareRepositoryWorkspaceError::Storage)?;
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
        mkdirat(
            &session,
            staging_name.as_str(),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        )
        .map_err(rustix_io)
        .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let staging = match open_created_directory(&session, &staging_name) {
            Ok(staging) => staging,
            Err(open_error) => {
                unlinkat(&session, staging_name.as_str(), AtFlags::REMOVEDIR)
                    .map_err(rustix_io)
                    .map_err(PrepareRepositoryWorkspaceError::Storage)?;
                session
                    .sync_all()
                    .map_err(RunnerWorkspaceError::CommitAmbiguous)
                    .map_err(PrepareRepositoryWorkspaceError::Storage)?;
                return Err(PrepareRepositoryWorkspaceError::Storage(open_error));
            }
        };
        let mut cleanup = StagingCleanup {
            parent: &session,
            name: &staging_name,
            directory: &staging,
            published: false,
            staging_guard: Some(staging_guard),
        };
        let repository = open_or_create_directory(&staging, REPOSITORY_WORKSPACE_DIRECTORY)
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
                return Err(PrepareRepositoryWorkspaceError::Preparation(error));
            }
        };
        checked_execution_directory(&staging_execution_path, &repository)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let mut manifest =
            repository_manifest(ManifestLifecycle::Staging, manifest_id, request, recovery);
        write_manifest(&staging, &manifest).map_err(PrepareRepositoryWorkspaceError::Storage)?;
        let durability_repository = repository
            .try_clone()
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        tokio::task::spawn_blocking(move || sync_directory_tree(&durability_repository))
            .await
            .map_err(|error| RunnerWorkspaceError::Io(io::Error::other(error)))
            .map_err(PrepareRepositoryWorkspaceError::Storage)?
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        manifest.lifecycle = ManifestLifecycle::Ready;
        write_manifest(&staging, &manifest).map_err(PrepareRepositoryWorkspaceError::Storage)?;
        staging
            .sync_all()
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(&staging, REPOSITORY_WORKSPACE_DIRECTORY, &repository)
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_directory(&session, &staging_name, &staging)
            .map_err(RunnerWorkspaceError::Io)
            .map_err(PrepareRepositoryWorkspaceError::Storage)?;
        validate_root_directory(&self.canonical_root, &self.root)
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
            let placement = open_directory(&session, &placement_name)
                .map_err(RunnerWorkspaceError::Io)
                .map_err(PrepareRepositoryWorkspaceError::Storage)?;
            return read_ready_repository_workspace(&placement, request, &execution_path)
                .map_err(PrepareRepositoryWorkspaceError::Storage);
        }
        cleanup.published = true;
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
}

struct StagingCleanup<'a> {
    parent: &'a File,
    name: &'a str,
    directory: &'a File,
    published: bool,
    staging_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for StagingCleanup<'_> {
    fn drop(&mut self) {
        if !self.published {
            let cleanup = self.parent.try_clone().and_then(|parent| {
                self.directory
                    .try_clone()
                    .map(|directory| (parent, directory))
            });
            match cleanup {
                Ok((parent, directory)) => {
                    let name = self.name.to_owned();
                    let staging_guard = self.staging_guard.take();
                    std::mem::drop(tokio::task::spawn_blocking(move || {
                        let _staging_guard = staging_guard;
                        if let Err(error) = release::finish_deletion(&parent, &name, directory) {
                            eprintln!("unpublished workspace staging cleanup failed: {error}");
                        }
                    }));
                }
                Err(error) => {
                    eprintln!("unpublished workspace staging cleanup failed: {error}");
                }
            }
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
) -> Result<PreparedWorkspace, RunnerWorkspaceError> {
    let mut manifest = read_manifest(placement)?;
    if manifest.lifecycle == ManifestLifecycle::Active {
        manifest.lifecycle = ManifestLifecycle::Ready;
    }
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
    Ok(PreparedWorkspace {
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

fn validate_execution_directory_representation(path: &Path) -> Result<(), RunnerWorkspaceError> {
    let text = path.to_str().ok_or(RunnerWorkspaceError::CorruptManifest)?;
    WorkingDirectory::try_new(text.to_owned())
        .map(|_| ())
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)
}

fn validate_root_directory(path: &Path, directory: &File) -> Result<(), io::Error> {
    let metadata = directory.metadata()?;
    let path_metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || !path_metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
        || metadata.dev() != path_metadata.dev()
        || metadata.ino() != path_metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace root identity is invalid",
        ));
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    inode: u64,
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
mod tests;
