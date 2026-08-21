//! Descriptor-relative managed workspace storage below the locked runner root.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
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
    CanonicalUuid, ManifestLifecycle, PositiveU64, ReadyManifest, SandboxProfile, WorkingDirectory,
    WorkspaceManifest, workspace_manifest_digest,
};
use uuid::Uuid;

const DIRECTORY_MODE: u32 = 0o700;
const DOCUMENT_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const MANIFEST_DOCUMENT_VERSION: u64 = 2;
const MANIFEST_FILE: &str = "workspace-manifest.json";
const MAXIMUM_MANIFEST_BYTES: u64 = 16 * 1024;
const SESSIONS_DIRECTORY: &str = "sessions";
const PRIVATE_WORKSPACE_DIRECTORY: &str = "work";
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    version: u64,
    manifest: WorkspaceManifest,
    execution_directory: WorkingDirectory,
}

fn create_private_workspace(
    session: &File,
    placement_name: &str,
    request: &PrivateWorkspaceRequest,
    execution_path: &Path,
) -> Result<ReadyManifest, RunnerWorkspaceError> {
    let execution_directory = represented_execution_directory(execution_path)?;
    let manifest_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let staging_name = format!(".{placement_name}-{manifest_id}.staging");
    mkdirat(
        session,
        staging_name.as_str(),
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .map_err(rustix_io)?;
    let staging = open_created_directory(session, &staging_name)?;
    let mut document = ManifestDocument {
        version: MANIFEST_DOCUMENT_VERSION,
        manifest: private_manifest(ManifestLifecycle::Staging, manifest_id, request),
        execution_directory: execution_directory.clone(),
    };
    write_manifest(&staging, &document)?;
    let work = open_or_create_directory(&staging, PRIVATE_WORKSPACE_DIRECTORY)?;
    work.sync_all().map_err(RunnerWorkspaceError::Io)?;
    document.manifest.lifecycle = ManifestLifecycle::Ready;
    write_manifest(&staging, &document)?;
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
    let placement =
        open_directory(session, placement_name).map_err(RunnerWorkspaceError::CommitAmbiguous)?;
    let published = read_ready_private_workspace(&placement, request, execution_path)
        .map_err(commit_ambiguous_after_publication)?;
    if published.execution_directory() != &execution_directory {
        return Err(commit_ambiguous_after_publication(
            RunnerWorkspaceError::ManifestConflict,
        ));
    }
    Ok(published)
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
    let document = read_manifest(placement)?;
    let manifest = document.manifest;
    let expected = private_manifest(ManifestLifecycle::Ready, manifest.manifest_id, request);
    if manifest != expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    let work = open_directory(placement, PRIVATE_WORKSPACE_DIRECTORY)
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    checked_execution_directory(execution_path, &work)
        .map_err(commit_ambiguous_after_publication)?;
    let manifest_digest =
        workspace_manifest_digest(&manifest).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    ReadyManifest::try_new(manifest, manifest_digest, document.execution_directory)
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)
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
    represented_execution_directory(path)
}

fn represented_execution_directory(path: &Path) -> Result<WorkingDirectory, RunnerWorkspaceError> {
    let text = path.to_str().ok_or(RunnerWorkspaceError::CorruptManifest)?;
    WorkingDirectory::try_new(text.to_owned()).map_err(|_| RunnerWorkspaceError::CorruptManifest)
}

