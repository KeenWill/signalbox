//! Worktree diff properties.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use git2::Repository;
use rustix::fs::{CWD, Mode, mkfifoat};

use crate::arguments::{GitDiffArguments, LocalOperation};
use crate::failure::LocalGitFailure;
use crate::limits::MAX_DIFF_BYTES;
use crate::tests::planting::plant_over_budget_worktree;
use crate::tests::support::{
    CHANGED_CONTENT, Fixture, INITIAL_CONTENT, INITIAL_MESSAGE, ModeOnlyPathFixture, TRACKED_PATH,
    UNTRACKED_CONTENT, UNTRACKED_PATH, commit_all, commit_index, execute,
    install_missing_skip_worktree_entry, install_staged_missing_skip_worktree_entry,
};

#[test]
fn worktree_diff_observes_real_repository_state() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains("-before")
    );
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains("+after")
    );
}

#[test]
fn worktree_diff_treats_a_missing_skip_worktree_entry_as_unchanged() {
    let fixture = Fixture::new();
    install_missing_skip_worktree_entry(&fixture);
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(diff["patch"], "");
}

#[test]
fn worktree_diff_includes_a_staged_missing_skip_worktree_entry() {
    let fixture = Fixture::new();
    install_staged_missing_skip_worktree_entry(&fixture);
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(&format!("-{}", INITIAL_CONTENT.trim_end())));
    assert!(patch.contains(&format!("+{}", CHANGED_CONTENT.trim_end())));
}

#[test]
fn worktree_diff_includes_an_untracked_file() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(UNTRACKED_PATH), UNTRACKED_CONTENT)
        .expect("untracked fixture writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains(UNTRACKED_PATH)
    );
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains(&format!("+{}", UNTRACKED_CONTENT.trim_end()))
    );
}

#[test]
fn worktree_diff_emits_the_executable_mode_for_an_untracked_file() {
    let fixture = Fixture::new();
    let path = fixture.root().join(UNTRACKED_PATH);
    fs::write(&path, UNTRACKED_CONTENT).expect("untracked fixture writes");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("untracked executable mode sets");
    let expected_mode = 0o100000
        | (fs::metadata(&path)
            .expect("untracked fixture metadata reads")
            .permissions()
            .mode()
            & 0o777);
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(&format!("new file mode {expected_mode:06o}")));
}

#[test]
fn worktree_diff_emits_the_executable_mode_for_a_deleted_file() {
    let fixture = Fixture::new();
    let path = fixture.root().join(TRACKED_PATH);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("tracked executable mode sets");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    commit_all(&repository, "make tracked fixture executable");
    let expected_mode = repository
        .index()
        .expect("fixture index opens")
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("tracked fixture exists")
        .mode;
    fs::remove_file(&path).expect("tracked executable fixture removes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(&format!("deleted file mode {expected_mode:06o}")));
}

#[test]
fn worktree_diff_treats_a_tracked_file_replaced_by_a_directory_as_deleted() {
    let fixture = Fixture::new();
    let replacement = fixture.root().join(TRACKED_PATH);
    fs::remove_file(&replacement).expect("tracked fixture file removes");
    fs::create_dir(&replacement).expect("replacement directory constructs");
    fs::write(replacement.join(UNTRACKED_PATH), UNTRACKED_CONTENT)
        .expect("replacement child writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(&format!("--- a/{TRACKED_PATH}")));
    assert!(patch.contains(&format!("-{}", INITIAL_CONTENT.trim_end())));
    assert!(patch.contains(&format!("{TRACKED_PATH}/{UNTRACKED_PATH}")));
    assert!(patch.contains(&format!("+{}", UNTRACKED_CONTENT.trim_end())));
}

#[test]
fn worktree_diff_renders_a_tracked_directory_replaced_by_a_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tracked_directory = "src";
    let tracked_child = "src/main.rs";
    let tracked_content = b"fn main() {}\n";
    let replacement_content = b"replacement file\n";
    fs::create_dir(fixture.root().join(tracked_directory))
        .expect("tracked fixture directory creates");
    fs::write(fixture.root().join(tracked_child), tracked_content)
        .expect("tracked fixture child writes");
    commit_all(&repository, "add tracked directory");
    fs::remove_dir_all(fixture.root().join(tracked_directory))
        .expect("tracked fixture directory removes");
    fs::write(fixture.root().join(tracked_directory), replacement_content)
        .expect("replacement fixture file writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(&format!("--- a/{tracked_child}")));
    assert!(patch.contains(&format!(
        "-{}",
        String::from_utf8_lossy(tracked_content).trim_end()
    )));
    assert!(patch.contains(&format!("+++ b/{tracked_directory}")));
    assert!(patch.contains(&format!(
        "+{}",
        String::from_utf8_lossy(replacement_content).trim_end()
    )));
}

#[test]
fn worktree_diff_reports_an_executable_mode_only_change() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let original_mode = repository
        .index()
        .expect("fixture index opens")
        .get_path(Path::new(TRACKED_PATH), 0)
        .expect("tracked fixture exists")
        .mode;
    fs::set_permissions(
        fixture.root().join(TRACKED_PATH),
        fs::Permissions::from_mode(0o755),
    )
    .expect("fixture executable mode sets");
    let observed_mode = 0o100000
        | (fs::metadata(fixture.root().join(TRACKED_PATH))
            .expect("fixture metadata reads")
            .permissions()
            .mode()
            & 0o777);
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains(&format!("old mode {original_mode:06o}"))
    );
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains(&format!("new mode {observed_mode:06o}"))
    );
}

