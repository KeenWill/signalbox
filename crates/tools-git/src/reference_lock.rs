use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, statat,
        unlinkat,
    },
    io::dup,
};

use crate::descriptor::{FileIdentity, descriptor_entry_exists, file_identity};
use crate::failure::LocalGitFailure;
use crate::packed_reference::{packed_reference_namespace_conflicts, packed_reference_target};
use crate::pinning::PinnedRepository;
use crate::reference_read::{open_git_directory_path, read_reference_leaf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PinnedReferenceValue {
    Direct(git2::Oid),
    Symbolic(String),
    Missing,
}

pub(super) struct ReferenceLock {
    pub(super) name: String,
    parent: OwnedFd,
    leaf: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    _created_directories: CreatedReferenceDirectories,
    committed: bool,
}

pub(super) struct ReferenceParent {
    pub(super) directory: OwnedFd,
    pub(super) leaf: OsString,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    created_directories: CreatedReferenceDirectories,
    creation_file_mode: Option<Mode>,
}

#[derive(Debug)]
pub(super) struct CreatedReferenceDirectory {
    parent: OwnedFd,
    name: OsString,
    identity: FileIdentity,
}

#[derive(Default)]
pub(super) struct CreatedReferenceDirectories(Vec<CreatedReferenceDirectory>);

impl ReferenceLock {
    pub(super) fn acquire(
        authority: &PinnedRepository,
        name: &str,
    ) -> Result<Self, LocalGitFailure> {
        let bound = open_reference_parent(authority, name, true)?;
        let creation_file_mode = bound.creation_file_mode;
        let parent = bound.directory;
        let leaf = bound.leaf;
        let mut lock_name = OsString::from(&leaf);
        lock_name.push(".lock");
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let guard = Self {
            name: name.to_owned(),
            parent,
            leaf,
            lock_name,
            lock,
            identity,
            hierarchy: bound.hierarchy,
            _created_directories: bound.created_directories,
            committed: false,
        };
        let permissions = reference_permissions(&guard.parent, &guard.leaf)?
            .or_else(|| creation_file_mode.map(|mode| fs::Permissions::from_mode(mode.bits())));
        if let Some(permissions) = permissions {
            guard
                .lock
                .set_permissions(permissions)
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        Ok(guard)
    }

    pub(super) fn read(
        &self,
        authority: &PinnedRepository,
    ) -> Result<PinnedReferenceValue, LocalGitFailure> {
        read_reference_leaf(&self.parent, &self.leaf, authority, &self.name)
    }

    pub(super) fn hierarchy_is_current(&self, authority: &PinnedRepository) -> bool {
        self.hierarchy.iter().all(|(relative, expected)| {
            open_git_directory_path(authority, relative)
                .and_then(|directory| {
                    let metadata = fs::File::from(directory)
                        .metadata()
                        .map_err(|_| LocalGitFailure::Operation)?;
                    Ok(file_identity(&metadata) == *expected)
                })
                .unwrap_or(false)
        })
    }

    pub(super) fn prepare(
        &mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        writeln!(self.lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    pub(super) fn prepare_symbolic(
        &mut self,
        authority: &PinnedRepository,
        target: &str,
    ) -> Result<(), LocalGitFailure> {
        writeln!(self.lock, "ref: {target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    pub(super) fn publish(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hook(authority, expected, || {})
    }

    pub(super) fn publish_with_hook<Hook: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: Hook,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(authority, expected, before_absent_publish, || {})
    }

    fn publish_with_hooks<BeforeAbsent: FnOnce(), AfterPublish: FnOnce()>(
        mut self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: BeforeAbsent,
        after_publish: AfterPublish,
    ) -> Result<(), LocalGitFailure> {
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        let expected_packed = packed_reference_target(authority, &self.name)?;
        if packed_reference_namespace_conflicts(authority, &self.name)? {
            return Err(LocalGitFailure::Operation);
        }
        if !descriptor_entry_exists(&self.parent, &self.leaf)? {
            if self.read(authority)? != *expected {
                return Err(LocalGitFailure::Operation);
            }
            before_absent_publish();
            renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::NOREPLACE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            after_publish();
            let packed_is_current = packed_reference_target(authority, &self.name)
                .is_ok_and(|current| current == expected_packed);
            let packed_namespace_is_clear =
                packed_reference_namespace_conflicts(authority, &self.name)
                    .is_ok_and(|conflicts| !conflicts);
            if !packed_is_current
                || !packed_namespace_is_clear
                || !self.hierarchy_is_current(authority)
            {
                if self.published_path_still_owned() {
                    let _ = unlinkat(&self.parent, &self.leaf, AtFlags::empty());
                }
                return Err(LocalGitFailure::Operation);
            }
            self.committed = true;
            return Ok(());
        }
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::EXCHANGE,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        after_publish();
        let displaced = read_reference_leaf(&self.parent, &self.lock_name, authority, &self.name);
        let packed_is_current = packed_reference_target(authority, &self.name)
            .is_ok_and(|current| current == expected_packed);
        let packed_namespace_is_clear = packed_reference_namespace_conflicts(authority, &self.name)
            .is_ok_and(|conflicts| !conflicts);
        if displaced.as_ref() != Ok(expected)
            || !packed_is_current
            || !packed_namespace_is_clear
            || !self.hierarchy_is_current(authority)
        {
            renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::EXCHANGE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            return Err(LocalGitFailure::Operation);
        }
        if unlinkat(&self.parent, &self.lock_name, AtFlags::empty()).is_err() {
            renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::EXCHANGE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            return Err(LocalGitFailure::Operation);
        }
        self.committed = true;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn publish_with_test_hooks<BeforeAbsent: FnOnce(), AfterPublish: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: BeforeAbsent,
        after_publish: AfterPublish,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(authority, expected, before_absent_publish, after_publish)
    }

    #[cfg(test)]
    pub(super) fn commit(
        mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        let expected = self.read(authority)?;
        self.prepare(authority, target)?;
        self.publish(authority, &expected)
    }

    fn path_still_owned(&self) -> bool {
        let descriptor_identity = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok();
        let path_identity = openat(
            &self.parent,
            &self.lock_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        descriptor_identity == Some(self.identity) && path_identity == Some(self.identity)
    }

    fn published_path_still_owned(&self) -> bool {
        let descriptor_identity = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok();
        let path_identity = openat(
            &self.parent,
            &self.leaf,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        descriptor_identity == Some(self.identity) && path_identity == Some(self.identity)
    }
}

pub(super) fn reference_permissions(
    parent: &OwnedFd,
    leaf: &OsStr,
) -> Result<Option<fs::Permissions>, LocalGitFailure> {
    let descriptor = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let metadata = fs::File::from(descriptor)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() {
        return Err(LocalGitFailure::Operation);
    }
    Ok(Some(fs::Permissions::from_mode(metadata.mode() & 0o777)))
}

impl Drop for ReferenceLock {
    fn drop(&mut self) {
        if !self.committed && self.path_still_owned() {
            let _ = unlinkat(&self.parent, &self.lock_name, AtFlags::empty());
        }
    }
}

impl Drop for CreatedReferenceDirectories {
    fn drop(&mut self) {
        for directory in self.0.iter().rev() {
            directory.remove_if_owned();
        }
    }
}

impl CreatedReferenceDirectory {
    fn remove_if_owned(&self) {
        let current_identity = openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|directory| fs::File::from(directory).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        if current_identity == Some(self.identity) {
            let _ = unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
        }
    }
}

impl CreatedReferenceDirectories {
    pub(super) fn open_or_create(
        &mut self,
        parent: &OwnedFd,
        name: &OsStr,
        mode: Mode,
    ) -> Result<OwnedFd, LocalGitFailure> {
        let (directory, created) =
            open_or_create_ref_directory_with_mode_tracked(parent, name, mode)?;
        if let Some(created) = created {
            self.0.push(created);
        }
        Ok(directory)
    }
}

pub(super) fn open_reference_parent(
    authority: &PinnedRepository,
    name: &str,
    create: bool,
) -> Result<ReferenceParent, LocalGitFailure> {
    if name != "HEAD" && (!name.starts_with("refs/") || !git2::Reference::is_valid_name(name)) {
        return Err(LocalGitFailure::Operation);
    }
    let path = Path::new(name);
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
    let creation_modes = if create && name.starts_with("refs/") {
        Some(reference_creation_modes(authority)?)
    } else {
        None
    };
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    let mut relative = PathBuf::new();
    let mut hierarchy = vec![(
        relative.clone(),
        file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        ),
    )];
    let mut created_directories = CreatedReferenceDirectories::default();
    for component in parent_path.components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        let next_directory = if create {
            match creation_modes {
                Some((directory_mode, _)) => {
                    created_directories.open_or_create(&directory, component, directory_mode)?
                }
                None => open_or_create_ref_directory(&directory, component)?,
            }
        } else {
            openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?
        };
        directory = next_directory;
        relative.push(component);
        let identity = file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        hierarchy.push((relative.clone(), identity));
    }
    Ok(ReferenceParent {
        directory,
        leaf,
        hierarchy,
        created_directories,
        creation_file_mode: creation_modes.map(|(_, file_mode)| file_mode),
    })
}

pub(super) fn reference_creation_modes(
    authority: &PinnedRepository,
) -> Result<(Mode, Mode), LocalGitFailure> {
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    reference_installation_modes(&refs)
}

pub(super) fn reference_installation_modes(
    refs: &OwnedFd,
) -> Result<(Mode, Mode), LocalGitFailure> {
    let metadata = fs::File::from(dup(refs).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let directory_mode = (metadata.mode() & 0o2777) | 0o700;
    let file_mode = (metadata.mode() & 0o666) | 0o600;
    Ok((
        Mode::from_raw_mode(directory_mode),
        Mode::from_raw_mode(file_mode),
    ))
}

pub(super) fn open_or_create_ref_directory(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
}

pub(super) fn open_or_create_ref_directory_with_mode(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked(parent, name, mode)
        .map(|(directory, _)| directory)
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<(OwnedFd, Option<CreatedReferenceDirectory>), LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked_and_hook(parent, name, mode, || Ok(()))
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked_and_hook<PostCreate>(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
    post_create: PostCreate,
) -> Result<(OwnedFd, Option<CreatedReferenceDirectory>), LocalGitFailure>
where
    PostCreate: FnOnce() -> Result<(), LocalGitFailure>,
{
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => Ok((directory, None)),
        Err(error) if error == rustix::io::Errno::NOENT => {
            let pinned_parent = dup(parent).map_err(|_| LocalGitFailure::Operation)?;
            mkdirat(parent, name, mode).map_err(|_| LocalGitFailure::Operation)?;
            let status = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(status) if FileType::from_raw_mode(status.st_mode) == FileType::Directory => {
                    status
                }
                Ok(_) | Err(_) => {
                    let _ = unlinkat(parent, name, AtFlags::REMOVEDIR);
                    return Err(LocalGitFailure::Operation);
                }
            };
            let created = CreatedReferenceDirectory {
                parent: pinned_parent,
                name: name.to_owned(),
                identity: FileIdentity {
                    device: status.st_dev,
                    inode: status.st_ino,
                },
            };
            if let Err(failure) = post_create() {
                created.remove_if_owned();
                return Err(failure);
            }
            let directory = match openat(parent, name, flags, Mode::empty()) {
                Ok(directory) => directory,
                Err(_) => {
                    created.remove_if_owned();
                    return Err(LocalGitFailure::Operation);
                }
            };
            let permission_file = match dup(&directory) {
                Ok(descriptor) => fs::File::from(descriptor),
                Err(_) => {
                    created.remove_if_owned();
                    return Err(LocalGitFailure::Operation);
                }
            };
            if permission_file
                .set_permissions(fs::Permissions::from_mode(mode.bits()))
                .is_err()
            {
                created.remove_if_owned();
                return Err(LocalGitFailure::Operation);
            }
            Ok((directory, Some(created)))
        }
        Err(_) => Err(LocalGitFailure::Operation),
    }
}
