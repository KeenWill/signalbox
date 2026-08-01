use std::{
    ffi::OsStr,
    fmt, fs,
    io::Read,
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

use git2::{Config, ErrorCode, Odb, Repository, RepositoryOpenFlags};
use rustix::fs::{CWD, Mode, OFlags, openat};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    RepositoryIdentity, descriptor_path, descriptor_path_from_fd, file_identity,
};
use crate::failure::LocalGitFailure;
use crate::layout::open_repository_config_at;
use crate::limits::{MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES, MAX_PACK_FILE_BYTES};

pub(super) struct PinnedRepository {
    pub(super) root: fs::File,
    pub(super) git_directory: fs::File,
    pub(super) _config: fs::File,
    pub(super) _config_snapshot: fs::File,
    repository: Mutex<Repository>,
}

pub(super) struct PinnedObjectDatabase {
    pub(super) directory: tempfile::TempDir,
}

impl fmt::Debug for PinnedRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedRepository")
            .finish_non_exhaustive()
    }
}

impl PinnedRepository {
    pub(super) fn open(
        root_path: &Path,
        expected: RepositoryIdentity,
    ) -> Result<Self, LocalGitToolsConstructionError> {
        Self::open_with_hook(root_path, expected, || {})
    }

    pub(super) fn open_with_hook<Hook>(
        root_path: &Path,
        expected: RepositoryIdentity,
        after_git_directory_open: Hook,
    ) -> Result<Self, LocalGitToolsConstructionError>
    where
        Hook: FnOnce(),
    {
        Self::open_with_hooks(root_path, expected, after_git_directory_open, || {})
    }