#[test]
fn worktree_diff_reports_a_non_utf8_mode_only_change() {
    let fixture = Fixture::new();
    let path = ModeOnlyPathFixture::non_utf8();
    fs::write(fixture.root().join(path.path()), INITIAL_CONTENT)
        .expect("non-UTF-8 fixture file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(path.path())
        .expect("non-UTF-8 fixture path stages");
    index.write().expect("fixture index writes");
    commit_index(&repository, INITIAL_MESSAGE);
    fs::set_permissions(
        fixture.root().join(path.path()),
        fs::Permissions::from_mode(0o755),
    )
    .expect("non-UTF-8 fixture executable mode sets");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert_eq!(diff["truncated"], false);
    assert!(patch.contains(path.quoted_header()));
    assert!(patch.contains("old mode 100644"));
    assert!(patch.contains("new mode 100755"));
}

#[test]
fn worktree_diff_quotes_control_bytes_in_a_mode_only_path() {
    let fixture = Fixture::new();
    let path = ModeOnlyPathFixture::control();
    fs::write(fixture.root().join(path.path()), INITIAL_CONTENT)
        .expect("control-path fixture file writes");
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let mut index = repository.index().expect("fixture index opens");
    index
        .add_path(path.path())
        .expect("control-path fixture stages");
    index.write().expect("fixture index writes");
    commit_index(&repository, INITIAL_MESSAGE);
    fs::set_permissions(
        fixture.root().join(path.path()),
        fs::Permissions::from_mode(0o755),
    )
    .expect("control-path fixture executable mode sets");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let patch = diff["patch"].as_str().expect("patch is text");

    assert!(patch.contains(path.quoted_header()));
    assert!(!patch.contains(path.unquoted_header()));
}

#[test]
fn worktree_diff_never_opens_a_worktree_ignore_fifo() {
    let fixture = Fixture::new();
    let ignore_path = fixture.root().join(".gitignore");
    mkfifoat(CWD, &ignore_path, Mode::RUSR | Mode::WUSR).expect("worktree ignore FIFO constructs");
    fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT).expect("fixture change writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains("+after")
    );
}

#[test]
fn worktree_diff_rejects_a_noncommit_head() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let head_name = repository
        .head()
        .expect("fixture HEAD exists")
        .name()
        .expect("fixture HEAD is named")
        .to_owned();
    let blob = repository
        .blob(b"not a commit\n")
        .expect("noncommit HEAD object writes");
    repository
        .reference(&head_name, blob, true, "fixture corrupts HEAD")
        .expect("fixture HEAD target replaces");
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Worktree))
        .expect_err("noncommit HEAD rejects worktree diff");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn worktree_diff_rejects_worktree_over_discovery_budget() {
    let fixture = Fixture::new();
    plant_over_budget_worktree(fixture.root());
    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Diff(GitDiffArguments::Worktree))
        .expect_err("over-budget diff discovery rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
}

#[test]
fn worktree_diff_marks_invalid_utf8_patch_incomplete() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(TRACKED_PATH), b"after-\xff\n")
        .expect("invalid UTF-8 fixture content writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(diff["truncated"], true);
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch is text")
            .contains('\u{fffd}')
    );
}

#[test]
fn worktree_diff_bounds_lossy_utf8_rendering() {
    let fixture = Fixture::new();
    let invalid = vec![0xff; MAX_DIFF_BYTES];
    fs::write(fixture.root().join(TRACKED_PATH), invalid)
        .expect("large invalid UTF-8 fixture content writes");
    let executor = fixture.executor();

    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

    assert_eq!(diff["truncated"], true);
    assert!(diff["patch"].as_str().expect("patch is text").len() <= MAX_DIFF_BYTES);
}
