//! Injected-repository admission properties.

use std::{
    fs,
    io::Write,
    os::unix::fs::symlink,
    sync::{Arc, Mutex},
};

use git2::Repository;
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_application::ToolCatalog;
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::LocalOperation;
use crate::catalog::LocalGitTools;
use crate::construction::LocalGitToolsConstructionError;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_REPOSITORY_CONFIG_BYTES;
use crate::names::LOCAL_GIT_TOOL_NAMES;
use crate::tests::support::{
    CHANGED_CONTENT, ConcurrentRootOpenFileSystem, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE,
    ReplacingRootFileSystem, TRACKED_PATH, commit_all, identity,
    repository_uses_pinned_config_without_fifo_wait,
};

#[test]
fn injected_root_symlink_is_rejected() {
    let fixture = Fixture::new();
    let parent = tempfile::tempdir().expect("link parent constructs");
    let linked_root = parent.path().join("linked");
    symlink(fixture.root(), &linked_root).expect("root symlink constructs");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, linked_root, identity())
        .expect_err("symlink root rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Root(_)));
}

#[test]
fn gitdir_file_pointing_outside_root_is_rejected() {
    let root = tempfile::tempdir().expect("workspace root constructs");
    let outside = tempfile::tempdir().expect("outside repository constructs");
    Repository::init_bare(outside.path()).expect("outside repository initializes");
    fs::write(
        root.path().join(".git"),
        format!("gitdir: {}", outside.path().display()),
    )
    .expect("gitdir indirection writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
        .expect_err("external gitdir rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn configured_external_worktree_is_rejected() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside worktree constructs");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .config()
        .expect("config opens")
        .set_str(
            "core.worktree",
            outside.path().to_str().expect("temporary path is UTF-8"),
        )
        .expect("worktree override writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("external worktree rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn configured_external_ignore_file_is_rejected() {
    let fixture = Fixture::new();
    let outside = tempfile::NamedTempFile::new().expect("outside ignore file constructs");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .config()
        .expect("config opens")
        .set_str(
            "core.excludesFile",
            outside.path().to_str().expect("temporary path is UTF-8"),
        )
        .expect("external ignore override writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("external ignore file rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn inline_configured_external_ignore_file_is_rejected() {
    let fixture = Fixture::new();
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(fixture.root().join(".git/config"))
        .expect("fixture config opens");
    writeln!(config, "[core] excludesFile = /outside/evil")
        .expect("inline external ignore override writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("inline external ignore file rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn per_worktree_configuration_extension_is_rejected() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .config()
        .expect("config opens")
        .set_bool("extensions.worktreeConfig", true)
        .expect("worktree-config extension writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("per-worktree configuration rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn oversized_repository_config_is_rejected() {
    let fixture = Fixture::new();
    let config = fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join(".git/config"))
        .expect("fixture config opens");
    config
        .set_len((MAX_REPOSITORY_CONFIG_BYTES + 1) as u64)
        .expect("oversized sparse config sets length");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("oversized repository config rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn fifo_repository_config_is_rejected_without_blocking() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::remove_file(&config_path).expect("repository config removes for fixture");
    mkfifoat(CWD, &config_path, Mode::RUSR | Mode::WUSR)
        .expect("repository config FIFO constructs");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("repository config FIFO rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn pinned_config_never_opens_replacement_fifo() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let config_path = fixture.root().join(".git/config");
    fs::rename(&config_path, fixture.root().join(".git/config.pinned"))
        .expect("validated config retires");
    mkfifoat(CWD, &config_path, Mode::RUSR | Mode::WUSR)
        .expect("replacement config FIFO constructs");

    let opened_without_wait =
        repository_uses_pinned_config_without_fifo_wait(executor, config_path);

    assert!(opened_without_wait);
}

#[test]
fn status_rejects_a_replaced_repository_config() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let config_path = fixture.root().join(".git/config");
    fs::rename(&config_path, fixture.root().join(".git/config.pinned"))
        .expect("validated config retires");
    fs::write(&config_path, "[core]\n\tfilemode = false\n").expect("replacement config writes");

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("replacement config rejects status");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn common_git_directory_indirection_is_rejected() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".git/commondir"), "../outside")
        .expect("common-directory indirection writes");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("common-directory indirection rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn administrative_config_symlink_is_rejected() {
    let fixture = Fixture::new();
    let outside = tempfile::NamedTempFile::new().expect("outside config constructs");
    fs::remove_file(fixture.root().join(".git/config"))
        .expect("repository config removes for fixture");
    symlink(outside.path(), fixture.root().join(".git/config"))
        .expect("administrative symlink constructs");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("administrative symlink rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn nonregular_administrative_entry_is_rejected_without_blocking() {
    let fixture = Fixture::new();
    let head_path = fixture.root().join(".git/HEAD");
    fs::remove_file(&head_path).expect("repository HEAD removes for fixture");
    mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR).expect("repository HEAD FIFO constructs");

    let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect_err("nonregular administrative entry rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn executor_rejects_replacement_at_the_injected_root_path() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original file writes");
    commit_all(&original, INITIAL_MESSAGE);
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("suite constructs")
        .into_parts()
        .1;
    let retired = parent.path().join("retired");
    fs::rename(&root, &retired).expect("original workspace retires");
    fs::create_dir(&root).expect("replacement root constructs");
    let replacement = Repository::init(&root).expect("replacement repository initializes");
    fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("replacement file writes");
    commit_all(&replacement, INITIAL_MESSAGE);

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("replacement root rejects");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn pinned_repository_uses_portable_dev_fd_alias() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");

    assert!(
        executor
            .repository_authority
            .git_path("HEAD")
            .starts_with("/dev/fd/")
    );
    assert_eq!(
        repository.head().expect("HEAD exists").target(),
        Some(fixture.initial)
    );
}

#[test]
fn construction_rejects_replacement_while_workspace_root_is_pinned() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    fs::create_dir(&root).expect("workspace root constructs");
    let repository = Repository::init(&root).expect("repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("fixture file writes");
    commit_all(&repository, INITIAL_MESSAGE);
    let filesystem = ReplacingRootFileSystem {
        retired_root: parent.path().join("retired"),
        replacement_root: parent.path().join("replacement"),
    };

    let error = LocalGitTools::try_new(filesystem, &root, identity())
        .expect_err("replacement during root pinning rejects");

    assert!(matches!(error, LocalGitToolsConstructionError::Repository));
}

#[test]
fn construction_accepts_a_concurrent_descriptor_for_the_same_root() {
    let fixture = Fixture::new();
    let extra_root = Arc::new(Mutex::new(None));
    let filesystem = ConcurrentRootOpenFileSystem {
        extra_root: Arc::clone(&extra_root),
    };

    let tools = LocalGitTools::try_new(filesystem, fixture.root(), identity())
        .expect("concurrent same-root descriptor is harmless");

    assert_eq!(
        tools.catalog.definitions().len(),
        LOCAL_GIT_TOOL_NAMES.len()
    );
    assert!(
        extra_root
            .lock()
            .expect("concurrent root holder locks")
            .is_some()
    );
}
