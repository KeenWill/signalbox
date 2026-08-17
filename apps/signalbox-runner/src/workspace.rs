//! Descriptor-relative managed workspace storage below the locked runner root.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, Read, Write},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    rc::Rc,
};

use rustix::{
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, chmodat, fchmod, mkdirat, openat,
        renameat, renameat_with, statat, unlinkat,
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
const REMOVAL_BATCH_SIZE: usize = 64;
const MAXIMUM_REMOVAL_DEPTH: usize = 256;
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

    /// Deletes one exact private root only after its release is durably accepted.
    pub fn release_private_root(
        &self,
        accepted: &crate::AcceptedWorkspaceRelease,
    ) -> Result<(), RunnerWorkspaceError> {
        let correlation = accepted.correlation();
        if !crate::state::accepted_workspace_release_is_current(&self.root, correlation)
            .map_err(|_| RunnerWorkspaceError::ManifestConflict)?
        {
            return Err(RunnerWorkspaceError::ManifestConflict);
        }
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
                let session = open_private_session(&self.root, correlation)?
                    .ok_or(RunnerWorkspaceError::ManifestConflict)?;
                session
                    .sync_all()
                    .map_err(RunnerWorkspaceError::CommitAmbiguous)?;
                finish_private_workspace_deletion(&trash, &trash_name, placement, correlation)
            }
            (None, None) => {
                if let Some(trash) = trash {
                    trash
                        .sync_all()
                        .map_err(RunnerWorkspaceError::CommitAmbiguous)?;
                }
                Ok(())
            }
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
    read_ready_private_workspace(&placement, request)
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

fn open_private_placement(
    root: &File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<Option<(File, File)>, RunnerWorkspaceError> {
    let Some(session) = open_private_session(root, correlation)? else {
        return Ok(None);
    };
    let placement_name = correlation.placement_revision.get().to_string();
    let placement = open_optional_directory(&session, &placement_name)?;
    Ok(placement.map(|placement| (session, placement)))
}

fn open_private_session(
    root: &File,
    correlation: &signalbox_runner_wire::ReleaseCorrelation,
) -> Result<Option<File>, RunnerWorkspaceError> {
    let Some(sessions) = open_optional_directory(root, SESSIONS_DIRECTORY)? else {
        return Ok(None);
    };
    let session_name = correlation.session_id.to_string();
    open_optional_directory(&sessions, &session_name)
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
    let manifest_present = match read_private_release_manifest(&placement, correlation) {
        Ok(manifest) if manifest.lifecycle == ManifestLifecycle::Releasing => true,
        Ok(_) => return Err(RunnerWorkspaceError::ManifestConflict),
        Err(RunnerWorkspaceError::Io(error)) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !path_names_directory(trash, trash_name, &placement)? {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    remove_open_directory_tree(trash, OsStr::new(trash_name), placement, manifest_present)?;
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
    chmodat(parent, name, Mode::RWXU, AtFlags::empty()).map_err(rustix_io)?;
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
        parent_path: RemovalPath,
        name: OsString,
    },
    ScanDirectory {
        path: RemovalPath,
        identity: DirectoryIdentity,
        preserve_manifest: bool,
    },
}

#[derive(Clone, Default)]
struct RemovalPath(Option<Rc<RemovalPathNode>>);

struct RemovalPathNode {
    parent: RemovalPath,
    component: OsString,
    depth: usize,
}

impl RemovalPath {
    fn pushed(&self, component: OsString) -> Result<Self, RunnerWorkspaceError> {
        let depth = self.0.as_deref().map_or(1, |node| node.depth + 1);
        if depth > MAXIMUM_REMOVAL_DEPTH {
            return Err(RunnerWorkspaceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace cleanup depth exceeds the supported maximum",
            )));
        }
        Ok(Self(Some(Rc::new(RemovalPathNode {
            parent: self.clone(),
            component,
            depth,
        }))))
    }

    fn split_last(&self) -> Option<(&OsStr, &Self)> {
        self.0
            .as_deref()
            .map(|node| (node.component.as_os_str(), &node.parent))
    }
}

fn remove_open_directory_tree(
    parent: &File,
    name: &OsStr,
    directory: File,
    preserve_manifest: bool,
) -> Result<(), RunnerWorkspaceError> {
    let identity = DirectoryIdentity::from_file(&directory)?;
    let mut steps = vec![RemovalStep::ScanDirectory {
        path: RemovalPath::default(),
        identity,
        preserve_manifest,
    }];
    while let Some(step) = steps.pop() {
        match step {
            RemovalStep::Inspect { parent_path, name } => {
                let parent_directory = open_removal_path(&directory, &parent_path)?;
                let status = statat(&parent_directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(rustix_io)?;
                if FileType::from_raw_mode(status.st_mode) == FileType::Directory {
                    let child = open_removal_directory(&parent_directory, &name)?;
                    let identity = DirectoryIdentity::from_file(&child)?;
                    steps.push(RemovalStep::ScanDirectory {
                        path: parent_path.pushed(name)?,
                        identity,
                        preserve_manifest: false,
                    });
                } else {
                    unlinkat(&parent_directory, &name, AtFlags::empty()).map_err(rustix_io)?;
                }
            }
            RemovalStep::ScanDirectory {
                path,
                identity,
                preserve_manifest,
            } => {
                let current = open_removal_path(&directory, &path)?;
                if DirectoryIdentity::from_file(&current)? != identity {
                    return Err(RunnerWorkspaceError::ManifestConflict);
                }
                let entries = read_directory_batch(&current, preserve_manifest)?;
                if entries.is_empty() {
                    if preserve_manifest {
                        unlinkat(&current, MANIFEST_FILE, AtFlags::empty()).map_err(rustix_io)?;
                    }
                    if let Some((child_name, parent_path)) = path.split_last() {
                        let parent_directory = open_removal_path(&directory, parent_path)?;
                        if !identity.names(&parent_directory, child_name)? {
                            return Err(RunnerWorkspaceError::ManifestConflict);
                        }
                        unlinkat(&parent_directory, child_name, AtFlags::REMOVEDIR)
                            .map_err(rustix_io)?;
                    } else {
                        if !identity.names(parent, name)? {
                            return Err(RunnerWorkspaceError::ManifestConflict);
                        }
                        unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(rustix_io)?;
                    }
                } else {
                    steps.push(RemovalStep::ScanDirectory {
                        path: path.clone(),
                        identity,
                        preserve_manifest,
                    });
                    steps.extend(entries.into_iter().map(|name| RemovalStep::Inspect {
                        parent_path: path.clone(),
                        name,
                    }));
                }
            }
        }
    }
    Ok(())
}

fn open_removal_path(root: &File, path: &RemovalPath) -> Result<File, RunnerWorkspaceError> {
    let mut reversed = Vec::new();
    let mut node = path.0.as_deref();
    while let Some(component) = node {
        reversed.push(component.component.as_os_str());
        node = component.parent.0.as_deref();
    }
    let mut current = root.try_clone().map_err(RunnerWorkspaceError::Io)?;
    for component in reversed.into_iter().rev() {
        current = open_removal_directory(&current, component)?;
    }
    Ok(current)
}

fn open_removal_directory(parent: &File, name: &OsStr) -> Result<File, RunnerWorkspaceError> {
    let pinned = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    let pinned = File::from(pinned);
    let pinned_identity = DirectoryIdentity::from_file(&pinned)?;
    let metadata = pinned.metadata().map_err(RunnerWorkspaceError::Io)?;
    if !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || !pinned_identity.names(parent, name)?
    {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    chmodat(
        parent,
        name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
        AtFlags::empty(),
    )
    .map_err(rustix_io)?;
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    fchmod(&descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(rustix_io)?;
    let directory = File::from(descriptor);
    let identity = DirectoryIdentity::from_file(&directory)?;
    if identity != pinned_identity || !identity.names(parent, name)? {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    Ok(directory)
}

fn read_directory_batch(
    directory: &File,
    preserve_manifest: bool,
) -> Result<Vec<OsString>, RunnerWorkspaceError> {
    let descriptor = openat(
        directory,
        OsStr::new("."),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(rustix_io)?;
    let scan = File::from(descriptor);
    let mut directory_entries = Dir::read_from(&scan).map_err(rustix_io)?;
    let mut entries = Vec::with_capacity(REMOVAL_BATCH_SIZE);
    while entries.len() < REMOVAL_BATCH_SIZE {
        let Some(entry) = directory_entries.read() else {
            break;
        };
        let entry = entry.map_err(rustix_io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == OsStr::new(".")
            || name == OsStr::new("..")
            || preserve_manifest && name == OsStr::new(MANIFEST_FILE)
        {
            continue;
        }
        entries.push(OsString::from_vec(name.as_bytes().to_vec()));
    }
    Ok(entries)
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

    use signalbox_runner_wire::{
        Advertisement, CanonicalUuid, DetailName, FailureCategory, FailureDetail,
        ManifestLifecycle, OperationCorrelation, OperationFailure, PositiveU64, ReleaseCorrelation,
        ReleasePhase, SandboxProfile, WorkspaceOperation, advertisement_digest,
        workspace_manifest_digest,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{EnrollmentAuthority, EnrollmentReceipt, RunnerStateRoot};

    use super::{
        MANIFEST_FILE, PrivateWorkspaceRequest, RunnerWorkspaceError, TRASH_DIRECTORY,
        open_created_directory,
    };

    const SESSION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e1;
    const RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e2;
    const OTHER_RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e3;
    const OTHER_MANIFEST: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e6;
    const PLACEMENT_REVISION: u64 = 3;
    const ENROLLMENT: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e4;
    const AUTHENTICATION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00e5;
    const OPEN_DIRECTORY_MODE: u32 = 0o750;
    const EXPECTED_DOCUMENT_MODE: u32 = 0o600;
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
            session_id: prepared.manifest.session,
            placement_revision: prepared.manifest.placement_revision,
            runner_id: prepared.manifest.runner,
            manifest_id: prepared.manifest.manifest_id,
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

        assert_eq!(prepared.manifest.session, expected.session());
        assert_eq!(prepared.manifest.runner, expected.runner());
        assert_eq!(prepared.manifest.lifecycle, ManifestLifecycle::Ready);
        assert_eq!(prepared.manifest.relative_path, EXPECTED_RELATIVE_PATH);
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
        let outside_contents = b"retained\n";
        fs::write(&outside, outside_contents).expect("the outside fixture exists");
        fs::create_dir(placement.join("work").join("nested"))
            .expect("the nested workspace fixture exists");
        fs::write(
            placement.join("work").join("nested").join("owned"),
            b"owned\n",
        )
        .expect("the owned workspace fixture exists");
        fs::set_permissions(
            placement.join("work").join("nested"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("the nested workspace fixture is deliberately inaccessible");
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
            outside_contents
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
    fn retired_private_root_release_proof_cannot_delete_the_placement() {
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
        let failure = OperationFailure {
            correlation: OperationCorrelation::Release(correlation.clone()),
            category: FailureCategory::WorkspaceCleanupFailed,
            detail: FailureDetail::try_new(
                DetailName::try_new("fixture-cleanup".to_owned())
                    .expect("the fixture detail name is valid"),
                String::from("the synthetic cleanup failed"),
                serde_json::json!({}),
            )
            .expect("the fixture cleanup failure is bounded"),
        };
        state
            .record_workspace_release_failure(failure)
            .expect("the cleanup failure is durable");
        state
            .acknowledge_workspace_release_failure(&correlation)
            .expect("the accepted release and failure are retired");
        let placement = parent
            .path()
            .join("runner-state")
            .join("sessions")
            .join(expected.session().to_string())
            .join(expected.placement_revision().get().to_string());

        let failure = state
            .workspace_store()
            .expect("the locked root forms a cleanup store")
            .release_private_root(&accepted)
            .expect_err("retired deletion authority fails closed");

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
