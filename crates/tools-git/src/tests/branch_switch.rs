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
fn branch_switch_preserves_an_untracked_obstruction_in_a_target_ancestor() {
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
    let mut nested_builder = repository
        .treebuilder(None)
        .expect("nested fixture tree builder opens");
    nested_builder
        .insert("target.txt", target_blob, 0o100644)
        .expect("nested target file inserts");
    let nested_tree = nested_builder.write().expect("nested fixture tree writes");
    let mut ancestor_builder = repository
        .treebuilder(None)
        .expect("ancestor fixture tree builder opens");
    ancestor_builder
        .insert("nested", nested_tree, 0o040000)
        .expect("nested target directory inserts");
    let ancestor_tree = ancestor_builder
        .write()
        .expect("ancestor fixture tree writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("ancestor", ancestor_tree, 0o040000)
        .expect("target ancestor directory inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::write(
        fixture.root().join("ancestor"),
        UNTRACKED_CONTENT.as_bytes(),
    )
    .expect("untracked ancestor obstruction writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("untracked ancestor obstruction rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join("ancestor")).expect("untracked ancestor obstruction reads"),
        UNTRACKED_CONTENT.as_bytes()
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
fn branch_switch_replaces_a_clean_tracked_file_with_a_directory() {
    let fixture = Fixture::new();
    let nested_content = b"nested\n";
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let nested_blob = repository
        .blob(nested_content)
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
        nested_content
    );
}

#[test]
fn branch_switch_replaces_a_clean_tracked_directory_with_a_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let flat_content = b"flat\n";
    let flat_blob = repository
        .blob(flat_content)
        .expect("flat fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("src", flat_blob, 0o100644)
        .expect("target fixture file inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::create_dir(fixture.root().join("src")).expect("current fixture directory creates");
    fs::write(fixture.root().join("src/main.txt"), b"nested\n")
        .expect("current nested fixture writes");
    commit_all(&repository, "nested source");
    let executor = fixture.executor();

    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }),
    );

    assert_eq!(
        fs::read(fixture.root().join("src")).expect("switched flat fixture reads"),
        flat_content
    );
}

