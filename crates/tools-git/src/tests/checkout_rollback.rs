//! Checkout rollback properties.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use git2::{Repository, RepositoryState};
use rustix::fs::{CWD, Mode, mkfifoat};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::GitBranchSwitchArguments;
use crate::catalog::LocalGitTools;
use crate::executor::clone_index_entry;
use crate::failure::LocalGitFailure;
use crate::rollback::{CheckoutRollbackContext, checkout_tree_with_rollback};
use crate::tests::planting::install_resolve_undo_extension;
use crate::tests::support::{
    CHANGED_CONTENT, CONFLICT_OURS_CONTENT, FIX_BRANCH, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE,
    MODEL_MESSAGE, TARGET_CONTENT, TRACKED_PATH, UNTRACKED_CONTENT, commit_all, identity,
    index_extension, install_deleted_conflict,
};

#[test]
fn branch_switch_rejects_head_lock_before_checkout() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("root-level fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    let original_branch = repository
        .head()
        .expect("fixture HEAD exists")
        .shorthand()
        .expect("fixture branch name is UTF-8")
        .to_owned();
    let executor = fixture.executor();
    fs::write(fixture.root().join(".git/HEAD.lock"), []).expect("fixture HEAD lock writes");

    let failure = executor
        .branch_switch(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
        )
        .expect_err("locked HEAD rejects before checkout");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("locked fixture content reads"),
        CHANGED_CONTENT.as_bytes()
    );
    assert_eq!(
        repository.head().expect("fixture HEAD remains").shorthand(),
        Ok(original_branch.as_str())
    );
}

#[test]
fn branch_switch_rejects_target_lock_before_checkout() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("root-level fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    fs::write(fixture.root().join(".git/refs/heads/agent/fix.lock"), [])
        .expect("fixture target lock writes");
    let executor = fixture.executor();

    let failure = executor
        .branch_switch(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
        )
        .expect_err("locked target rejects before checkout");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("locked fixture content reads"),
        CHANGED_CONTENT.as_bytes()
    );
}

#[test]
fn branch_switch_rejects_a_replaced_target_reference_directory() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let outside = tempfile::tempdir().expect("outside reference directory constructs");
    let target_parent = fixture.root().join(".git/refs/heads/agent");
    let retired_parent = fixture.root().join(".git/refs/heads/agent.retired");

    let failure = executor
        .branch_switch_with_reference_lock_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::rename(&target_parent, &retired_parent)
                    .expect("target reference parent retires");
                symlink(outside.path(), &target_parent)
                    .expect("replacement reference symlink constructs");
            },
        )
        .expect_err("replacement target reference directory rejects");
    fs::remove_file(&target_parent).expect("replacement reference symlink removes");
    fs::rename(&retired_parent, &target_parent).expect("target reference parent restores");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!outside.path().join("fix.lock").exists());
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_rolls_back_checkout_after_index_commit_failure() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("root-level fixture change writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let index_lock = fixture.root().join(".git/index.lock");
    let retired_lock = fixture.root().join(".git/index.lock.pinned");

    let failure = executor
        .branch_switch_with_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::rename(&index_lock, &retired_lock).expect("fixture index lock retires");
                fs::write(&index_lock, []).expect("replacement index lock writes");
            },
        )
        .expect_err("replaced index lock rejects after checkout");
    fs::remove_file(&index_lock).expect("replacement index lock removes");
    fs::remove_file(&retired_lock).expect("retired index lock removes");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
        CHANGED_CONTENT.as_bytes()
    );
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_rolls_back_after_target_reference_revalidation_fails() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
    let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");

    let failure = executor
        .branch_switch_with_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::rename(&target_reference, &retired_reference)
                    .expect("target reference retires after checkout");
                mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                    .expect("replacement target reference FIFO constructs");
            },
        )
        .expect_err("target reference revalidation rejects after checkout");
    fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
    fs::rename(&retired_reference, &target_reference).expect("target reference restores");
    let restored_repository =
        Repository::open(fixture.root()).expect("restored fixture repository opens");
    let restored_index = restored_repository
        .index()
        .expect("restored fixture index opens");
    let restored_entry = restored_index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("restored tracked entry exists");
    let restored_blob = restored_repository
        .find_blob(restored_entry.id)
        .expect("restored tracked blob opens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
        CHANGED_CONTENT.as_bytes()
    );
    assert_eq!(restored_blob.content(), CHANGED_CONTENT.as_bytes());
    assert_eq!(
        restored_repository
            .head()
            .expect("fixture HEAD remains")
            .target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_revalidates_the_root_at_head_publication() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    let retired = parent.path().join("retired");
    fs::create_dir(&root).expect("workspace root constructs");
    let original = Repository::init(&root).expect("original repository initializes");
    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
    let initial = commit_all(&original, INITIAL_MESSAGE);
    let initial_commit = original
        .find_commit(initial)
        .expect("original initial commit opens");
    original
        .branch(FIX_BRANCH, &initial_commit, false)
        .expect("fixture branch creates");
    drop(initial_commit);
    fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("original fixture change writes");
    let original_head = commit_all(&original, MODEL_MESSAGE);
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;
    let mut replacement_head = None;

    let failure = executor
        .branch_switch_with_head_publish_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned original repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::rename(&root, &retired).expect("original workspace retires");
                fs::create_dir(&root).expect("replacement workspace constructs");
                let replacement =
                    Repository::init(&root).expect("replacement repository initializes");
                fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT)
                    .expect("replacement fixture file writes");
                replacement_head.replace(commit_all(&replacement, INITIAL_MESSAGE));
            },
        )
        .expect_err("root replacement rejects at HEAD publication");
    let replacement = Repository::open(&root).expect("replacement repository opens");
    let retired_repository = Repository::open(&retired).expect("retired original repository opens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        replacement
            .head()
            .expect("replacement HEAD exists")
            .target(),
        replacement_head
    );
    assert_eq!(
        retired_repository
            .head()
            .expect("retired original HEAD exists")
            .target(),
        Some(original_head)
    );
    assert_eq!(
        fs::read(retired.join(TRACKED_PATH)).expect("retired worktree file reads"),
        INITIAL_CONTENT.as_bytes()
    );
}

