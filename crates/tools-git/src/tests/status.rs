//! Worktree status properties.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use git2::Repository;
use rustix::fs::{CWD, Mode, mkfifoat};

use crate::arguments::{GitDiffArguments, LocalOperation};
use crate::failure::LocalGitFailure;
use crate::limits::{MAX_INDEX_BYTES, MAX_REVISION_BYTES, MAX_STAGE_FILE_BYTES};
use crate::tests::planting::{
    plant_over_budget_directory, plant_over_budget_entries, plant_over_budget_worktree,
    plant_status_over_byte_budget,
};
use crate::tests::support::{
    CHANGED_CONTENT, EMBEDDED_REPOSITORY_PATH, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE,
    MODEL_MESSAGE, NESTED_TRACKED_DIRECTORY, NESTED_TRACKED_PATH, TRACKED_PATH, commit_all,
    execute, install_deleted_conflict, install_missing_skip_worktree_entry,
    status_uses_bound_index_without_fifo_wait,
};

#[test]
fn status_observes_real_worktree_state() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["worktree"], "modified");
}

#[test]
fn status_treats_a_missing_skip_worktree_entry_as_unchanged() {
    let fixture = Fixture::new();
    install_missing_skip_worktree_entry(&fixture);
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let entries = status["entries"]
        .as_array()
        .expect("status entries are an array");

    assert!(entries.is_empty());
}

#[test]
fn status_treats_a_tracked_child_as_deleted_when_its_parent_becomes_a_file() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("nested fixture directory constructs");
    fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
        .expect("nested tracked file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, INITIAL_MESSAGE);
    fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
        .expect("tracked parent directory removes");
    fs::write(
        fixture.root().join(NESTED_TRACKED_DIRECTORY),
        CHANGED_CONTENT,
    )
    .expect("replacement parent file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"][0]["path"], NESTED_TRACKED_DIRECTORY);
    assert_eq!(status["entries"][0]["worktree"], "untracked");
    assert_eq!(status["entries"][1]["path"], NESTED_TRACKED_PATH);
    assert_eq!(status["entries"][1]["worktree"], "deleted");
}

#[test]
fn status_rejects_worktree_over_discovery_budget() {
    let fixture = Fixture::new();
    plant_over_budget_worktree(fixture.root());
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("over-budget status discovery rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_rejects_aggregate_worktree_bytes_over_budget() {
    let fixture = Fixture::new();
    plant_status_over_byte_budget(&fixture);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("aggregate status byte budget rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_honors_disabled_filemode_tracking() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    repository
        .config()
        .expect("fixture config opens")
        .set_bool("core.filemode", false)
        .expect("fixture filemode disables");
    fs::set_permissions(
        fixture.root().join(TRACKED_PATH),
        fs::Permissions::from_mode(0o755),
    )
    .expect("fixture executable mode sets");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        0
    );
}

#[test]
fn status_reports_a_recreated_path_after_staged_deletion() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .remove_path(Path::new(TRACKED_PATH))
        .expect("fixture deletion stages");
    index.write().expect("fixture index writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
    assert_eq!(status["entries"][0]["index"], "deleted");
    assert_eq!(status["entries"][0]["worktree"], "untracked");
}

#[test]
fn status_prunes_an_untracked_embedded_repository() {
    let fixture = Fixture::new();
    let embedded = fixture.root().join(EMBEDDED_REPOSITORY_PATH);
    Repository::init(&embedded).expect("embedded repository initializes");
    plant_over_budget_entries(&embedded);
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        1
    );
    assert_eq!(status["entries"][0]["path"], EMBEDDED_REPOSITORY_PATH);
    assert_eq!(status["entries"][0]["worktree"], "untracked");
}