#[test]
fn branch_switch_preserves_an_untracked_file_inside_a_replaced_directory() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture initial commit exists")
        .tree()
        .expect("fixture initial tree opens");
    let flat_blob = repository
        .blob(b"flat\n")
        .expect("flat fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&initial_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("src", flat_blob, 0o100644)
        .expect("target fixture file inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    fs::create_dir(fixture.root().join("src")).expect("current fixture directory creates");
    fs::write(fixture.root().join("src/main.txt"), b"nested\n")
        .expect("current nested fixture writes");
    commit_all(&repository, "nested source");
    fs::write(
        fixture.root().join("src/local.txt"),
        UNTRACKED_CONTENT.as_bytes(),
    )
    .expect("untracked nested fixture writes");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("untracked nested obstruction rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join("src/local.txt")).expect("untracked nested fixture remains"),
        UNTRACKED_CONTENT.as_bytes()
    );
}

#[test]
fn failed_branch_switch_retains_every_obstructed_directory_quarantine() {
    let fixture = Fixture::new();
    let obstructed_directories = ["alpha"];
    let alpha_content = b"alpha original\n";
    let omega_content = b"omega original\n";
    let foreign_content = b"foreign alpha\n";
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::create_dir(fixture.root().join("alpha")).expect("alpha fixture directory creates");
    fs::write(fixture.root().join("alpha/main.txt"), alpha_content)
        .expect("alpha fixture file writes");
    fs::create_dir(fixture.root().join("omega")).expect("omega fixture directory creates");
    fs::write(fixture.root().join("omega/main.txt"), omega_content)
        .expect("omega fixture file writes");
    let current = commit_all(&repository, "directory source");
    let current_tree = repository
        .find_commit(current)
        .expect("current fixture commit exists")
        .tree()
        .expect("current fixture tree opens");
    let alpha_target = repository
        .blob(b"alpha target\n")
        .expect("alpha target blob writes");
    let omega_target = repository
        .blob(b"omega target\n")
        .expect("omega target blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&current_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("alpha", alpha_target, 0o100644)
        .expect("alpha target file inserts");
    target_builder
        .insert("omega", omega_target, 0o100644)
        .expect("omega target file inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, current);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    let executor = fixture.executor();
    let merge_head = format!("{current}\n");

    let failure = executor
        .branch_switch_with_quarantine_hook(
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                fs::write(fixture.root().join("alpha"), foreign_content)
                    .expect("foreign alpha obstruction writes");
                fs::write(fixture.root().join(".git/MERGE_HEAD"), merge_head)
                    .expect("concurrent merge state writes");
            },
        )
        .expect_err("concurrent operation state rejects checkout");
    fs::remove_file(fixture.root().join(".git/MERGE_HEAD"))
        .expect("concurrent merge state removes");
    let retained_quarantines = fs::read_dir(fixture.root())
        .expect("fixture root reads")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("entry/main.txt"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    let retained_alpha = retained_quarantines
        .first()
        .expect("obstructed alpha quarantine remains");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join("alpha")).expect("foreign alpha obstruction reads"),
        foreign_content
    );
    assert_eq!(
        fs::read(retained_alpha).expect("retained alpha original reads"),
        alpha_content
    );
    assert_eq!(
        retained_quarantines.len(),
        obstructed_directories.len(),
        "restored directories do not leave empty quarantines"
    );
    assert_eq!(
        fs::read(fixture.root().join("omega/main.txt")).expect("restored omega original reads"),
        omega_content
    );
}

#[test]
fn failed_branch_switch_preserves_a_foreign_entry_in_a_restored_quarantine() {
    let fixture = Fixture::new();
    let original_content = b"original directory\n";
    let foreign_content = b"foreign quarantine entry\n";
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::create_dir(fixture.root().join("src")).expect("source fixture directory creates");
    fs::write(fixture.root().join("src/main.txt"), original_content)
        .expect("source fixture file writes");
    let current = commit_all(&repository, "directory source");
    let current_tree = repository
        .find_commit(current)
        .expect("current fixture commit exists")
        .tree()
        .expect("current fixture tree opens");
    let target_blob = repository
        .blob(b"target file\n")
        .expect("target fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&current_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("src", target_blob, 0o100644)
        .expect("target fixture file inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, current);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    let executor = fixture.executor();
    let merge_head = format!("{current}\n");

    let failure = executor
        .branch_switch_with_quarantine_hook(
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                let quarantined_entry = fs::read_dir(fixture.root())
                    .expect("fixture root reads")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path().join("entry/main.txt"))
                    .find(|path| path.is_file())
                    .expect("quarantined source entry exists");
                let quarantine = quarantined_entry
                    .parent()
                    .and_then(Path::parent)
                    .expect("quarantine root exists");
                fs::write(quarantine.join("foreign.txt"), foreign_content)
                    .expect("foreign quarantine entry writes");
                fs::write(fixture.root().join(".git/MERGE_HEAD"), merge_head)
                    .expect("concurrent merge state writes");
            },
        )
        .expect_err("foreign quarantine entry prevents cleanup");
    fs::remove_file(fixture.root().join(".git/MERGE_HEAD"))
        .expect("concurrent merge state removes");
    let retained_foreign = fs::read_dir(fixture.root())
        .expect("fixture root reads")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("foreign.txt"))
        .find(|path| path.is_file())
        .expect("foreign quarantine entry remains");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(fixture.root().join("src/main.txt")).expect("restored source fixture reads"),
        original_content
    );
    assert_eq!(
        fs::read(retained_foreign).expect("foreign quarantine entry reads"),
        foreign_content
    );
}