#[test]
fn branch_switch_rollback_preserves_a_concurrent_worktree_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
    let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");

    let failure = executor
        .branch_switch_with_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::write(fixture.root().join(TRACKED_PATH), TARGET_CONTENT)
                    .expect("concurrent worktree edit writes");
                fs::rename(&target_reference, &retired_reference)
                    .expect("target reference retires after checkout");
                mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                    .expect("replacement target reference FIFO constructs");
            },
        )
        .expect_err("target reference failure rejects switch");
    fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
    fs::rename(&retired_reference, &target_reference).expect("target reference restores");
    let restored_index = repository.index().expect("restored index opens");
    let restored_entry = restored_index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("restored tracked entry exists");
    let restored_blob = repository
        .find_blob(restored_entry.id)
        .expect("restored tracked blob opens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("concurrent worktree edit reads"),
        TARGET_CONTENT.as_bytes()
    );
    assert_eq!(restored_blob.content(), CHANGED_CONTENT.as_bytes());
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_rolls_back_unchanged_paths_when_another_path_becomes_a_symlink() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let concurrent_path = "z-concurrent.txt";
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    fs::write(fixture.root().join(concurrent_path), CHANGED_CONTENT)
        .expect("current branch second file writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    let executor = fixture.executor();
    let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
    let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");
    let outside = tempfile::NamedTempFile::new().expect("outside file constructs");

    let failure = executor
        .branch_switch_with_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                symlink(outside.path(), fixture.root().join(concurrent_path))
                    .expect("concurrent replacement symlink constructs");
                fs::rename(&target_reference, &retired_reference)
                    .expect("target reference retires after checkout");
                mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                    .expect("replacement target reference FIFO constructs");
            },
        )
        .expect_err("concurrent symlink rejects switch publication");
    fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
    fs::rename(&retired_reference, &target_reference).expect("target reference restores");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("unchanged checkout path rolls back"),
        CHANGED_CONTENT.as_bytes()
    );
    assert!(
        fs::symlink_metadata(fixture.root().join(concurrent_path))
            .expect("concurrent symlink remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_rollback_preserves_the_original_index_extensions() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    let expected_extension = install_resolve_undo_extension(&fixture, CONFLICT_OURS_CONTENT);
    let original_head = repository.head().expect("fixture HEAD exists").target();
    let executor = fixture.executor();
    let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
    let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");

    let failure = executor
        .branch_switch_with_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::rename(&target_reference, &retired_reference)
                    .expect("target reference retires after checkout");
                mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                    .expect("replacement target reference FIFO constructs");
            },
        )
        .expect_err("target reference failure rejects switch");
    fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
    fs::rename(&retired_reference, &target_reference).expect("target reference restores");
    let observed_extension = index_extension(
        &fs::read(fixture.root().join(".git/index")).expect("restored index reads"),
        b"REUC",
    );

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(observed_extension, expected_extension);
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        original_head
    );
}