fn commit_ambiguous_after_publication(error: RunnerWorkspaceError) -> RunnerWorkspaceError {
    let source = match error {
        RunnerWorkspaceError::Io(source) | RunnerWorkspaceError::CommitAmbiguous(source) => source,
        RunnerWorkspaceError::ManifestConflict => {
            io::Error::other("published workspace path no longer names its directory")
        }
        RunnerWorkspaceError::CorruptManifest => {
            io::Error::other("published workspace readback is corrupt")
        }
        RunnerWorkspaceError::ManifestTooLarge => {
            io::Error::other("published workspace manifest exceeds its byte bound")
        }
    };
    RunnerWorkspaceError::CommitAmbiguous(source)
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
    let mut document = read_private_release_manifest(&placement, correlation)?;
    document.manifest.lifecycle = ManifestLifecycle::Releasing;
    write_manifest(&placement, &document)?;
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
    let document = read_private_release_manifest(&placement, correlation)?;
    if document.manifest.lifecycle != ManifestLifecycle::Releasing
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
) -> Result<ManifestDocument, RunnerWorkspaceError> {
    let document = read_manifest(placement)?;
    let manifest = &document.manifest;
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
    if manifest != &expected {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    Ok(document)
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

fn read_manifest(directory: &File) -> Result<ManifestDocument, RunnerWorkspaceError> {
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
    Ok(document)
}

fn write_manifest(
    directory: &File,
    document: &ManifestDocument,
) -> Result<(), RunnerWorkspaceError> {
    document
        .manifest
        .validate()
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    if document.version != MANIFEST_DOCUMENT_VERSION {
        return Err(RunnerWorkspaceError::CorruptManifest);
    }
    let mut encoded =
        serde_json::to_vec(document).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
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
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use signalbox_runner_wire::{
        Advertisement, CanonicalUuid, ManifestLifecycle, PositiveU64, ReleaseCorrelation,
        ReleasePhase, SandboxProfile, WorkspaceOperation, advertisement_digest,
        workspace_manifest_digest,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{EnrollmentAuthority, EnrollmentReceipt, RunnerStateRoot};

    use super::{
        DOCUMENT_MODE, MANIFEST_FILE, PrivateWorkspaceRequest, RunnerWorkspaceError,
        TRASH_DIRECTORY,
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

    fn release_correlation(prepared: &signalbox_runner_wire::ReadyManifest) -> ReleaseCorrelation {
        ReleaseCorrelation {
            session_id: prepared.manifest().session,
            placement_revision: prepared.manifest().placement_revision,
            runner_id: prepared.manifest().runner,
            manifest_id: prepared.manifest().manifest_id,
        }
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

        assert_eq!(prepared.manifest().session, expected.session());
        assert_eq!(prepared.manifest().runner, expected.runner());
        assert_eq!(prepared.manifest().lifecycle, ManifestLifecycle::Ready);
        assert_eq!(prepared.manifest().relative_path, EXPECTED_RELATIVE_PATH);
        assert_eq!(
            prepared.execution_directory().as_str(),
            expected_execution_directory
                .to_str()
                .expect("the fixture path is UTF-8")
        );
        assert_eq!(
            prepared.manifest_digest(),
            &workspace_manifest_digest(prepared.manifest())
                .expect("the ready private manifest has its canonical digest")
        );
        assert!(prepared.manifest().repository.is_none());
        assert!(prepared.manifest().credential_profile.is_none());
        assert!(prepared.manifest().recovery.is_none());
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
    fn private_root_reopen_after_root_rename_preserves_the_authored_path() {
        let (parent, state) = fixture_root();
        let first = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect("the private workspace publishes");
        drop(state);
        let original_root = parent.path().join("runner-state");
        let moved_root = parent.path().join("moved-runner-state");
        fs::rename(&original_root, &moved_root).expect("the runner root moves between processes");
        let reopened = RunnerStateRoot::open(&moved_root)
            .expect("the runner root reopens at its new configured path");
        let replay = reopened
            .workspace_store()
            .expect("the reopened root forms a workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect("the moved private workspace replays");

        assert_eq!(replay, first);
    }

    #[test]
    fn private_root_namespace_loss_after_open_is_commit_ambiguous() {
        let (parent, state) = fixture_root();
        let original_root = parent.path().join("runner-state");
        let moved_root = parent.path().join("moved-runner-state");
        fs::rename(&original_root, &moved_root)
            .expect("the opened runner root is moved before publication");

        let failure = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect_err("post-commit namespace loss is not a definite failure");
        let published = moved_root
            .join("sessions")
            .join(request(RUNNER).session().to_string())
            .join(PLACEMENT_REVISION.to_string());

        assert!(matches!(failure, RunnerWorkspaceError::CommitAmbiguous(_)));
        assert!(published.is_dir());

        let replay_failure = state
            .workspace_store()
            .expect("the locked root forms another workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect_err("replay after namespace loss remains commit ambiguous");

        assert!(matches!(
            replay_failure,
            RunnerWorkspaceError::CommitAmbiguous(_)
        ));
        assert!(published.is_dir());
    }

    #[test]
    fn private_root_replay_preserves_authored_path_after_root_relink() {
        let (parent, state) = fixture_root();
        let first = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect("the private workspace publishes");
        let original_root = parent.path().join("runner-state");
        let moved_root = parent.path().join("moved-runner-state");
        fs::rename(&original_root, &moved_root)
            .expect("the opened runner root moves after publication");
        std::os::unix::fs::symlink(&moved_root, &original_root)
            .expect("the original runner-root path is relinked");

        let replay = state
            .workspace_store()
            .expect("the locked root forms another workspace store")
            .prepare_private_root(&request(RUNNER))
            .expect("the relinked private workspace replays");

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
        let mut document = super::read_manifest(&placement_descriptor)
            .expect("the ready manifest fixture is readable");
        document.manifest.lifecycle = ManifestLifecycle::Releasing;
        super::write_manifest(&placement_descriptor, &document)
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