#[test]
fn successful_branch_switch_preserves_foreign_entries_in_both_quarantines() {
    let fixture = Fixture::new();
    let original_content = b"original directory\n";
    let target_content = b"target file\n";
    let source_foreign_content = b"foreign source quarantine entry\n";
    let target_foreign_content = b"foreign target quarantine entry\n";
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::create_dir(fixture.root().join("src")).expect("source fixture directory creates");
    fs::write(fixture.root().join("src/main.txt"), original_content)
        .expect("source fixture file writes");
    let current = commit_all(&repository, "directory source");
    let current_tree = repository
        .find_commit(current)
        .expect("current fixture commit exists")
        .tree()
        .expect("current fixture tree opens");
    let target_blob = repository
        .blob(target_content)
        .expect("target fixture blob writes");
    let mut target_builder = repository
        .treebuilder(Some(&current_tree))
        .expect("target fixture tree builder opens");
    target_builder
        .insert("src", target_blob, 0o100644)
        .expect("target fixture file inserts");
    let target_tree = target_builder.write().expect("target fixture tree writes");
    let target = raw_commit_with_tree(&repository, target_tree, current);
    let target = repository
        .find_commit(target)
        .expect("target fixture commit exists");
    repository
        .branch(FIX_BRANCH, &target, false)
        .expect("target fixture branch creates");
    let executor = fixture.executor();

    executor
        .branch_switch_with_quarantine_hook(
            GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            },
            || {
                let cleanup_directories = fs::read_dir(fixture.root())
                    .expect("fixture root reads")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name().is_some_and(|name| {
                            name.to_string_lossy().starts_with(".signalbox-cleanup-")
                        })
                    })
                    .collect::<Vec<_>>();
                let source_quarantine = cleanup_directories
                    .iter()
                    .find(|path| path.join("entry/main.txt").is_file())
                    .expect("source quarantine exists");
                let target_quarantine = cleanup_directories
                    .iter()
                    .find(|path| path.join("src").is_file())
                    .expect("target quarantine exists");
                fs::write(
                    source_quarantine.join("foreign-source.txt"),
                    source_foreign_content,
                )
                .expect("foreign source quarantine entry writes");
                fs::write(
                    target_quarantine.join("foreign-target.txt"),
                    target_foreign_content,
                )
                .expect("foreign target quarantine entry writes");
            },
        )
        .expect("branch switch succeeds while preserving foreign cleanup entries");
    let retained_cleanup_directories = fs::read_dir(fixture.root())
        .expect("fixture root reads")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".signalbox-cleanup-"))
        })
        .collect::<Vec<_>>();
    let retained_source = retained_cleanup_directories
        .iter()
        .map(|path| path.join("foreign-source.txt"))
        .find(|path| path.is_file())
        .expect("foreign source quarantine entry remains");
    let retained_target = retained_cleanup_directories
        .iter()
        .map(|path| path.join("foreign-target.txt"))
        .find(|path| path.is_file())
        .expect("foreign target quarantine entry remains");

    assert_eq!(
        fs::read(fixture.root().join("src")).expect("published target fixture reads"),
        target_content
    );
    assert_eq!(
        fs::read(&retained_source).expect("foreign source quarantine entry reads"),
        source_foreign_content
    );
    assert_eq!(
        fs::read(&retained_target).expect("foreign target quarantine entry reads"),
        target_foreign_content
    );
    assert_eq!(
        fs::read(
            retained_source
                .parent()
                .expect("source quarantine root exists")
                .join("entry/main.txt")
        )
        .expect("original quarantined source reads"),
        original_content
    );
    assert_ne!(retained_source.parent(), retained_target.parent());
    assert_eq!(
        repository.head().expect("head exists").shorthand(),
        Ok(FIX_BRANCH)
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

    assert_eq!(failure, LocalGitFailure::Repository);
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
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("staged worktree content reads"),
        CHANGED_CONTENT.as_bytes()
    );
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
fn branch_switch_rejects_skip_worktree_on_a_changed_path() {
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
    let current = commit_all(&repository, MODEL_MESSAGE);
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

    let failure = executor
        .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: FIX_BRANCH.to_owned(),
        }))
        .expect_err("skip-worktree index rejects switching");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        repository.head().expect("HEAD remains").target(),
        Some(current)
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
