use std::{ffi::OsStr, fs::File, io::Read, os::fd::AsFd, os::unix::ffi::OsStrExt, path::Path};

use bstr::BStr;
use rustix::fs::{Mode, OFlags, openat};
use signalbox_tools_workspace::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspacePathRejection, WorkspaceResolveError,
    WorkspaceRoot,
};

use crate::{
    descriptor::file_snapshot_identity, descriptor_identity::bind_still_holds,
    failure::LocalGitFailure, pinning::PinnedRepository,
};

pub(super) struct IgnoreRules(gix_ignore::Search);

impl IgnoreRules {
    pub(super) fn new(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        let mut rules = Self(gix_ignore::Search::default());
        if let Some(bytes) = read_repository_excludes(authority)? {
            rules
                .0
                .add_patterns_buffer(&bytes, "info/exclude", None, Default::default());
        }
        Ok(rules)
    }

    pub(super) fn add_directory<FileSystem: WorkspaceFileSystem>(
        &mut self,
        filesystem: &FileSystem,
        root: &WorkspaceRoot,
        root_path: &Path,
        directory: &Path,
    ) -> Result<(), LocalGitFailure> {
        let path = if directory == Path::new(".") {
            Path::new(".gitignore").to_owned()
        } else {
            directory.join(".gitignore")
        };
        match filesystem.entry_kind(root, &path) {
            Ok(WorkspaceEntryKind::File) => {}
            Ok(_) | Err(WorkspaceResolveError::Rejected(WorkspacePathRejection::Symlink)) => {
                return Ok(());
            }
            Err(WorkspaceResolveError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(());
            }
            Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
        }
        let mut reader = filesystem
            .open_file_stream(root, &path)
            .map_err(|_| LocalGitFailure::Operation)?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.0.add_patterns_buffer(
            &bytes,
            root_path.join(path),
            Some(root_path),
            Default::default(),
        );
        Ok(())
    }

    pub(super) fn excludes(&self, path: &Path, directory: bool, ignorecase: bool) -> bool {
        let case = if ignorecase {
            gix_ignore::glob::pattern::Case::Fold
        } else {
            gix_ignore::glob::pattern::Case::Sensitive
        };
        self.0
            .pattern_matching_relative_path(
                BStr::new(path.as_os_str().as_bytes()),
                Some(directory),
                case,
            )
            .is_some_and(|matched| !matched.pattern.is_negative())
    }
}

fn read_repository_excludes(
    authority: &PinnedRepository,
) -> Result<Option<Vec<u8>>, LocalGitFailure> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let directory = match openat(
        &authority.git_directory,
        "info",
        flags | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let mut file = match openat(&directory, "exclude", flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let before = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if !before.is_file() {
        return Ok(None);
    }
    let expected = file_snapshot_identity(&before);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    bind_still_holds(
        authority.git_directory.as_fd(),
        OsStr::new("info"),
        &directory,
        || LocalGitFailure::Repository,
    )?;
    let current = File::from(
        openat(&directory, "exclude", flags, Mode::empty())
            .map_err(|_| LocalGitFailure::Repository)?,
    );
    if file_snapshot_identity(&file.metadata().map_err(|_| LocalGitFailure::Repository)?)
        != expected
        || file_snapshot_identity(
            &current
                .metadata()
                .map_err(|_| LocalGitFailure::Repository)?,
        ) != expected
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(Some(bytes))
}
