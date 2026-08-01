//! Status branch, path, and rename reporting properties.

use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use git2::Repository;
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::arguments::LocalOperation;
use crate::catalog::LocalGitTools;
use crate::limits::MAX_STATUS_PATH_BYTES;
use crate::tests::support::{
    CHANGED_CONTENT, FIX_BRANCH, Fixture, RENAMED_TRACKED_PATH, TRACKED_PATH,
    TWICE_RENAMED_TRACKED_PATH, execute, identity, long_status_path,
};

#[test]
fn status_reports_unborn_symbolic_branch() {
    let root = tempfile::tempdir().expect("temporary repository root constructs");
    let repository = Repository::init(root.path()).expect("repository initializes");
    let branch = repository
        .find_reference("HEAD")
        .expect("symbolic HEAD exists")
        .symbolic_target()
        .expect("symbolic target lookup succeeds")
        .expect("HEAD has a symbolic target")
        .strip_prefix("refs/heads/")
        .expect("HEAD targets a local branch")
        .to_owned();
    let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
        .expect("local Git suite constructs")
        .into_parts()
        .1;

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["branch"], branch);
    assert!(status["head"].is_null());
}

#[test]
fn status_reports_the_symbolic_branch_selected_by_head() {
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
    repository
        .set_head("refs/heads/alias")
        .expect("fixture HEAD selects alias");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["branch"], "alias");
    assert_eq!(status["head"], fixture.initial.to_string());
}

#[test]
fn status_marks_non_utf8_symbolic_branch_identity_incomplete() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let reference_name = b"refs/heads/non-utf8-\xff";
    let reference_path = PathBuf::from(OsString::from_vec(reference_name.to_vec()));
    fs::write(
        fixture.root().join(".git").join(reference_path),
        format!("{}\n", fixture.initial),
    )
    .expect("non-UTF-8 fixture reference writes");
    repository
        .set_head_bytes(reference_name)
        .expect("non-UTF-8 fixture HEAD selects");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["branch"], "non-utf8-�");
    assert_eq!(status["branch_truncated"], true);
    assert_eq!(status["head"], fixture.initial.to_string());
}

#[test]
fn status_marks_truncated_path_output() {
    let fixture = Fixture::new();
    let path = long_status_path();
    fs::create_dir_all(
        fixture
            .root()
            .join(&path)
            .parent()
            .expect("long path has a parent"),
    )
    .expect("long fixture directory constructs");
    fs::write(fixture.root().join(&path), CHANGED_CONTENT).expect("long fixture file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["truncated"], true);
    assert_eq!(
        status["entries"][0]["path"]
            .as_str()
            .expect("status path is text")
            .len(),
        MAX_STATUS_PATH_BYTES
    );
}

#[test]
fn status_marks_non_utf8_path_output_incomplete() {
    let fixture = Fixture::new();
    let path = OsString::from_vec(b"invalid-\xff".to_vec());
    fs::write(fixture.root().join(path), CHANGED_CONTENT).expect("non-UTF-8 fixture file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"][0]["path"], "[non-utf8]");
    assert_eq!(status["truncated"], true);
}

#[test]
fn status_reports_detached_head_without_branch() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .set_head_detached(fixture.initial)
        .expect("fixture HEAD detaches");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert!(status["branch"].is_null());
    assert_eq!(status["head"], fixture.initial.to_string());
}

#[test]
fn status_detects_staged_rename() {
    let fixture = Fixture::new();
    fs::rename(
        fixture.root().join(TRACKED_PATH),
        fixture.root().join(RENAMED_TRACKED_PATH),
    )
    .expect("fixture tracked file renames");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .remove_path(Path::new(TRACKED_PATH))
        .expect("old fixture path removes from index");
    index
        .add_path(Path::new(RENAMED_TRACKED_PATH))
        .expect("new fixture path adds to index");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        1
    );
    assert_eq!(status["entries"][0]["index"], "renamed");
    assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
}

#[test]
fn status_detects_unstaged_rename() {
    let fixture = Fixture::new();
    fs::rename(
        fixture.root().join(TRACKED_PATH),
        fixture.root().join(RENAMED_TRACKED_PATH),
    )
    .expect("fixture tracked file renames");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        1
    );
    assert_eq!(status["entries"][0]["worktree"], "renamed");
    assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
}

#[test]
fn status_preserves_staged_and_worktree_rename_hops() {
    let fixture = Fixture::new();
    fs::rename(
        fixture.root().join(TRACKED_PATH),
        fixture.root().join(RENAMED_TRACKED_PATH),
    )
    .expect("fixture staged rename writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .remove_path(Path::new(TRACKED_PATH))
        .expect("old fixture path removes from index");
    index
        .add_path(Path::new(RENAMED_TRACKED_PATH))
        .expect("middle fixture path adds to index");
    index.write().expect("fixture index writes");
    fs::rename(
        fixture.root().join(RENAMED_TRACKED_PATH),
        fixture.root().join(TWICE_RENAMED_TRACKED_PATH),
    )
    .expect("fixture worktree rename writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        2
    );
    assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
    assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["index"], "renamed");
    assert_eq!(status["entries"][0]["worktree"], "deleted");
    assert_eq!(status["entries"][1]["path"], TWICE_RENAMED_TRACKED_PATH);
    assert_eq!(status["entries"][1]["previous_path"], RENAMED_TRACKED_PATH);
    assert_eq!(status["entries"][1]["index"], "unchanged");
    assert_eq!(status["entries"][1]["worktree"], "renamed");
}