    pub(super) fn open_with_hooks<GitDirectoryHook, ConfigHook>(
        root_path: &Path,
        expected: RepositoryIdentity,
        after_git_directory_open: GitDirectoryHook,
        after_config_snapshot: ConfigHook,
    ) -> Result<Self, LocalGitToolsConstructionError>
    where
        GitDirectoryHook: FnOnce(),
        ConfigHook: FnOnce(),
    {
        let root =
            fs::File::open(root_path).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let git_directory = fs::File::open(root_path.join(".git"))
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        after_git_directory_open();
        let config = open_repository_config_at(&git_directory)?;
        after_config_snapshot();
        let observed = RepositoryIdentity {
            root: file_identity(
                &root
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            git_directory: file_identity(
                &git_directory
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            config: file_identity(
                &config
                    .source
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
        };
        if observed != expected {
            return Err(LocalGitToolsConstructionError::Repository);
        }
        let repository = open_pinned_repository(&root, &git_directory, &config.snapshot)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        Ok(Self {
            root,
            git_directory,
            _config: config.source,
            _config_snapshot: config.snapshot,
            repository: Mutex::new(repository),
        })
    }

    pub(super) fn repository(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Repository>, LocalGitFailure> {
        self.repository
            .lock()
            .map_err(|_| LocalGitFailure::Repository)
    }

    pub(super) fn git_path(&self, path: &str) -> PathBuf {
        descriptor_path(&self.git_directory).join(path)
    }
}

impl PinnedObjectDatabase {
    pub(super) fn capture(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        let objects = openat(
            &authority.git_directory,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let directory = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
        fs::create_dir(directory.path().join("pack")).map_err(|_| LocalGitFailure::Operation)?;
        let mut inspected = 0_usize;
        let mut captured_bytes = 0_u64;
        for entry in fs::read_dir(descriptor_path_from_fd(&objects))
            .map_err(|_| LocalGitFailure::Repository)?
        {
            let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(LocalGitFailure::Repository);
            }
            let name = entry.file_name();
            let bytes = name.as_bytes();
            if name == OsStr::new("info") {
                continue;
            }
            if name == OsStr::new("pack") {
                let pack = openat(
                    &objects,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitFailure::Repository)?;
                pin_object_directory(
                    &pack,
                    &directory.path().join("pack"),
                    &mut inspected,
                    &mut captured_bytes,
                    false,
                )?;
                continue;
            }
            if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(LocalGitFailure::Repository);
            }
            let loose = openat(
                &objects,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            let destination = directory.path().join(&name);
            fs::create_dir(&destination).map_err(|_| LocalGitFailure::Operation)?;
            pin_object_directory(
                &loose,
                &destination,
                &mut inspected,
                &mut captured_bytes,
                true,
            )?;
        }
        Ok(Self { directory })
    }

    pub(super) fn add_to(&self, object_database: &Odb<'_>) -> Result<(), LocalGitFailure> {
        let path = self
            .directory
            .path()
            .to_str()
            .ok_or(LocalGitFailure::Operation)?;
        object_database
            .add_disk_alternate(path)
            .map_err(|_| LocalGitFailure::Operation)
    }
}

pub(super) fn pin_object_directory(
    source: &OwnedFd,
    destination: &Path,
    inspected: &mut usize,
    captured_bytes: &mut u64,
    loose: bool,
) -> Result<(), LocalGitFailure> {
    for entry in
        fs::read_dir(descriptor_path_from_fd(source)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        *inspected = inspected.saturating_add(1);
        if *inspected > 100_000 {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if loose
            && (name.as_bytes().len() != 38 || !name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        {
            return Err(LocalGitFailure::Repository);
        }
        let descriptor = openat(
            source,
            &name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let mut file = fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
        let per_file_limit = if loose {
            MAX_OBJECT_BYTES.saturating_mul(2)
        } else {
            MAX_PACK_FILE_BYTES
        } as u64;
        if !metadata.is_file()
            || metadata.len() > per_file_limit
            || captured_bytes.saturating_add(metadata.len()) > MAX_OBJECT_DATABASE_BYTES as u64
        {
            return Err(LocalGitFailure::Repository);
        }
        let mut snapshot = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(&name))
            .map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut file).take(metadata.len().saturating_add(1)),
            &mut snapshot,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied != metadata.len() {
            return Err(LocalGitFailure::Repository);
        }
        *captured_bytes = captured_bytes.saturating_add(copied);
    }
    Ok(())
}

pub(super) fn live_object_database_bytes(
    authority: &PinnedRepository,
) -> Result<u64, LocalGitFailure> {
    let objects = openat(
        &authority.git_directory,
        "objects",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let mut inspected = 0_usize;
    let mut bytes = 0_u64;
    for entry in
        fs::read_dir(descriptor_path_from_fd(&objects)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        inspected = inspected.saturating_add(1);
        if inspected > 100_000 {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if name == OsStr::new("info") {
            continue;
        }
        if name == OsStr::new("pack") {
            let pack = openat(
                &objects,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            measure_object_directory(&pack, &mut inspected, &mut bytes, false)?;
            continue;
        }
        if name.as_bytes().len() != 2 || !name.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            return Err(LocalGitFailure::Repository);
        }
        let loose = openat(
            &objects,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        measure_object_directory(&loose, &mut inspected, &mut bytes, true)?;
    }
    Ok(bytes)
}

pub(super) fn measure_object_directory(
    directory: &OwnedFd,
    inspected: &mut usize,
    total_bytes: &mut u64,
    loose: bool,
) -> Result<(), LocalGitFailure> {
    for entry in
        fs::read_dir(descriptor_path_from_fd(directory)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        *inspected = inspected.saturating_add(1);
        if *inspected > 100_000 {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if loose
            && (name.as_bytes().len() != 38 || !name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        {
            return Err(LocalGitFailure::Repository);
        }
        let descriptor = openat(
            directory,
            &name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let metadata = fs::File::from(descriptor)
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?;
        let per_file_limit = if loose {
            MAX_OBJECT_BYTES.saturating_mul(2)
        } else {
            MAX_PACK_FILE_BYTES
        } as u64;
        *total_bytes = total_bytes
            .checked_add(metadata.len())
            .filter(|bytes| *bytes <= MAX_OBJECT_DATABASE_BYTES as u64)
            .ok_or(LocalGitFailure::Repository)?;
        if !metadata.is_file() || metadata.len() > per_file_limit {
            return Err(LocalGitFailure::Repository);
        }
    }
    Ok(())
}

pub(super) fn open_pinned_repository(
    root: &fs::File,
    git_directory: &fs::File,
    config: &fs::File,
) -> Result<Repository, git2::Error> {
    let root_path = descriptor_path(root);
    let git_directory_path = descriptor_path(git_directory);
    let repository = Repository::open_ext(
        &git_directory_path,
        RepositoryOpenFlags::BARE | RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )?;
    repository.set_workdir(&root_path, false)?;
    let config = Config::open(&descriptor_path(config))?;
    repository.set_config(&config)?;
    Ok(repository)
}

pub(super) fn repository_filemode(repository: &Repository) -> Result<bool, LocalGitFailure> {
    let config = repository
        .config()
        .map_err(|_| LocalGitFailure::Repository)?;
    match config.get_bool("core.filemode") {
        Ok(filemode) => Ok(filemode),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(true),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}

pub(super) fn pin_optional_git_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<fs::File>, LocalGitFailure> {
    match openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let file = fs::File::from(descriptor);
            let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if metadata.is_file() && metadata.len() <= max_bytes as u64 {
                Ok(Some(file))
            } else {
                Err(LocalGitFailure::Repository)
            }
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}
