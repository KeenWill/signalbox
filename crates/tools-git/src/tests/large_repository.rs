//! Content size does not prevent ordinary repository operations.

use std::fs;

use git2::Repository;
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use super::{
    push::plant_uncompressed_push_pack,
    support::{Fixture, execute, identity},
};
use crate::arguments::{GitCommitArguments, GitDiffArguments, GitStageArguments, LocalOperation};
use crate::{LocalGitTools, MAX_DIFF_BYTES, MAX_STATUS_ENTRIES};

#[test]
fn large_repository_supports_status_diff_stage_and_commit_with_bounded_results() {
    // Exercise a tracked file above 16 MiB and a real object database above 128 MiB.
    const FILE_BYTES: usize = 20 * 1024 * 1024;
    const HISTORY_BYTES: usize = 200 * 1024 * 1024;
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository opens");
    let history = repository
        .blob(&vec![b'h'; HISTORY_BYTES])
        .expect("history blob writes");
    let pack = plant_uncompressed_push_pack(&repository, &[history]);
    assert!(fs::metadata(pack).expect("pack metadata").len() >= HISTORY_BYTES as u64);
    let path = "large.txt";
    let content = "large text line\n".repeat(FILE_BYTES / "large text line\n".len() + 1);
    fs::write(fixture.root().join(path), content).expect("large worktree file writes");
    let (_, executor) =
        LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect("default unbounded suite constructs")
            .into_parts();
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![path.to_owned()],
        }),
    );
    let first = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: "Track large file".to_owned(),
        }),
    );
    assert_ne!(first["commit"], fixture.initial.to_string());
    let status = execute(&executor, LocalOperation::Status);
    assert_eq!(status["entries"], serde_json::json!([]));
    fs::write(
        fixture.root().join(path),
        "changed text line\n".repeat(FILE_BYTES / "changed text line\n".len() + 1),
    )
    .expect("large file changes");
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    assert_eq!(diff["truncated"], true);
    assert!(diff["patch"].as_str().expect("patch text").len() <= MAX_DIFF_BYTES);
    for sequence in 0..=MAX_STATUS_ENTRIES {
        fs::write(fixture.root().join(format!("untracked-{sequence}")), [])
            .expect("untracked file writes");
    }
    let status = execute(&executor, LocalOperation::Status);
    assert_eq!(status["truncated"], true);
    assert_eq!(
        status["entries"].as_array().expect("status entries").len(),
        MAX_STATUS_ENTRIES
    );
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec![path.to_owned()],
        }),
    );
    let second = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: "Update large file".to_owned(),
        }),
    );
    assert_ne!(second["commit"], first["commit"]);
}

#[test]
fn configured_object_limit_rejects_large_content() {
    let fixture = Fixture::new();
    const LIMIT: usize = 4096;
    fs::write(fixture.root().join("large.txt"), vec![b'x'; LIMIT + 1]).expect("file writes");
    let (_, executor) =
        LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect("suite constructs")
            .with_max_object_bytes(Some(LIMIT))
            .into_parts();
    assert!(
        executor
            .execute_operation(LocalOperation::Stage(GitStageArguments {
                paths: vec!["large.txt".to_owned()]
            }))
            .is_err()
    );
}
