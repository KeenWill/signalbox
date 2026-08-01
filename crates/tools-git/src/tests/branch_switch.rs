//! Branch switch checkout properties.

use std::{fs, path::Path};

use git2::Repository;

use crate::arguments::{GitBranchSwitchArguments, LocalOperation};
use crate::executor::clone_index_entry;
use crate::failure::LocalGitFailure;
use crate::limits::{INDEX_ASSUME_VALID, INDEX_SKIP_WORKTREE};
use crate::tests::planting::{
    aggregate_blob_tree_commit, over_budget_tree_commit, plant_over_budget_index,
};
use crate::tests::support::{
    CHANGED_CONTENT, CRLF_CONTENT, FIX_BRANCH, Fixture, INITIAL_CONTENT, MODEL_MESSAGE,
    TARGET_CONTENT, TRACKED_PATH, UNTRACKED_CONTENT, UNTRACKED_PATH, commit_all, execute,
    raw_commit_with_tree,
};

#[test]
fn branch_switch_changes_real_head() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    let executor = fixture.executor();

    let switched = execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );

    assert_eq!(switched["branch"], FIX_BRANCH);
    assert_eq!(
        repository.head().expect("head exists").shorthand(),
        Ok(FIX_BRANCH)
    );
}

#[test]
fn branch_switch_does_not_create_a_hierarchy_for_an_absent_branch() {
    let fixture = Fixture::new();
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "missing/topic".to_owned(),
        }))
        .expect_err("absent nested branch rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!fixture.root().join(".git/refs/heads/missing").exists());
}

#[test]
fn branch_switch_preserves_a_modified_tracked_file_when_safe_checkout_refuses() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current branch fixture change writes");
    commit_all(&repository, MODEL_MESSAGE);
    fs::write(fixture.root().join(TRACKED_PATH), TARGET_CONTENT)
        .expect("local tracked modification writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("safe checkout rejects the local tracked modification");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("local modification reads"),
        TARGET_CONTENT.as_bytes()
    );
}

#[test]
fn branch_switch_preserves_an_untracked_obstruction_when_safe_checkout_refuses() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let target_blob = repository
        .blob(UNTRACKED_CONTENT.as_bytes())
        .expect("target fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert(UNTRACKED_PATH, target_blob, 0o100644)
        .expect("target fixture path inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::write(fixture.root().join(UNTRACKED_PATH), TARGET_CONTENT)
        .expect("untracked obstruction writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("safe checkout rejects the untracked obstruction");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(UNTRACKED_PATH)).expect("untracked obstruction reads"),
        TARGET_CONTENT.as_bytes()
    );
}

#[test]
fn branch_switch_resolves_symbolic_local_branch() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    repository
        .reference_symbolic(
            "refs/heads/alias",
            "refs/heads/agent/fix",
            false,
            "fixture symbolic branch",
        )
        .expect("fixture symbolic branch creates");
    let executor = fixture.executor();

    let switched = execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "alias".to_owned(),
        }),
    );

    assert_eq!(switched["branch"], "alias");
    assert_eq!(
        repository
            .find_reference("HEAD")
            .expect("HEAD exists")
            .symbolic_target(),
        Ok(Some("refs/heads/alias"))
    );
}

#[test]
fn branch_switch_checks_out_root_level_change() {
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
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );

    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("switched fixture content reads"),
        INITIAL_CONTENT.as_bytes()
    );
    let switched_repository =
        Repository::open(fixture.root()).expect("switched repository reopens");
    let index = switched_repository.index().expect("switched index opens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("switched path remains indexed");
    let blob = switched_repository
        .find_blob(entry.id)
        .expect("switched blob exists");
    assert_eq!(blob.content(), INITIAL_CONTENT.as_bytes());
}

#[test]
fn branch_switch_allows_a_clean_file_to_directory_transition() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let nested_blob = repository
        .blob(b"nested\n")
        .expect("nested fixture blob writes");
    let mut nested_builder = repository
        .treebuilder(None)
        .expect("nested fixture tree builder opens");
    nested_builder
        .insert("main.txt", nested_blob, 0o100644)
        .expect("nested fixture blob inserts");
    let nested_tree = nested_builder.write().expect("nested fixture tree writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("src", nested_tree, 0o040000)
        .expect("target fixture directory inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::write(fixture.root().join("src"), b"flat\n").expect("flat fixture file writes");
    commit_all(&repository, "flat source");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );

    assert_eq!(
        fs::read(fixture.root().join("src/main.txt")).expect("nested fixture content reads"),
        b"nested\n"
    );
}

#[test]
fn branch_switch_checks_out_exact_blob_bytes_without_attribute_filtering() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let content = repository.blob(CRLF_CONTENT).expect("CRLF blob writes");
    let attributes = repository
        .blob(b"*.txt text eol=lf\n")
        .expect("attribute blob writes");
    let mut builder = repository
        .treebuilder(None)
        .expect("target tree builder opens");
    builder
        .insert(TRACKED_PATH, content, 0o100644)
        .expect("content blob inserts");
    builder
        .insert(".gitattributes", attributes, 0o100644)
        .expect("attribute blob inserts");
    let tree = builder.write().expect("target tree writes");
    let target = raw_commit_with_tree(&repository, tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target branch creates");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );

    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("checked-out content reads"),
        CRLF_CONTENT
    );
}

