//! Cleanup authorized by a retained, fsynced release journal.

use super::*;
use crate::journal::AcceptedWorkspaceRelease;
use signalbox_runner_wire::ReleaseCorrelation;
use std::os::fd::AsRawFd as _;

pub(super) const TRASH_DIRECTORY: &str = "trash";

impl RunnerWorkspaceStore {
    pub(crate) fn release(
        &self,
        accepted: &AcceptedWorkspaceRelease,
    ) -> Result<(), RunnerWorkspaceError> {
        validate_root_directory(&self.canonical_root, &self.root)
            .map_err(RunnerWorkspaceError::Io)?;
        let correlation = accepted.correlation();
        let trash_name = correlation.manifest_id.to_string();
        let trash = optional_directory(&self.root, TRASH_DIRECTORY)?;
        let trashed = trash
            .as_ref()
            .map(|trash| optional_directory(trash, &trash_name))
            .transpose()?
            .flatten();
        let located = placement_directory(&self.root, correlation)?;
        match (located, trashed) {
            (Some(_), Some(_)) => Err(RunnerWorkspaceError::ManifestConflict),
            (Some((session, placement)), None) => {
                let mut manifest = read_manifest(&placement)?;
                let leaf = if manifest.repository.is_some() {
                    REPOSITORY_WORKSPACE_DIRECTORY
                } else {
                    PRIVATE_WORKSPACE_DIRECTORY
                };
                let expected_path = format!(
                    "{SESSIONS_DIRECTORY}/{}/{}/{leaf}",
                    correlation.session_id,
                    correlation.placement_revision.get()
                );
                if manifest.session != correlation.session_id
                    || manifest.placement_revision != correlation.placement_revision
                    || manifest.runner != correlation.runner_id
                    || manifest.manifest_id != correlation.manifest_id
                    || manifest.relative_path != expected_path
                    || !matches!(
                        manifest.lifecycle,
                        ManifestLifecycle::Ready
                            | ManifestLifecycle::Active
                            | ManifestLifecycle::Releasing
                    )
                {
                    return Err(RunnerWorkspaceError::ManifestConflict);
                }
                manifest.lifecycle = ManifestLifecycle::Releasing;
                write_manifest(&placement, &manifest)?;
                let placement_name = correlation.placement_revision.get().to_string();
                if !path_names_directory(&session, &placement_name, &placement)? {
                    return Err(RunnerWorkspaceError::ManifestConflict);
                }
                let trash = open_or_create_directory(&self.root, TRASH_DIRECTORY)?;
                renameat_with(
                    &session,
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
                finish_deletion(&trash, &trash_name, placement)
            }
            (None, Some(placement)) => {
                let trash = trash.ok_or(RunnerWorkspaceError::ManifestConflict)?;
                // The accepted journal authorizes this location even after partial
                // deletion has removed its manifest.
                finish_deletion(&trash, &trash_name, placement)
            }
            (None, None) => Ok(()),
        }
    }
}

fn placement_directory(
    root: &File,
    correlation: &ReleaseCorrelation,
) -> Result<Option<(File, File)>, RunnerWorkspaceError> {
    let Some(sessions) = optional_directory(root, SESSIONS_DIRECTORY)? else {
        return Ok(None);
    };
    let Some(session) = optional_directory(&sessions, &correlation.session_id.to_string())? else {
        return Ok(None);
    };
    Ok(
        optional_directory(&session, &correlation.placement_revision.get().to_string())?
            .map(|placement| (session, placement)),
    )
}

fn optional_directory(parent: &File, name: &str) -> Result<Option<File>, RunnerWorkspaceError> {
    match open_directory(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RunnerWorkspaceError::Io(error)),
    }
}

fn finish_deletion(trash: &File, name: &str, placement: File) -> Result<(), RunnerWorkspaceError> {
    if !path_names_directory(trash, name, &placement)? {
        return Err(RunnerWorkspaceError::ManifestConflict);
    }
    remove_open_directory_tree(trash, OsStr::new(name), placement)?;
    trash
        .sync_all()
        .map_err(RunnerWorkspaceError::CommitAmbiguous)
}
enum RemovalStep {
    Inspect {
        parent: Rc<File>,
        name: OsString,
    },
    Unlink {
        parent: Rc<File>,
        name: OsString,
    },
    RemoveDirectory {
        parent: Rc<File>,
        name: OsString,
        identity: DirectoryIdentity,
        directory: Rc<File>,
    },
}

