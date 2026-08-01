use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
};

use git2::{Odb, Repository};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat};

use crate::arguments::GitBranchCreateArguments;
use crate::bounded::{
    find_bounded_commit, find_bounded_tree, resolve_bounded_commit, validate_tree_discovery,
};
use crate::descriptor::{descriptor_path_from_fd, file_identity};
use crate::failure::LocalGitFailure;
use crate::objects::{PackRoot, persist_objects};
use crate::pack_install::pack_entry_is_owned;
use crate::packed_reference::packed_reference_exists;
use crate::pinning::{PinnedRepository, open_pinned_repository};
use crate::reference_lock::{CreatedReferenceDirectories, reference_installation_modes};
use crate::result::BranchResult;

pub(super) fn branch_create<ValidateRoot>(
    repository: &Repository,
    authority: &PinnedRepository,
    captured_objects: &Odb<'_>,
    arguments: GitBranchCreateArguments,
    validate_root_before_publish: ValidateRoot,
) -> Result<BranchResult, LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let commit = resolve_bounded_commit(repository, authority, &arguments.start)?;
    let head = commit.id().to_string();
    let absent_objects = Odb::new().map_err(|_| LocalGitFailure::Operation)?;
    persist_objects(
        authority,
        repository,
        &absent_objects,
        captured_objects,
        &[PackRoot::Commit(commit.id())],
    )?;
    let reference_name = format!("refs/heads/{}", arguments.name);
    if packed_reference_exists(authority, &reference_name)? {
        return Err(LocalGitFailure::Operation);
    }
    create_loose_branch_reference(
        authority,
        &arguments.name,
        commit.id(),
        validate_root_before_publish,
    )?;
    Ok(BranchResult {
        branch: arguments.name,
        head,
    })
}

pub(super) fn create_loose_branch_reference<ValidateRoot>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    validate_root_before_publish: ValidateRoot,
) -> Result<(), LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    create_loose_branch_reference_with_hooks(
        authority,
        branch,
        target,
        || {},
        validate_root_before_publish,
    )
}

#[cfg(test)]
pub(super) fn create_loose_branch_reference_with_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    post_write: Hook,
) -> Result<(), LocalGitFailure> {
    create_loose_branch_reference_with_hooks(authority, branch, target, post_write, || Ok(()))
}

pub(super) fn create_loose_branch_reference_with_hooks<Hook, ValidateRoot>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    post_write: Hook,
    validate_root_before_publish: ValidateRoot,
) -> Result<(), LocalGitFailure>
where
    Hook: FnOnce(),
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let (directory_mode, file_mode) = reference_installation_modes(&refs)?;
    let mut created_directories = CreatedReferenceDirectories::default();
    let mut directory =
        created_directories.open_or_create(&refs, OsStr::new("heads"), directory_mode)?;
    let mut components = Path::new(branch).components().peekable();
    let mut leaf = None;
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        if components.peek().is_some() {
            directory =
                created_directories.open_or_create(&directory, component, directory_mode)?;
        } else {
            leaf = Some(component.to_owned());
        }
    }
    let leaf = leaf.ok_or(LocalGitFailure::Operation)?;
    let mut lock_name = OsString::from(&leaf);
    lock_name.push(".lock");
    let lock = openat(
        &directory,
        &lock_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        file_mode,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut lock = fs::File::from(lock);
    let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
    let lock_path = descriptor_path_from_fd(&directory).join(&lock_name);
    let outcome = (|| {
        lock.set_permissions(fs::Permissions::from_mode(file_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        writeln!(lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        lock.sync_all().map_err(|_| LocalGitFailure::Operation)?;
        post_write();
        let reference_name = format!("refs/heads/{branch}");
        let packed_is_absent =
            packed_reference_exists(authority, &reference_name).is_ok_and(|exists| !exists);
        if !packed_is_absent {
            return Err(LocalGitFailure::Operation);
        }
        let path_identity = fs::symlink_metadata(&lock_path)
            .map(|metadata| file_identity(&metadata))
            .map_err(|_| LocalGitFailure::Operation)?;
        let descriptor_identity =
            file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        if path_identity != identity || descriptor_identity != identity {
            return Err(LocalGitFailure::Operation);
        }
        validate_root_before_publish()?;
        validate_live_branch_target(authority, target)?;
        renameat_with(
            &directory,
            &lock_name,
            &directory,
            &leaf,
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let packed_still_absent =
            packed_reference_exists(authority, &reference_name).is_ok_and(|exists| !exists);
        if !packed_still_absent {
            if pack_entry_is_owned(&directory, &leaf, &lock, identity) {
                let _ = unlinkat(&directory, &leaf, AtFlags::empty());
            }
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    })();
    let still_owned = fs::symlink_metadata(&lock_path)
        .map(|metadata| file_identity(&metadata) == identity)
        .unwrap_or(false);
    if outcome.is_err() && still_owned {
        let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
    }
    outcome
}

pub(super) fn validate_live_branch_target(
    authority: &PinnedRepository,
    target: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let repository = open_pinned_repository(
        &authority.root,
        &authority.git_directory,
        &authority._config,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let commit = find_bounded_commit(&repository, target)?;
    let tree = find_bounded_tree(&repository, commit.tree_id())?;
    validate_tree_discovery(&repository, &tree)
}