#[test]
fn branch_switch_rejects_a_target_symlink_before_checkout() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let target = repository
        .blob(b"../../outside")
        .expect("symlink target blob writes");
    let mut builder = repository
        .treebuilder(None)
        .expect("target tree builder opens");
    builder
        .insert(TRACKED_PATH, target, 0o120000)
        .expect("symlink blob inserts");
    let tree = builder.write().expect("target tree writes");
    let target = raw_commit_with_tree(&repository, tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target branch creates");
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
        .expect_err("target symlink rejects before checkout");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("original content reads"),
        INITIAL_CONTENT.as_bytes()
    );
}

#[test]
fn branch_switch_rejects_an_index_over_the_entry_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    plant_over_budget_index(&repository);
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
        .expect_err("over-budget index rejects before staged-path collection");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn branch_switch_rejects_a_target_tree_over_the_checkout_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = over_budget_tree_commit(&repository, fixture.initial);
    let oversized = repository
        .find_commit(oversized)
        .expect("over-budget fixture commit exists");
    repository
        .branch(FIX_BRANCH, &oversized, false)
        .expect("over-budget fixture branch creates");
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
        .expect_err("over-budget checkout tree rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(fixture.initial)
    );
}

#[test]
fn branch_switch_rejects_target_tree_blob_bytes_over_the_aggregate_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = aggregate_blob_tree_commit(&repository, fixture.initial);
    let oversized = repository
        .find_commit(oversized)
        .expect("aggregate-tree fixture commit exists");
    repository
        .branch(FIX_BRANCH, &oversized, false)
        .expect("aggregate-tree fixture branch creates");
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
        .expect_err("aggregate target-tree bytes reject");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(fixture.initial)
    );
}

#[test]
fn branch_switch_preserves_a_nonconflicting_staged_change() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join("branch-only.txt"), "current branch\n")
        .expect("current-branch fixture file writes");
    commit_all(&repository, MODEL_MESSAGE);
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("staged fixture change writes");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );
    let switched_repository =
        Repository::open(fixture.root()).expect("switched repository reopens");
    let index = switched_repository.index().expect("switched index opens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("staged path remains indexed");
    let blob = switched_repository
        .find_blob(entry.id)
        .expect("staged blob exists");

    assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
}

#[test]
fn branch_switch_preserves_assume_valid_on_an_unchanged_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join("branch-only.txt"), CHANGED_CONTENT)
        .expect("current-branch fixture file writes");
    commit_all(&repository, MODEL_MESSAGE);
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = clone_index_entry(
        &index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("unchanged fixture entry exists"),
    );
    entry.flags |= INDEX_ASSUME_VALID;
    index
        .add(&entry)
        .expect("assume-valid fixture entry installs");
    index.write().expect("assume-valid fixture index writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );
    let switched_repository =
        Repository::open(fixture.root()).expect("switched repository reopens");
    let switched_index = switched_repository
        .index()
        .expect("switched fixture index opens");
    let switched_entry = switched_index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("unchanged switched entry exists");

    assert_eq!(
        switched_entry.flags & INDEX_ASSUME_VALID,
        entry.flags & INDEX_ASSUME_VALID
    );
}

#[test]
fn branch_switch_preserves_skip_worktree_on_a_changed_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("current-branch fixture file writes");
    commit_all(&repository, MODEL_MESSAGE);
    let mut index = repository.index().expect("fixture index opens");
    let mut entry = clone_index_entry(
        &index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("changed fixture entry exists"),
    );
    entry.flags_extended |= INDEX_SKIP_WORKTREE;
    index
        .add(&entry)
        .expect("skip-worktree fixture entry installs");
    index.write().expect("skip-worktree fixture index writes");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );
    let switched_repository =
        Repository::open(fixture.root()).expect("switched repository reopens");
    let switched_index = switched_repository
        .index()
        .expect("switched fixture index opens");
    let switched_entry = switched_index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("changed switched entry exists");

    assert_eq!(
        switched_entry.flags_extended & INDEX_SKIP_WORKTREE,
        entry.flags_extended & INDEX_SKIP_WORKTREE
    );
}

#[test]
fn branch_switch_rejects_a_staged_path_changed_by_the_target() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let target_blob = repository
        .blob(TARGET_CONTENT.as_bytes())
        .expect("target fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert(TRACKED_PATH, target_blob, 0o100644)
        .expect("target fixture blob inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("staged fixture change writes");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture change stages");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("target overlap rejects branch switch");
    let index = repository.index().expect("fixture index reopens");
    let entry = index
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("staged fixture path remains indexed");
    let blob = repository
        .find_blob(entry.id)
        .expect("staged fixture blob remains readable");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("fixture content remains readable"),
        CHANGED_CONTENT.as_bytes()
    );
    assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
    assert_eq!(
        repository.head().expect("fixture HEAD remains").target(),
        Some(fixture.initial)
    );
}
