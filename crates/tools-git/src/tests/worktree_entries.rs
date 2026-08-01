//! Symlink, gitlink, and flagged-entry reporting properties.

use std::{fs, os::unix::fs::symlink, path::Path};

use git2::Repository;

use crate::arguments::{GitDiffArguments, LocalOperation};
use crate::limits::INDEX_ASSUME_VALID;
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE, MODEL_MESSAGE, SUBMODULE_PATH,
    TRACKED_PATH, UNTRACKED_CONTENT, UNTRACKED_PATH, commit_all, commit_index, execute,
    install_gitlink, set_index_flags,
};

#[test]
fn status_and_worktree_diff_hide_an_assume_unchanged_worktree_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    set_index_flags(&repository, TRACKED_PATH, INDEX_ASSUME_VALID);
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("assume-unchanged fixture edit writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(status["entries"], serde_json::json!([]));
    assert_eq!(diff["patch"], "");
    assert_eq!(diff["truncated"], false);
}

#[test]
fn status_and_worktree_diff_preserve_an_assume_unchanged_staged_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("staged fixture edit writes");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(Path::new(TRACKED_PATH))
        .expect("fixture edit stages");
    index.write().expect("fixture index writes");
    set_index_flags(&repository, TRACKED_PATH, INDEX_ASSUME_VALID);
    fs::write(fixture.root().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("assume-unchanged worktree restores");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["index"], "modified");
    assert_eq!(status["entries"][0]["worktree"], "unchanged");
    assert!(
        diff["patch"]
            .as_str()
            .expect("fixture patch is text")
            .contains(CHANGED_CONTENT)
    );
}

#[test]
fn worktree_diff_ignores_submodule_repository_outside_root() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().expect("outside repository root constructs");
    let outside_repository =
        Repository::init(outside.path()).expect("outside repository initializes");
    fs::write(outside.path().join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("outside fixture file writes");
    let outside_commit = commit_all(&outside_repository, INITIAL_MESSAGE);
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, outside_commit);
    commit_index(&repository, "track dependency");
    fs::create_dir(fixture.root().join(SUBMODULE_PATH))
        .expect("submodule fixture directory constructs");
    fs::write(
        fixture.root().join(SUBMODULE_PATH).join(".git"),
        format!("gitdir: {}", outside.path().join(".git").display()),
    )
    .expect("submodule gitdir indirection writes");
    fs::write(outside.path().join(TRACKED_PATH), CHANGED_CONTENT)
        .expect("outside fixture change writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(diff["patch"], "");
    assert_eq!(diff["truncated"], false);
}

#[test]
fn status_and_worktree_diff_read_tracked_symlink_targets_without_following() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let link_path = "tracked-link";
    let initial_target = "first-target";
    let changed_target = "second-target";
    symlink(initial_target, fixture.root().join(link_path))
        .expect("tracked fixture symlink creates");
    commit_all(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    let clean_status = execute(&executor, LocalOperation::Status);
    let clean_diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    fs::remove_file(fixture.root().join(link_path)).expect("fixture symlink removes");
    symlink(changed_target, fixture.root().join(link_path))
        .expect("changed fixture symlink creates");
    let changed_status = execute(&executor, LocalOperation::Status);
    let changed_diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = changed_diff["patch"]
        .as_str()
        .expect("changed symlink patch is text");

    assert_eq!(clean_status["entries"], serde_json::json!([]));
    assert_eq!(clean_diff["patch"], "");
    assert_eq!(changed_status["entries"][0]["path"], link_path);
    assert_eq!(changed_status["entries"][0]["worktree"], "modified");
    assert!(patch.contains(initial_target));
    assert!(patch.contains(changed_target));
}

#[test]
fn status_reports_a_tracked_symlink_replaced_by_a_file_as_type_changed() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let link_path = "tracked-link";
    let initial_target = "first-target";
    symlink(initial_target, fixture.root().join(link_path))
        .expect("tracked fixture symlink creates");
    commit_all(&repository, MODEL_MESSAGE);
    fs::remove_file(fixture.root().join(link_path)).expect("fixture symlink removes");
    fs::write(fixture.root().join(link_path), CHANGED_CONTENT)
        .expect("replacement fixture file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"][0]["path"], link_path);
    assert_eq!(status["entries"][0]["worktree"], "type_changed");
}

#[test]
fn worktree_diff_includes_an_untracked_symlink_without_following_it() {
    let fixture = Fixture::new();
    let link_target = "untracked-target";
    symlink(link_target, fixture.root().join(UNTRACKED_PATH))
        .expect("untracked fixture symlink creates");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"]
        .as_str()
        .expect("untracked symlink patch is text");

    assert!(patch.contains(UNTRACKED_PATH));
    assert!(patch.contains(link_target));
    assert!(patch.contains("new file mode 120000"));
}

#[test]
fn status_and_worktree_diff_inspect_an_absent_staged_gitlink() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
    assert_eq!(status["entries"][0]["index"], "added");
    assert_eq!(status["entries"][0]["worktree"], "deleted");
    assert_eq!(diff["patch"], "");
}

#[test]
fn status_and_worktree_diff_report_a_missing_tracked_gitlink_as_deleted() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    commit_index(&repository, MODEL_MESSAGE);
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("fixture patch is text");

    assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
    assert_eq!(status["entries"][0]["worktree"], "deleted");
    assert!(patch.contains("deleted file mode 160000"));
    assert!(patch.contains(&format!("Subproject commit {}", fixture.initial)));
}

#[test]
fn status_and_worktree_diff_report_a_tracked_gitlink_replaced_by_a_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
    commit_index(&repository, MODEL_MESSAGE);
    fs::write(fixture.root().join(SUBMODULE_PATH), UNTRACKED_CONTENT)
        .expect("replacement fixture file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("fixture patch is text");

    assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
    assert_eq!(status["entries"][0]["worktree"], "type_changed");
    assert!(patch.contains("old mode 160000"));
    assert!(patch.contains("new mode 100644"));
    assert!(patch.contains(UNTRACKED_CONTENT));
}