#[test]
fn status_and_diff_exclude_nested_git_administration_under_a_tracked_directory() {
    let fixture = Fixture::new();
    let embedded = fixture.root().join(EMBEDDED_REPOSITORY_PATH);
    fs::create_dir(&embedded).expect("tracked embedded directory constructs");
    fs::write(embedded.join(TRACKED_PATH), INITIAL_CONTENT)
        .expect("tracked embedded fixture file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, MODEL_MESSAGE);
    Repository::init(&embedded).expect("embedded repository initializes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(status["entries"], serde_json::json!([]));
    assert_eq!(diff["patch"], "");
}

#[test]
fn status_does_not_prune_a_malformed_embedded_repository_marker() {
    let fixture = Fixture::new();
    let malformed = fixture.root().join(EMBEDDED_REPOSITORY_PATH).join(".git");
    fs::create_dir_all(&malformed).expect("malformed marker directory constructs");
    plant_over_budget_entries(&malformed);
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("malformed embedded marker remains subject to discovery bounds");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_bounds_directories_even_when_an_ignore_file_names_them() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".gitignore"), "ignored/\n").expect("ignore fixture writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, MODEL_MESSAGE);
    plant_over_budget_directory(fixture.root(), "ignored");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("ignored over-budget directory still rejects safely");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn status_rejects_oversized_index_before_libgit2_parsing() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let index = fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join(".git/index"))
        .expect("fixture index opens");
    index
        .set_len((MAX_INDEX_BYTES + 1) as u64)
        .expect("oversized sparse index sets length");

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("oversized index rejects before status parsing");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn status_rejects_oversized_head_before_libgit2_parsing() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let head = fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join(".git/HEAD"))
        .expect("fixture HEAD opens");
    head.set_len((MAX_REVISION_BYTES + 1) as u64)
        .expect("oversized sparse HEAD sets length");

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("oversized HEAD rejects before revision parsing");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn status_rejects_oversized_loose_ref_before_libgit2_parsing() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let reference_path = fixture.root().join(".git/refs/heads/oversized");
    fs::write(&reference_path, []).expect("fixture loose ref writes");
    let reference = fs::OpenOptions::new()
        .write(true)
        .open(reference_path)
        .expect("fixture loose ref opens");
    reference
        .set_len((MAX_REVISION_BYTES + 1) as u64)
        .expect("oversized sparse loose ref sets length");

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("oversized loose ref rejects before revision parsing");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn status_parses_bound_index_snapshot_after_path_replacement() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let index_path = fixture.root().join(".git/index");

    let completed = status_uses_bound_index_without_fifo_wait(executor, index_path);

    assert!(completed);
}

#[test]
fn status_never_opens_a_worktree_ignore_fifo() {
    let fixture = Fixture::new();
    let ignore_path = fixture.root().join(".gitignore");
    mkfifoat(CWD, &ignore_path, Mode::RUSR | Mode::WUSR).expect("worktree ignore FIFO constructs");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(status["entries"][0]["path"], ".gitignore");
    assert_eq!(status["entries"][0]["worktree"], "untracked");
}

#[test]
fn read_only_worktree_tools_leave_no_repository_lock() {
    let fixture = Fixture::new();
    let executor = fixture.executor();

    execute(&executor, LocalOperation::Status);
    execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(!fixture.root().join(".git/index.lock").exists());
}

#[test]
fn status_never_opens_an_oversized_repository_exclude() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let exclude = fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join(".git/info/exclude"))
        .expect("repository exclude fixture opens");
    exclude
        .set_len((MAX_STAGE_FILE_BYTES + 1) as u64)
        .expect("oversized sparse repository exclude sets length");

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        0
    );
}

#[test]
fn status_ignores_a_directory_named_gitignore() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join(".gitignore"))
        .expect("gitignore-named directory constructs");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);

    assert_eq!(
        status["entries"]
            .as_array()
            .expect("entries are an array")
            .len(),
        0
    );
}

#[test]
fn status_does_not_reclassify_a_conflict_stage_as_untracked() {
    let fixture = Fixture::new();
    install_deleted_conflict(&fixture);
    let conflict_worktree_content = "conflict candidate\n";
    fs::write(fixture.root().join(TRACKED_PATH), conflict_worktree_content)
        .expect("conflict worktree file writes");
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let entry = status["entries"]
        .as_array()
        .expect("status entries are an array")
        .iter()
        .find(|entry| entry["path"] == TRACKED_PATH)
        .expect("conflicted path is reported");
    let patch = diff["patch"].as_str().expect("conflict patch is text");

    assert_eq!(entry["index"], "conflicted");
    assert_eq!(entry["worktree"], "unchanged");
    assert!(patch.contains(conflict_worktree_content));
    assert!(!patch.contains("deleted file mode"));
    assert_eq!(
        fs::read(fixture.root().join(TRACKED_PATH)).expect("conflict worktree file reads"),
        conflict_worktree_content.as_bytes()
    );
}
