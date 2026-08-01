//! Revision diff properties.

use std::{fs, os::unix::fs::symlink};

use git2::Repository;
use rustix::fs::{CWD, Mode, mkfifoat};

use crate::arguments::{GitDiffArguments, LocalOperation};
use crate::diff::diff;
use crate::failure::LocalGitFailure;
use crate::tests::planting::{over_budget_tree_commit, oversized_root_tree_commit};
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, MODEL_MESSAGE, SUBMODULE_PATH, TRACKED_PATH, commit_all,
    commit_index, deep_full_path_tree_commit, execute, install_gitlink, plant_linear_history,
};

#[test]
fn revision_diff_uses_real_commits() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let changed = commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    let diff = execute(
        &executor,
        LocalOperation::Diff(GitDiffArguments::Revisions {
            base: fixture.initial.to_string(),
            head: changed.to_string(),
        }),
    );
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains("+after")
    );
}

#[test]
fn revision_diff_rejects_a_fifo_reference_without_blocking() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let reference_name = "refs/heads/fifo-revision";
    let reference_path = fixture.root().join(".git").join(reference_name);
    mkfifoat(CWD, &reference_path, Mode::RUSR | Mode::WUSR)
        .expect("revision fixture FIFO constructs");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");

    let failure = diff(
        &repository,
        &executor.repository_authority,
        GitDiffArguments::Revisions {
            base: fixture.initial.to_string(),
            head: reference_name.to_owned(),
        },
        &executor.filesystem,
        &executor.root,
        Vec::new(),
    )
    .expect_err("FIFO revision rejects without libgit2 reopen");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn revision_diff_admits_an_unchanged_tracked_symlink() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let link_path = "tracked-link";
    symlink("target.txt", fixture.root().join(link_path)).expect("tracked fixture symlink creates");
    let with_symlink = commit_all(&repository, "add tracked symlink");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("fixture ordinary change writes");
    let changed = commit_all(&repository, "change ordinary file");
    let executor = fixture.executor();

    let diff = execute(
        &executor,
        LocalOperation::Diff(GitDiffArguments::Revisions {
            base: with_symlink.to_string(),
            head: changed.to_string(),
        }),
    );
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(TRACKED_PATH));
    assert!(patch.contains(&format!("+{}", CHANGED_CONTENT.trim_end())));
}

#[test]
fn revision_diff_reports_a_gitlink_change() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    let base = commit_index(&repository, "base gitlink");
    let changed_target = plant_linear_history(&repository, fixture.initial, 1);
    install_gitlink(&repository, SUBMODULE_PATH, changed_target);
    let head = commit_index(&repository, "changed gitlink");
    let executor = fixture.executor();

    let diff = execute(
        &executor,
        LocalOperation::Diff(GitDiffArguments::Revisions {
            base: base.to_string(),
            head: head.to_string(),
        }),
    );
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(SUBMODULE_PATH));
    assert!(patch.contains(&format!("-Subproject commit {}", fixture.initial)));
    assert!(patch.contains(&format!("+Subproject commit {changed_target}")));
}

#[test]
fn revision_diff_rejects_tree_over_discovery_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = over_budget_tree_commit(&repository, fixture.initial);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
            base: fixture.initial.to_string(),
            head: oversized.to_string(),
        }))
        .expect_err("over-budget revision tree rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn revision_diff_rejects_tree_paths_over_materialization_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let deep = deep_full_path_tree_commit(&repository, fixture.initial);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
            base: fixture.initial.to_string(),
            head: deep.to_string(),
        }))
        .expect_err("deep full paths reject before materialization");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn revision_diff_rejects_an_oversized_root_tree_before_parsing() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let oversized = oversized_root_tree_commit(&repository, fixture.initial);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
            base: fixture.initial.to_string(),
            head: oversized.to_string(),
        }))
        .expect_err("oversized root tree rejects before parsing");

    assert_eq!(failure, LocalGitFailure::Operation);
}