#[test]
fn branch_switch_rollback_preserves_a_concurrently_published_index() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    let original_head = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();
    let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
    let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");
    let mut competing_index = Vec::new();

    let failure = executor
        .branch_switch_with_index_publish_hook(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                let competing =
                    Repository::open(fixture.root()).expect("competing repository opens");
                let blob = competing
                    .blob(UNTRACKED_CONTENT.as_bytes())
                    .expect("competing blob writes");
                let mut index = competing.index().expect("competing index opens");
                let mut entry = clone_index_entry(
                    &index
                        .get_path(Path::new(TRACKED_PATH), 0)
                        .expect("competing tracked entry exists"),
                );
                entry.id = blob;
                entry.file_size = UNTRACKED_CONTENT.len() as u32;
                index.add(&entry).expect("competing entry stages");
                index.write().expect("competing index publishes");
                competing_index = fs::read(fixture.root().join(".git/index"))
                    .expect("competing index bytes read");
                fs::rename(&target_reference, &retired_reference)
                    .expect("target reference retires after index publication");
                mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                    .expect("replacement target reference FIFO constructs");
            },
        )
        .expect_err("target reference failure rejects switch");
    fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
    fs::rename(&retired_reference, &target_reference).expect("target reference restores");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(".git/index")).expect("published index reads"),
        competing_index
    );
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(original_head)
    );
}

#[test]
fn branch_switch_rejects_non_clean_repository_state() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let executor = fixture.executor();

    let failure = executor
        .branch_switch(
            &executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens"),
            GitBranchSwitchArguments {
                name: "conflicting".to_owned(),
            },
        )
        .expect_err("merge state rejects branch switch");
    let repository = Repository::open(fixture.root()).expect("fixture repository reopens");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(repository.state(), RepositoryState::Merge);
}

#[test]
fn checkout_error_rolls_back_a_partially_written_worktree() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("target checkout fixture writes");
    let target = commit_all(&repository, MODEL_MESSAGE);
    fs::write(fixture.root().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("current checkout fixture restores");
    let current_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit opens")
        .tree()
        .expect("fixture initial tree opens");
    let target_tree = repository
        .find_commit(target)
        .expect("fixture target commit opens")
        .tree()
        .expect("fixture target tree opens");
    let updated_paths = RefCell::new(BTreeSet::from([PathBuf::from(TRACKED_PATH)]));
    let executor = fixture.executor();

    let failure = checkout_tree_with_rollback(
        &repository,
        Some(&current_tree),
        &target_tree,
        &updated_paths,
        CheckoutRollbackContext {
            filesystem: &executor.filesystem,
            root: &executor.root,
            authority: &executor.repository_authority,
        },
        || {
            fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
                .expect("partial checkout fixture writes");
            Err(LocalGitFailure::Operation)
        },
    )
    .expect_err("partial checkout error is reported");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
        INITIAL_CONTENT.as_bytes()
    );
}

#[test]
fn checkout_error_preserves_an_edit_after_a_partial_write() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("target checkout fixture writes");
    let target = commit_all(&repository, MODEL_MESSAGE);
    let current_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit opens")
        .tree()
        .expect("fixture initial tree opens");
    let target_tree = repository
        .find_commit(target)
        .expect("fixture target commit opens")
        .tree()
        .expect("fixture target tree opens");
    let updated_paths = RefCell::new(BTreeSet::from([PathBuf::from(TRACKED_PATH)]));
    let executor = fixture.executor();

    let failure = checkout_tree_with_rollback(
        &repository,
        Some(&current_tree),
        &target_tree,
        &updated_paths,
        CheckoutRollbackContext {
            filesystem: &executor.filesystem,
            root: &executor.root,
            authority: &executor.repository_authority,
        },
        || {
            fs::write(fixture.root().join(TRACKED_PATH), TARGET_CONTENT)
                .expect("concurrent checkout fixture edit writes");
            Err(LocalGitFailure::Operation)
        },
    )
    .expect_err("partial checkout error is reported");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("concurrent fixture edit reads"),
        TARGET_CONTENT.as_bytes()
    );
}