fn remove_open_directory_tree(
    parent: &File,
    name: &OsStr,
    directory: File,
) -> Result<(), RunnerWorkspaceError> {
    fchmod(&directory, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(rustix_io)?;
    let identity = DirectoryIdentity::from_file(&directory)?;
    let parent = Rc::new(parent.try_clone().map_err(RunnerWorkspaceError::Io)?);
    let directory = Rc::new(directory);
    let mut steps = vec![RemovalStep::RemoveDirectory {
        parent,
        name: name.to_owned(),
        identity,
        directory: Rc::clone(&directory),
    }];
    push_directory_entries(&mut steps, Rc::clone(&directory))?;
    remove_directory_steps(steps)
}

fn remove_directory_steps(mut steps: Vec<RemovalStep>) -> Result<(), RunnerWorkspaceError> {
    while let Some(step) = steps.pop() {
        match step {
            RemovalStep::Inspect { parent, name } => {
                let status = match statat(parent.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(status) => status,
                    Err(error) if error == rustix::io::Errno::NOENT => continue,
                    Err(error) => return Err(rustix_io(error)),
                };
                if FileType::from_raw_mode(status.st_mode) == FileType::Directory {
                    let opaque_descriptor = openat(
                        parent.as_ref(),
                        &name,
                        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(rustix_io)?;
                    let opaque = File::from(opaque_descriptor);
                    let identity = DirectoryIdentity::from_file(&opaque)?;
                    if !identity.names(parent.as_ref(), &name)? {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    // Linux's descriptor link names the pinned directory even
                    // when its mode prevents opening it for reading.
                    std::fs::set_permissions(
                        format!("/proc/self/fd/{}", opaque.as_raw_fd()),
                        std::fs::Permissions::from_mode(DIRECTORY_MODE),
                    )
                    .map_err(RunnerWorkspaceError::Io)?;
                    if DirectoryIdentity::from_file(&opaque)? != identity
                        || !identity.names(parent.as_ref(), &name)?
                    {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    let readable_descriptor = openat(
                        parent.as_ref(),
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(rustix_io)?;
                    let child = Rc::new(File::from(readable_descriptor));
                    if DirectoryIdentity::from_file(child.as_ref())? != identity
                        || !identity.names(parent.as_ref(), &name)?
                    {
                        return Err(RunnerWorkspaceError::ManifestConflict);
                    }
                    steps.push(RemovalStep::RemoveDirectory {
                        parent,
                        name,
                        identity,
                        directory: Rc::clone(&child),
                    });
                    push_directory_entries(&mut steps, child)?;
                } else {
                    steps.push(RemovalStep::Unlink { parent, name });
                }
            }
            RemovalStep::Unlink { parent, name } => {
                match unlinkat(parent.as_ref(), &name, AtFlags::empty()) {
                    Ok(()) => {}
                    Err(error) if error == rustix::io::Errno::NOENT => {}
                    Err(error) => return Err(rustix_io(error)),
                }
            }
            RemovalStep::RemoveDirectory {
                parent,
                name,
                identity,
                directory,
            } => {
                match statat(parent.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Err(error) if error == rustix::io::Errno::NOENT => continue,
                    Err(error) => return Err(rustix_io(error)),
                    Ok(status)
                        if status.st_dev == identity.device && status.st_ino == identity.inode => {}
                    Ok(_) => return Err(RunnerWorkspaceError::ManifestConflict),
                }
                directory.sync_all().map_err(RunnerWorkspaceError::Io)?;
                match unlinkat(parent.as_ref(), &name, AtFlags::REMOVEDIR) {
                    Ok(()) => {}
                    Err(error) if error == rustix::io::Errno::NOENT => {}
                    Err(error) => return Err(rustix_io(error)),
                }
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
