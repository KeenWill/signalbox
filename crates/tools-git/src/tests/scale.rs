//! Explicit generated-repository scale suite; see tooling/generate-git-scale.py.

use super::support::identity;
use crate::arguments::{
    GitBranchCreateArguments, GitBranchSwitchArguments, GitCommitArguments, GitDiffArguments,
    GitLogArguments, GitStageArguments, LocalOperation,
};
use crate::{
    ConfiguredGitRemote, GitPushArguments, GitPushReceipt, GitPushRequest, GitPushTools,
    GitPushTransport, GitPushTransportFailure, LocalGitTools,
};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;
use std::{fs, os::unix::fs::FileExt, path::PathBuf, process::Command};

#[derive(Debug)]
struct LocalBareTransport(PathBuf);
impl GitPushTransport for LocalBareTransport {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let result = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("fixture PATH"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", request.git_directory())
            .env("GIT_OBJECT_DIRECTORY", request.object_directory())
            .args([
                "-c",
                "core.bigFileThreshold=1",
                "-c",
                "core.packedGitWindowSize=1m",
                "-c",
                "core.packedGitLimit=8m",
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "--quiet",
                "--",
            ])
            .arg(&self.0)
            .arg(request.refspec())
            .output()
            .expect("local push starts");
        assert!(
            result.status.success(),
            "local push failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let remote = git2::Repository::open_bare(&self.0).expect("bare destination opens");
        assert_eq!(
            remote
                .find_reference(&format!("refs/heads/{}", request.branch()))
                .expect("remote branch exists")
                .target()
                .expect("direct remote reference")
                .to_string(),
            request.commit()
        );
        GitPushReceipt::try_new(request.commit()).map_err(|_| GitPushTransportFailure::Rejected)
    }
}

#[tokio::test]
#[ignore = "generate a repository with tooling/generate-git-scale.py and set SIGNALBOX_GIT_SCALE_ROOT"]
async fn every_git_tool_handles_the_generated_scale_repository() {
    let root = PathBuf::from(
        std::env::var_os("SIGNALBOX_GIT_SCALE_ROOT").expect("scale root is explicit"),
    );
    exercise_every_git_tool(root).await;
}

pub(super) async fn exercise_every_git_tool(root: PathBuf) {
    let remote_directory = tempfile::tempdir().expect("remote directory");
    let remote_path = remote_directory.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["clone", "--quiet", "--bare", "--shared", "--"])
            .arg(&root)
            .arg(&remote_path)
            .status()
            .expect("bare clone starts")
            .success()
    );
    let repository = git2::Repository::open(&root).expect("scale repository opens");
    let initial = repository
        .head()
        .expect("HEAD")
        .target()
        .expect("initial commit");
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("scale suite constructs")
        .into_parts();
    let status = execute(&executor, LocalOperation::Status);
    assert_eq!(status["entries"], serde_json::json!([]));
    let file = fs::OpenOptions::new()
        .write(true)
        .open(root.join("large.bin"))
        .expect("large file opens");
    file.write_all_at(b"changed", 0)
        .expect("large file changes");
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    assert_eq!(diff["truncated"], true);
    assert!(diff["patch"].as_str().expect("patch").len() <= crate::MAX_DIFF_BYTES);
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec!["large.bin".to_owned()],
        }),
    );
    let commit = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: "Scale content update".to_owned(),
        }),
    );
    let committed = commit["commit"].as_str().expect("new commit");
    execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: "HEAD".to_owned(),
            max_entries: 2,
        }),
    );
    execute(
        &executor,
        LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: "scale-tool-branch".to_owned(),
            start: "HEAD".to_owned(),
        }),
    );
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "scale-0".to_owned(),
        }),
    );
    let mut prefix = [0u8; 7];
    fs::File::open(root.join("large.bin"))
        .expect("old file opens")
        .read_exact_at(&mut prefix, 0)
        .expect("old prefix reads");
    assert_eq!(prefix, [0u8; 7]);
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "scale-tool-branch".to_owned(),
        }),
    );
    fs::File::open(root.join("large.bin"))
        .expect("new file opens")
        .read_exact_at(&mut prefix, 0)
        .expect("new prefix reads");
    assert_eq!(&prefix, b"changed");
    let revision_diff = execute(
        &executor,
        LocalOperation::Diff(GitDiffArguments::Revisions {
            base: initial.to_string(),
            head: committed.to_owned(),
        }),
    );
    assert_eq!(revision_diff["truncated"], true);
    let (_, mut push) = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        &root,
        ConfiguredGitRemote::try_new("origin", "https://example.test/fixture.git")
            .expect("configured fixture remote"),
        LocalBareTransport(remote_path),
    )
    .expect("push tools construct")
    .into_parts();
    push = push
        .with_branch_fence("scale-tool-branch".to_owned())
        .with_commit_fence(initial.to_string());
    push.execute_push(GitPushArguments::for_test("scale-tool-branch"))
        .await
        .expect("scale push succeeds");
    let native_index = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "--error-unmatch", "large.bin"])
        .output()
        .expect("native Git reads the published index");
    assert!(
        native_index.status.success(),
        "{}",
        String::from_utf8_lossy(&native_index.stderr)
    );
}

fn execute(
    executor: &crate::LocalGitExecutor<LocalWorkspaceFileSystem>,
    operation: LocalOperation,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    eprintln!("scale starts {operation:?}");
    let result = super::support::execute(executor, operation);
    eprintln!("scale operation elapsed {:?}", started.elapsed());
    result
}

#[tokio::test]
async fn every_git_tool_operates_on_a_linked_worktree() {
    let fixture = super::support::Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("fixture opens");
    // Exceed the diff prefix budget so both diff modes exercise truncation.
    let blob_bytes = crate::MAX_DIFF_BYTES * 2;
    fs::write(fixture.root().join("large.bin"), vec![0u8; blob_bytes]).expect("blob writes");
    let initial = super::support::commit_all(&repository, "linked fixture");
    repository
        .branch(
            "scale-0",
            &repository.find_commit(initial).expect("initial commit"),
            false,
        )
        .expect("scale branch");
    let parent = tempfile::tempdir().expect("linked parent");
    let linked = parent.path().join("linked");
    repository
        .worktree("linked", &linked, None)
        .expect("linked worktree creates");
    let main_head = fs::read(fixture.root().join(".git/HEAD")).expect("main HEAD");
    let main_index = fs::read(fixture.root().join(".git/index")).expect("main index");
    exercise_every_git_tool(linked).await;
    assert_eq!(
        fs::read(fixture.root().join(".git/HEAD")).expect("main HEAD remains"),
        main_head
    );
    assert_eq!(
        fs::read(fixture.root().join(".git/index")).expect("main index remains"),
        main_index
    );
    assert_eq!(
        repository.head().expect("main branch remains").target(),
        Some(initial)
    );
}
